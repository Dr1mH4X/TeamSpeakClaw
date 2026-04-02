pub mod headless;
pub mod napcat;
pub mod serverquery;

// 保持向后兼容的 re-export（TS 适配器）
pub use serverquery::command;
pub use serverquery::connection::TsAdapter;
pub use serverquery::event::{TextMessageEvent, TextMessageTarget, TsEvent};
