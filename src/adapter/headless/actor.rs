use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures::{FutureExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use tsclientlib::ChannelId;
use tsclientlib::{events, MessageTarget};
use tsclientlib::{Connection, DisconnectOptions, Identity, StreamItem, Version};
use tsproto_packets::packets::{AudioData, CodecType, OutCommand, OutPacket};

use super::resolve_repo_relative;
use super::tsbot::voice::v1 as voicev1;
use super::types::{emit_log, now_unix_ms};
use super::HeadlessRuntimeConfig;

struct AvatarUploadState {
    handle: tsclientlib::FiletransferHandle,
    local_path: PathBuf,
    md5_hex: String,
}

fn pick_avatar_file(dir: &std::path::Path) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    let rd = fs::read_dir(dir).ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "png" || ext == "jpg" || ext == "jpeg" || ext == "gif" {
            files.push(p);
        }
    }
    files.sort();
    files.into_iter().next()
}

fn md5_hex_of_file(path: &std::path::Path) -> Result<String> {
    let bs = fs::read(path).map_err(|e| anyhow!("read avatar file failed: {e}"))?;
    let digest = md5::compute(&bs);
    Ok(format!("{:x}", digest))
}

pub async fn ts3_actor(
    mut audio_rx: mpsc::Receiver<OutPacket>,
    mut notice_rx: mpsc::Receiver<(i32, u32, String)>,
    mut cmd_rx: mpsc::Receiver<OutCommand>,
    events_tx: broadcast::Sender<voicev1::Event>,
    shutdown_token: CancellationToken,
    config: HeadlessRuntimeConfig,
) -> Result<()> {
    let host = config.ts3_host;
    let port = config.ts3_port;
    let nickname = config.nickname;
    let server_password = config.server_password;
    let channel_password = config.channel_password;
    let channel_path = config.channel_path;
    let channel_id = config.channel_id;
    let identity_str = config.identity;
    let identity_file = resolve_repo_relative(&config.identity_file);
    let diag_enabled = false;
    let diag_interval = Duration::from_secs(5);
    let avatar_dir = config.avatar_dir.trim();
    let avatar_dir = if avatar_dir.is_empty() {
        None
    } else {
        Some(resolve_repo_relative(avatar_dir))
    };

    let address = format!("{}:{}", host, port);

    let client_version = Version::Custom {
        platform: "Windows".to_string(),
        version: "3.6.2 [Build: 1695203293]".to_string(),
        signature: vec![
            224, 23, 90, 102, 151, 96, 81, 35, 2, 184, 139, 60, 169, 201, 104, 36, 243, 113, 54,
            82, 120, 163, 180, 10, 159, 19, 2, 68, 238, 180, 153, 35, 147, 180, 150, 114, 42, 51,
            171, 24, 176, 38, 120, 1, 45, 44, 130, 99, 114, 57, 157, 74, 156, 156, 49, 180, 14, 33,
            95, 118, 43, 107, 215, 3,
        ],
    };

    let mut opts = Connection::build(address)
        .name(nickname)
        .version(client_version)
        .input_muted(false)
        .output_muted(false)
        .input_hardware_enabled(true)
        .output_hardware_enabled(true)
        .log_commands(false);

    if !server_password.is_empty() {
        opts = opts.password(server_password);
    }

    if !channel_password.is_empty() {
        opts = opts.channel_password(channel_password);
    }

    if !channel_id.is_empty() {
        if let Ok(id) = channel_id.parse::<u64>() {
            opts = opts.channel_id(tsclientlib::ChannelId(id));
        }
    } else if !channel_path.is_empty() {
        opts = opts.channel(channel_path);
    }

    if !identity_str.is_empty() {
        if let Ok(id) = Identity::new_from_str(&identity_str) {
            opts = opts.identity(id);
        }
    } else {
        let mut ident: Option<Identity> = None;

        if let Ok(s) = fs::read_to_string(&identity_file) {
            let s = s.trim();
            if !s.is_empty() {
                if let Ok(id) = serde_json::from_str::<Identity>(s) {
                    ident = Some(id);
                } else if let Ok(id) = Identity::new_from_str(s) {
                    ident = Some(id);
                }
            }
        }

        if ident.is_none() {
            if let Some(parent) = identity_file.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let id = Identity::create();
            let _ = fs::write(
                &identity_file,
                serde_json::to_string(&id).unwrap_or_default(),
            );
            ident = Some(id);
        }

        if let Some(id) = ident {
            opts = opts.identity(id);
        }
    }

    let mut out_buf: VecDeque<OutPacket> = VecDeque::with_capacity(400);
    let mut avatar_set_done = false;
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);

    'outer: loop {
        if shutdown_token.is_cancelled() {
            break;
        }

        let mut connect_handle = tokio::task::spawn_blocking({
            let o = opts.clone();
            move || -> anyhow::Result<Connection> { Ok(o.connect()?) }
        });

        let mut con = match tokio::select! {
            res = &mut connect_handle => {
                match res {
                    Ok(r) => r,
                    Err(e) => Err(anyhow!("ts3 connect join failed: {e}")),
                }
            }
            _ = shutdown_token.cancelled() => {
                connect_handle.abort();
                break 'outer;
            }
        } {
            Ok(c) => {
                backoff = Duration::from_secs(1);
                out_buf.clear();
                c
            }
            Err(e) => {
                let msg = format!("{e}");
                emit_log(&events_tx, 3, format!("ts3 connect failed: {msg}"));
                let wait = if msg.contains("ClientTooManyClonesConnected") {
                    std::cmp::max(backoff, Duration::from_secs(30))
                } else {
                    backoff
                };
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = shutdown_token.cancelled() => { break 'outer; }
                }
                backoff = std::cmp::min(backoff.saturating_mul(2), max_backoff);
                continue;
            }
        };

        let mut logged_connected = false;
        let mut last_muted_warn = Instant::now() - Duration::from_secs(60);
        let mut avatar_upload: Option<AvatarUploadState> = None;
        let mut conn_err: Option<String> = None;

        let mut send_last_tick = Instant::now();
        let mut send_jitter_max_ms: u128 = 0;
        let mut out_buf_max: usize = 0;
        let mut out_buf_drops: u64 = 0;
        let mut send_audio_errs: u64 = 0;
        let mut diag_next = Instant::now() + diag_interval;

        let mut event_tick = tokio::time::interval(std::time::Duration::from_millis(50));
        let mut send_tick = tokio::time::interval(std::time::Duration::from_millis(20));

        'inner: loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => {
                    if let Err(e) = con.disconnect(DisconnectOptions::new()) {
                        emit_log(&events_tx, 3, format!("ts3 disconnect failed: {e}"));
                    }
                    let drain = async {
                        con.events()
                            .for_each(|_| futures::future::ready(()))
                            .await;
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
                    conn_err = None;
                    break 'inner;
                }

                _ = event_tick.tick() => {
                    loop {
                        let next_item = {
                            let mut evs = con.events();
                            evs.next().now_or_never()
                        };

                        match next_item {
                            Some(Some(Ok(StreamItem::BookEvents(evts)))) => {
                                if !logged_connected {
                                    logged_connected = true;
                                    emit_log(&events_tx, 2, "ts3 connected");

                                    if !avatar_set_done {
                                        if let Some(dir) = avatar_dir.as_ref() {
                                            if dir.is_dir() {
                                                if let Some(p) = pick_avatar_file(dir) {
                                                    match fs::metadata(&p) {
                                                        Ok(md) => {
                                                            let size = md.len();
                                                            match md5_hex_of_file(&p) {
                                                                Ok(md5_hex) => {
                                                                    let remote_path = format!("/avatar_{}", md5_hex);
                                                                    match con.upload_file(ChannelId(0), &remote_path, None, size, true, false) {
                                                                        Ok(h) => {
                                                                            avatar_upload = Some(AvatarUploadState { handle: h, local_path: p.clone(), md5_hex: md5_hex.clone() });
                                                                            emit_log(&events_tx, 2, format!("avatar upload started: {} -> {}", p.display(), remote_path));
                                                                        }
                                                                        Err(e) => {
                                                                            emit_log(&events_tx, 3, format!("avatar upload start failed: {e}"));
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    emit_log(&events_tx, 3, format!("avatar md5 failed: {e}"));
                                                                }
                                                            }
                                                        }
                                                        Err(e) => {
                                                            emit_log(&events_tx, 3, format!("avatar stat failed: {e}"));
                                                        }
                                                    }
                                                } else {
                                                    emit_log(&events_tx, 3, format!("avatar dir has no supported images: {}", dir.display()));
                                                }
                                            } else {
                                                emit_log(&events_tx, 3, format!("avatar dir not found: {}", dir.display()));
                                            }
                                        }
                                    }
                                }

                                for e in evts {
                                    if let events::Event::Message { target, invoker, message } = e {
                                        let mode = match target {
                                            MessageTarget::Client(_) | MessageTarget::Poke(_) => 1,
                                            MessageTarget::Channel => 2,
                                            MessageTarget::Server => 3,
                                        };
                                        let msg_content = message.trim().to_string();
                                        let should_trigger_llm = mode == 1 && config.bot_respond_to_private
                                            || config
                                                .bot_trigger_prefixes
                                                .iter()
                                                .any(|prefix| msg_content.starts_with(prefix));
                                        let should_respond = should_trigger_llm;
                                        let (reply_target_mode, reply_target_client_id) = if mode == 1 {
                                            (1, invoker.id.0 as u32)
                                        } else {
                                            match config.bot_default_reply_mode.as_str() {
                                                "channel" => (2, 0),
                                                "server" => (3, 0),
                                                _ => (1, invoker.id.0 as u32),
                                            }
                                        };

                                        let uid = invoker
                                            .uid
                                            .as_ref()
                                            .map(|u| u.as_ref().to_string())
                                            .unwrap_or_default();

                                        let mut avatar_hash = String::new();
                                        let mut description = String::new();
                                        if !uid.is_empty() {
                                            if let Ok(st) = con.get_state() {
                                                for c in st.clients.values() {
                                                    if let Some(cuid) = c.uid.as_ref() {
                                                        if cuid.to_string() == uid {
                                                            avatar_hash = c.avatar_hash.clone();
                                                            description = c.description.clone();
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        let _ = events_tx.send(voicev1::Event {
                                            unix_ms: now_unix_ms(),
                                            payload: Some(voicev1::event::Payload::Chat(voicev1::ChatEvent {
                                                target_mode: mode,
                                                invoker_unique_id: uid,
                                                invoker_name: invoker.name,
                                                message: msg_content,
                                                invoker_avatar_hash: avatar_hash,
                                                invoker_description: description,
                                                should_trigger_llm,
                                                should_respond,
                                                reply_target_mode,
                                                reply_target_client_id,
                                            })),
                                        });
                                    }
                                }
                            }

                            Some(Some(Ok(StreamItem::FileUpload(h, r)))) => {
                                if let Some(st) = avatar_upload.as_ref() {
                                    if st.handle.0 == h.0 {
                                        let local_path = st.local_path.clone();
                                        let md5_hex = st.md5_hex.clone();

                                        match tokio::fs::File::open(&local_path).await {
                                            Ok(mut file) => {
                                                let mut stream = r.stream;
                                                if let Err(e) = tokio::io::copy(&mut file, &mut stream).await {
                                                    emit_log(&events_tx, 3, format!("upload avatar failed: {e}"));
                                                    avatar_upload = None;
                                                    continue;
                                                }

                                                use tsproto_packets::packets::{Direction, Flags, PacketType, OutCommand as OutCmd};
                                                let mut cmd = OutCmd::new(
                                                    Direction::C2S,
                                                    Flags::empty(),
                                                    PacketType::Command,
                                                    "clientupdate",
                                                );
                                                cmd.write_arg("client_flag_avatar", &md5_hex);
                                                if let Ok(client) = con.get_tsproto_client_mut() {
                                                    if let Err(e) = client.send_packet(cmd.into_packet()) {
                                                        emit_log(&events_tx, 3, format!("set avatar flag failed: {e}"));
                                                        avatar_upload = None;
                                                        continue;
                                                    }
                                                }

                                                emit_log(&events_tx, 2, format!("avatar updated: {}", md5_hex));
                                                avatar_set_done = true;
                                                avatar_upload = None;
                                            }
                                            Err(e) => {
                                                emit_log(&events_tx, 3, format!("open avatar file failed: {e}"));
                                                avatar_upload = None;
                                            }
                                        }
                                    }
                                }
                            }

                            Some(Some(Ok(StreamItem::FiletransferFailed(h, e)))) => {
                                if let Some(st) = avatar_upload.as_ref() {
                                    if st.handle.0 == h.0 {
                                        emit_log(&events_tx, 3, format!("avatar filetransfer failed: {e}"));
                                        avatar_upload = None;
                                    }
                                }
                            }

                            Some(Some(Ok(StreamItem::Audio(pkt)))) => {
                                let maybe_audio = match pkt.data().data() {
                                    AudioData::S2C { from, codec, data, .. } => Some((*from as u32, false, *codec, data.to_vec())),
                                    AudioData::S2CWhisper { from, codec, data, .. } => Some((*from as u32, true, *codec, data.to_vec())),
                                    _ => None,
                                };

                                if let Some((from_client_id, is_whisper, codec, frame)) = maybe_audio {
                                    let from_client_name = if let Ok(st) = con.get_state() {
                                        st.clients
                                            .get(&tsclientlib::ClientId(from_client_id as u16))
                                            .map(|c| c.name.clone())
                                            .unwrap_or_default()
                                    } else {
                                        String::new()
                                    };

                                    let codec = match codec {
                                        CodecType::OpusVoice => voicev1::audio_frame_event::Codec::OpusVoice as i32,
                                        CodecType::OpusMusic => voicev1::audio_frame_event::Codec::OpusMusic as i32,
                                        CodecType::CeltMono => voicev1::audio_frame_event::Codec::CeltMono as i32,
                                        CodecType::SpeexNarrowband => voicev1::audio_frame_event::Codec::SpeexNarrow as i32,
                                        CodecType::SpeexWideband => voicev1::audio_frame_event::Codec::SpeexWide as i32,
                                        CodecType::SpeexUltrawideband => voicev1::audio_frame_event::Codec::SpeexUltraWide as i32,
                                    };

                                    let _ = events_tx.send(voicev1::Event {
                                        unix_ms: now_unix_ms(),
                                        payload: Some(voicev1::event::Payload::Audio(voicev1::AudioFrameEvent {
                                            from_client_id,
                                            from_client_name,
                                            codec,
                                            is_whisper,
                                            frame,
                                        })),
                                    });
                                }
                            }

                            Some(Some(Ok(item))) => {
                                let mut s = format!("{item:?}");
                                let lower = s.to_ascii_lowercase();
                                if lower.contains("commanderror") || lower.contains(" error") || lower.contains("=error") || lower.starts_with("error") {
                                    if s.len() > 600 {
                                        s.truncate(600);
                                        s.push_str("...");
                                    }
                                    let msg = format!("ts3 stream item: {s}");
                                    emit_log(&events_tx, 3, msg.clone());
                                    warn!("{msg}");
                                }
                            }

                            Some(Some(Err(e))) => {
                                emit_log(&events_tx, 4, format!("ts3 error: {e}"));
                                conn_err = Some(format!("ts3 event error: {e}"));
                                break;
                            }
                            Some(None) => {
                                emit_log(&events_tx, 4, "ts3 disconnected");
                                conn_err = Some("ts3 disconnected".to_string());
                                break;
                            }
                            None => break,
                        }
                    }

                    if conn_err.is_some() {
                        break 'inner;
                    }
                }

                _ = send_tick.tick() => {
                    let now = Instant::now();
                    let dt = now.duration_since(send_last_tick);
                    send_last_tick = now;
                    let dt_ms = dt.as_millis();
                    if dt_ms > send_jitter_max_ms {
                        send_jitter_max_ms = dt_ms;
                    }
                    if out_buf.len() > out_buf_max {
                        out_buf_max = out_buf.len();
                    }

                    if let Some(pkt) = out_buf.pop_front() {
                        if !con.can_send_audio() {
                            if last_muted_warn.elapsed() >= Duration::from_secs(3) {
                                last_muted_warn = Instant::now();
                                emit_log(
                                    &events_tx,
                                    3,
                                    "cannot send audio (muted / insufficient talk power / away / input muted)".to_string(),
                                );
                            }
                        } else if let Err(e) = con.send_audio(pkt) {
                            send_audio_errs += 1;
                            emit_log(
                                &events_tx,
                                3,
                                format!("send_audio failed (errs={}): {e}", send_audio_errs),
                            );
                            conn_err = Some(format!("send_audio failed: {e}"));
                            break 'inner;
                        }
                    }

                    if diag_enabled && now >= diag_next {
                        diag_next = now + diag_interval;
                        let msg = format!(
                            "audio_send_diag: out_buf_max={} drops={} send_jitter_max_ms={} send_audio_errs={}",
                            out_buf_max, out_buf_drops, send_jitter_max_ms, send_audio_errs
                        );
                        emit_log(&events_tx, 1, msg.clone());
                        out_buf_max = out_buf.len();
                        send_jitter_max_ms = 0;
                        send_audio_errs = 0;
                    }
                }

                msg = notice_rx.recv() => {
                    if let Some((mode, target, text)) = msg {
                        let target_mode = if mode == 1 || mode == 2 || mode == 3 { mode } else { 2 };
                        let target = if target_mode == 1 { target } else { 0 };
                        use tsproto_packets::packets::{Direction, Flags, PacketType, OutCommand as OutCmd};
                        let mut cmd = OutCmd::new(Direction::C2S, Flags::empty(), PacketType::Command, "sendtextmessage");
                        cmd.write_arg("targetmode", &target_mode);
                        cmd.write_arg("target", &target);
                        cmd.write_arg("msg", &text);
                        if let Ok(client) = con.get_tsproto_client_mut() {
                            if let Err(e) = client.send_packet(cmd.into_packet()) {
                                conn_err = Some(format!("sendtextmessage failed: {e}"));
                                break 'inner;
                            }
                        }
                    } else {
                        break 'outer;
                    }
                }

                cmd = cmd_rx.recv() => {
                    if let Some(c) = cmd {
                        if let Ok(client) = con.get_tsproto_client_mut() {
                            if let Err(e) = client.send_packet(c.into_packet()) {
                                conn_err = Some(format!("send_packet failed: {e}"));
                                break 'inner;
                            }
                        }
                    } else {
                        break 'outer;
                    }
                }

                pkt = audio_rx.recv() => {
                    if let Some(p) = pkt {
                        if out_buf.len() >= 800 {
                            out_buf.pop_front();
                            out_buf_drops += 1;
                        }
                        out_buf.push_back(p);
                    } else {
                        break 'outer;
                    }
                }
            }
        }

        if send_audio_errs > 0 {
            emit_log(
                &events_tx,
                3,
                format!("audio_send_errs_total: {}", send_audio_errs),
            );
        }

        if let Err(e) = con.disconnect(DisconnectOptions::new()) {
            emit_log(&events_tx, 3, format!("ts3 disconnect failed: {e}"));
        }

        let drain = async {
            con.events().for_each(|_| futures::future::ready(())).await;
        };
        let _ = tokio::time::timeout(Duration::from_millis(500), drain).await;

        if shutdown_token.is_cancelled() {
            break;
        }

        let mut wait = backoff;
        if let Some(msg) = conn_err {
            if msg.contains("ClientTooManyClonesConnected") {
                wait = std::cmp::max(wait, Duration::from_secs(30));
            }
            emit_log(
                &events_tx,
                3,
                format!("ts3 connection lost: {msg}; retry in {:?}", wait),
            );
        } else {
            emit_log(
                &events_tx,
                3,
                format!("ts3 disconnected; retry in {:?}", wait),
            );
        }

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown_token.cancelled() => { break; }
        }

        backoff = std::cmp::min(backoff.saturating_mul(2), max_backoff);
    }

    Ok(())
}
