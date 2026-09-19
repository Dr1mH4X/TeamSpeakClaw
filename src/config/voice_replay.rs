use serde::{Deserialize, Serialize};

/// voice_replay 配置。默认关闭；示例配置可写 true 但须注释说明与默认值差异。
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct VoiceReplayConfig {
    pub enabled: bool,
    pub window_secs: u32,
    pub direct_commands: Vec<String>,
}

impl Default for VoiceReplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_secs: 30,
            direct_commands: vec!["!replay".to_string(), "!回放".to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_replay_defaults_are_disabled_with_30s_window() {
        let cfg = VoiceReplayConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.window_secs, 30);
        assert!(cfg.direct_commands.iter().any(|c| c == "!replay"));
    }
}
