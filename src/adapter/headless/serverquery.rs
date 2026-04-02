use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::config::serverquery::SqConfig;

pub struct ServerQueryRuntimeConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub sid: u32,
    pub use_port: u16,
}

impl ServerQueryRuntimeConfig {
    pub fn from_sq_config(sq: &SqConfig, ts3_port: u16) -> Self {
        Self {
            host: sq.host.clone(),
            port: sq.port,
            user: sq.login_name.clone(),
            password: sq.login_pass.clone(),
            sid: sq.server_id,
            use_port: ts3_port,
        }
    }
}

pub fn ts3_escape_value(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => encoded.push_str("\\\\"),
            ' ' => encoded.push_str("\\s"),
            '|' => encoded.push_str("\\p"),
            '/' => encoded.push_str("\\/"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            _ => encoded.push(ch),
        }
    }
    encoded
}

pub async fn serverquery_set_client_description(
    cfg: &ServerQueryRuntimeConfig,
    nickname: &str,
    encoded_desc: &str,
) -> std::result::Result<(), String> {
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| e.to_string())?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    for _ in 0..3 {
        let mut line = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await;
    }

    let login = format!(
        "login client_login_name={} client_login_password={}",
        ts3_escape_value(&cfg.user),
        ts3_escape_value(&cfg.password)
    );
    serverquery_exec(&mut reader, &mut write_half, &login)
        .await
        .map(|_| ())?;

    let use_cmd = format!("use sid={}", cfg.sid);
    serverquery_exec(&mut reader, &mut write_half, &use_cmd)
        .await
        .map(|_| ())?;

    let find_cmd = format!("clientfind pattern={}", ts3_escape_value(nickname));
    let lines = serverquery_exec(&mut reader, &mut write_half, &find_cmd).await?;
    let clid = parse_clientfind_first_clid(&lines).ok_or_else(|| "clientfind returned no clid".to_string())?;

    let edit_cmd = format!("clientedit clid={} client_description={}", clid, encoded_desc);
    serverquery_exec(&mut reader, &mut write_half, &edit_cmd)
        .await
        .map(|_| ())?;

    let _ = serverquery_exec(&mut reader, &mut write_half, "quit").await;
    Ok(())
}

async fn serverquery_exec(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    write_half: &mut tokio::net::tcp::OwnedWriteHalf,
    cmd: &str,
) -> std::result::Result<Vec<String>, String> {
    write_half
        .write_all(cmd.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    write_half
        .write_all(b"\n")
        .await
        .map_err(|e| e.to_string())?;
    write_half.flush().await.map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .map_err(|_| "read timeout".to_string())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("server closed connection".to_string());
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("error") {
            let (id, msg) = parse_error_line(&line);
            if id == 0 {
                return Ok(out);
            }
            return Err(format!("error id={id} msg={msg}"));
        }
        out.push(line);
    }
}

fn parse_error_line(line: &str) -> (i64, String) {
    let mut id = -1i64;
    let mut msg = String::new();
    for token in line.split_whitespace() {
        if let Some((k, v)) = token.split_once('=') {
            match k {
                "id" => {
                    if let Ok(n) = v.parse::<i64>() {
                        id = n;
                    }
                }
                "msg" => {
                    msg = v.to_string();
                }
                _ => {}
            }
        }
    }
    (id, msg)
}

fn parse_clientfind_first_clid(lines: &[String]) -> Option<u64> {
    for line in lines {
        for part in line.split('|') {
            for token in part.split_whitespace() {
                if let Some((k, v)) = token.split_once('=') {
                    if k == "clid" {
                        if let Ok(n) = v.parse::<u64>() {
                            return Some(n);
                        }
                    }
                }
            }
        }
    }
    None
}
