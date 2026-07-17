use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(
        default = "default_ignore_unnamed_workspaces",
        alias = "ignoreUnnamedWorkspaces",
        alias = "ignore_unnamed",
        alias = "ignore_unnamed_workspaces"
    )]
    pub ignore_unnamed_workspaces: bool,
    #[serde(
        default = "default_ignore_numeric_sessions",
        alias = "ignoreNumericSessions",
        alias = "ignore_numeric_sessions"
    )]
    pub ignore_numeric_sessions: bool,
    #[serde(default = "default_theme")]
    pub theme: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ignore: Vec::new(),
            ignore_unnamed_workspaces: default_ignore_unnamed_workspaces(),
            ignore_numeric_sessions: default_ignore_numeric_sessions(),
            theme: default_theme(),
        }
    }
}

fn default_theme() -> String {
    "molokai".to_string()
}

pub fn default_ignore_unnamed_workspaces() -> bool {
    true
}

pub fn default_ignore_numeric_sessions() -> bool {
    false
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("agent-switch")
        .join("config.toml")
}

pub fn config_paths() -> Vec<PathBuf> {
    vec![config_path()]
}

pub fn load_config() -> Result<Config, String> {
    Ok(load_config_from_path(config_path().as_path())?.unwrap_or_default())
}

pub fn is_numeric_name(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
}

fn load_config_from_path(path: &Path) -> Result<Option<Config>, String> {
    match fs::read_to_string(path) {
        Ok(content) => toml::from_str::<Config>(&content)
            .map(Some)
            .map_err(|err| format!("Failed to parse {}: {}", path.display(), err)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("Failed to read {}: {}", path.display(), err)),
    }
}
