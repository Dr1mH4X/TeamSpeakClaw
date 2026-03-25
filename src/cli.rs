use crate::config::{AppConfig, DEFAULT_ACL_TOML, DEFAULT_PROMPTS_TOML, DEFAULT_SETTINGS_TOML};
use anyhow::Context;
use clap::{Parser, ValueEnum};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use std::path::Path;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// 控制台日志级别 (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// 配置管理：生成默认配置或交互式编辑
    #[arg(long, value_enum)]
    pub config: Option<ConfigAction>,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum ConfigAction {
    /// 在config/目录下生成默认配置文件
    Generate,
    /// 通过命令行向导交互式编辑配置
    Edit,
}

pub fn handle_config_action(action: ConfigAction) -> anyhow::Result<()> {
    match action {
        ConfigAction::Generate => generate_config(),
        ConfigAction::Edit => edit_config(),
    }
}

fn generate_config() -> anyhow::Result<()> {
    let config_dir = Path::new("config");
    if !config_dir.exists() {
        std::fs::create_dir_all(config_dir).context("Failed to create config directory")?;
        println!("Created directory: {}", config_dir.display());
    }

    let files = [
        ("settings.toml", DEFAULT_SETTINGS_TOML),
        ("acl.toml", DEFAULT_ACL_TOML),
        ("prompts.toml", DEFAULT_PROMPTS_TOML),
    ];

    for (filename, content) in files {
        let path = config_dir.join(filename);
        if path.exists() {
            println!(
                "Skipping {}: already exists at {}",
                filename,
                path.display()
            );
        } else {
            std::fs::write(&path, content)
                .with_context(|| format!("Failed to write {}", path.display()))?;
            println!("Created default config: {}", path.display());
        }
    }

    println!("Configuration generation complete.");
    Ok(())
}

fn edit_config() -> anyhow::Result<()> {
    // 目前我们专注于编辑 settings.toml (AppConfig)，因为它是主要的配置文件
    let config_path = Path::new("config/settings.toml");

    // 尝试加载现有配置，如果不存在则使用默认值
    let mut config = if config_path.exists() {
        println!(
            "Loading existing configuration from {}",
            config_path.display()
        );
        AppConfig::load(config_path)?
    } else {
        println!(
            "Config file not found at {}. Starting with defaults.",
            config_path.display()
        );
        AppConfig::default()
    };

    println!("\n=== TeamSpeakClaw Configuration Wizard ===");
    println!("Press Enter to accept [defaults].\n");

    // --- TeamSpeak 设置部分 ---
    if Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Edit TeamSpeak settings?")
        .default(true)
        .interact()?
    {
        config.teamspeak.host = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("TeamSpeak Server Host")
            .default(config.teamspeak.host)
            .interact_text()?;

        config.teamspeak.port = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Query Port")
            .default(config.teamspeak.port)
            .interact_text()?;

        config.teamspeak.ssh_port = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("SSH Port")
            .default(config.teamspeak.ssh_port)
            .interact_text()?;

        let methods = &["tcp", "ssh"];
        let method_index = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Connection Method")
            .items(methods)
            .default(if config.teamspeak.method == "ssh" {
                1
            } else {
                0
            })
            .interact()?;
        config.teamspeak.method = methods[method_index].to_string();

        config.teamspeak.login_name = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Query Login Name")
            .default(config.teamspeak.login_name)
            .interact_text()?;

        // 密码处理
        let change_pass = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Change Query Password?")
            .default(false)
            .interact()?;

        if change_pass {
            config.teamspeak.login_pass = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("New Query Login Password")
                .interact_text()?;
        }

        config.teamspeak.bot_nickname = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Bot Nickname")
            .default(config.teamspeak.bot_nickname)
            .interact_text()?;

        config.teamspeak.server_id = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Virtual Server ID")
            .default(config.teamspeak.server_id)
            .interact_text()?;
    }

    // --- LLM 设置部分 ---
    println!("");
    if Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Edit LLM settings?")
        .default(true)
        .interact()?
    {
        let providers = vec!["openai", "anthropic", "ollama"];
        let default_idx = providers
            .iter()
            .position(|&p| p == config.llm.provider)
            .unwrap_or(0);

        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("LLM Provider")
            .default(default_idx)
            .items(&providers)
            .interact()?;
        config.llm.provider = providers[selection].to_string();

        config.llm.base_url = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Base URL")
            .default(config.llm.base_url)
            .interact_text()?;

        config.llm.model = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Model Name")
            .default(config.llm.model)
            .interact_text()?;

        let change_key = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Change API Key?")
            .default(false)
            .interact()?;

        if change_key {
            config.llm.api_key = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("New API Key")
                .interact_text()?;
        }
    }

    // --- 保存 ---
    println!("");
    if Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(&format!("Save changes to {}?", config_path.display()))
        .default(true)
        .interact()?
    {
        // 确保目录存在
        if let Some(parent) = config_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // 使用toml序列化器
        let toml_string = toml::to_string_pretty(&config)?;
        std::fs::write(config_path, toml_string)?;
        println!("Configuration successfully saved to {:?}", config_path);
    } else {
        println!("Changes discarded.");
    }

    Ok(())
}
