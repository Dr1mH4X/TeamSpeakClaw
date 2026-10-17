use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchProvider {
    #[default]
    Exa,
    Parallel,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct WebSearchConfig {
    pub provider: SearchProvider,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn provider_is_opt_in_and_rejects_unknown_values() {
        let legacy: AppConfig = toml::from_str("").unwrap();
        assert_eq!(legacy.web_search.provider, SearchProvider::Exa);
        let config: AppConfig = toml::from_str("[web_search]\nprovider = 'parallel'").unwrap();
        assert_eq!(config.web_search.provider, SearchProvider::Parallel);
        assert!(toml::from_str::<AppConfig>("[web_search]\nprovider = 'unknown'").is_err());
    }

    #[test]
    fn example_configuration_keeps_exa() {
        let config: AppConfig =
            toml::from_str(include_str!("../../examples/config/settings.toml")).unwrap();
        config.validate().unwrap();
        assert_eq!(config.web_search.provider, SearchProvider::Exa);
    }
}
