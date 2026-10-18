use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum UiMode {
    /// 交互终端使用活动区，非交互输出回退为普通日志
    Auto,
    /// 禁用动态活动区，保留普通日志
    Plain,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// 控制台日志级别 (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// 终端界面模式
    #[arg(long, value_enum, default_value = "auto")]
    pub ui: UiMode,
}

pub fn banner(rich: bool) -> String {
    let narrow = std::env::var("COLUMNS")
        .ok()
        .and_then(|columns| columns.parse::<usize>().ok())
        .is_some_and(|columns| columns < 90);
    if !rich || narrow {
        return format!(
            "TeamSpeakClaw v{}\nTeamSpeak + NapCat · OpenAI-compatible\n{}",
            env!("CARGO_PKG_VERSION"),
            "-".repeat(60)
        );
    }
    let banner = r#"
    ░▒▓████████▓▒░▒▓███████▓▒░░▒▓██████▓▒░░▒▓█▓▒░       ░▒▓██████▓▒░░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░  ░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░  ░▒▓█▓▒░      ░▒▓█▓▒░      ░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░   ░▒▓██████▓▒░░▒▓█▓▒░      ░▒▓█▓▒░      ░▒▓████████▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░         ░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░         ░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░  ░▒▓███████▓▒░ ░▒▓██████▓▒░░▒▓████████▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█████████████▓▒░
                                                                                          "#;

    format!(
        "{}\n Version: v{}\n GitHub: https://github.com/Dr1mH4X/TeamSpeakClaw\n{:-<86}",
        banner,
        env!("CARGO_PKG_VERSION"),
        ""
    )
}
