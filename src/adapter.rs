pub mod headless;
pub mod napcat;

// Re-export for backward compatibility
pub use headless::{TextMessageEvent, TextMessageTarget, TsAdapter, TsEvent};
