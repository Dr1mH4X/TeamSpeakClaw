pub mod serverquery;
pub mod unified;

pub mod headless;

pub use unified::UnifiedAdapter;

// Re-export common types used throughout the application
pub use serverquery::connection::TsAdapter;
pub use serverquery::event::{TextMessageEvent, TextMessageTarget, TsEvent};

pub use headless::HeadlessAdapter;
