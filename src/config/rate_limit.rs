use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
    pub burst_size: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 10,
            burst_size: 3,
        }
    }
}

impl RateLimitConfig {
    pub fn to_toml(&self) -> String {
        let mut output = String::new();
        output.push_str("[rate_limit]\n");
        output.push_str(&format!(
            "requests_per_minute = {}\n",
            self.requests_per_minute
        ));
        output.push_str(&format!("burst_size = {}\n", self.burst_size));
        output
    }
}
