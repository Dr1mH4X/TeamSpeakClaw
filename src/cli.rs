use crate::config::{AppConfig, DEFAULT_ACL_TOML, DEFAULT_PROMPTS_TOML, DEFAULT_SETTINGS_TOML};
use anyhow::Context;
use clap::{Parser, ValueEnum};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use std::path::Path;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Console log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Connection mode: serverquery or headless
    #[cfg(feature = "headless")]
    #[arg(long, value_enum)]
    pub mode: Option<ConnectionMode>,

    /// Configuration management: generate defaults or edit interactively
    #[arg(long, value_enum)]
    pub config: Option<ConfigAction>,

    /// List registered skills and exit
    #[arg(long)]
    pub list_skills: bool,
}

#[cfg(feature = "headless")]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum ConnectionMode {
    /// Use ServerQuery protocol (default)
    Serverquery,
    /// Use headless client (direct UDP connection)
    Headless,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum ConfigAction {
    /// Generate default configuration files in config/ directory
    Generate,
    /// Edit configuration interactively via command line wizard
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
    // We will focus on editing settings.toml (AppConfig) for now as it's the main one.
    let config_path = Path::new("config/settings.toml");

    // Attempt to load existing config, or default if missing
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

    // --- TeamSpeak Section ---
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

        config.teamspeak.use_ssh = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Use SSH?")
            .default(config.teamspeak.use_ssh)
            .interact()?;

        config.teamspeak.login_name = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Query Login Name")
            .default(config.teamspeak.login_name)
            .interact_text()?;

        // Password handling
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

        #[cfg(feature = "headless")]
        {
            let modes = vec!["serverquery", "headless"];
            let default_idx = modes
                .iter()
                .position(|&m| m == config.teamspeak.connection_mode)
                .unwrap_or(0);
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Connection Mode")
                .default(default_idx)
                .items(&modes)
                .interact()?;
            config.teamspeak.connection_mode = modes[selection].to_string();

            if config.teamspeak.connection_mode == "headless" {
                println!("\n--- Headless Client Settings ---");

                config.teamspeak.headless.server_address =
                    Input::with_theme(&ColorfulTheme::default())
                        .with_prompt("Voice Server Address (host:voice_port)")
                        .default(config.teamspeak.headless.server_address)
                        .interact_text()?;

                config.teamspeak.headless.identity_path =
                    Input::with_theme(&ColorfulTheme::default())
                        .with_prompt("Identity Key File Path")
                        .default(config.teamspeak.headless.identity_path)
                        .interact_text()?;

                config.teamspeak.headless.connect_timeout_secs =
                    Input::with_theme(&ColorfulTheme::default())
                        .with_prompt("Connect Timeout (seconds)")
                        .default(config.teamspeak.headless.connect_timeout_secs)
                        .interact_text()?;
            }
        }
    }

    // --- LLM Section ---
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

    // --- Save ---
    println!("");
    if Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(&format!("Save changes to {}?", config_path.display()))
        .default(true)
        .interact()?
    {
        // Ensure directory exists
        if let Some(parent) = config_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // Use toml serializer
        let toml_string = toml::to_string_pretty(&config)?;
        std::fs::write(config_path, toml_string)?;
        println!("Configuration successfully saved to {:?}", config_path);
    } else {
        println!("Changes discarded.");
    }

    Ok(())
}
