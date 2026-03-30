use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// 控制台日志级别 (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    pub log_level: String,
}
