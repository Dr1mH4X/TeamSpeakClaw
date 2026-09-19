//! 技能侧共享的出站/录制句柄：headless Runtime 启动时 install。

use std::sync::{Arc, RwLock};

use crate::adapter::headless::audio_output::AudioOutput;
use crate::adapter::headless::speaker_ring::SpeakerRings;

#[derive(Clone)]
pub struct VoiceAudioRuntime {
    pub audio_output: AudioOutput,
    pub speaker_rings: Arc<SpeakerRings>,
    pub window_secs: u32,
    /// music_backend.musicbot_name；该昵称不入录制环
    pub musicbot_name: String,
}

#[derive(Clone, Default)]
pub struct VoiceAudioHandles {
    inner: Arc<RwLock<Option<VoiceAudioRuntime>>>,
}

impl VoiceAudioHandles {
    pub fn install(&self, runtime: VoiceAudioRuntime) {
        *self.inner.write().expect("voice audio handles poisoned") = Some(runtime);
    }

    pub fn get(&self) -> Option<VoiceAudioRuntime> {
        self.inner
            .read()
            .expect("voice audio handles poisoned")
            .clone()
    }
}
