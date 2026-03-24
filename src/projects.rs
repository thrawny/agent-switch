use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    #[allow(dead_code)]
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_project_dir")]
    pub dir: String,
    #[serde(default)]
    #[cfg_attr(not(feature = "niri"), allow(dead_code))]
    pub static_workspace: bool,
    #[serde(default = "default_true", alias = "skip_first_column")]
    #[cfg_attr(not(feature = "niri"), allow(dead_code))]
    pub skip_first_column: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub project: Vec<Project>,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(
        default = "default_ignore_unnamed_workspaces",
        alias = "ignoreUnnamedWorkspaces",
        alias = "ignore_unnamed",
        alias = "ignore_unnamed_workspaces"
    )]
    #[cfg_attr(not(feature = "niri"), allow(dead_code))]
    pub ignore_unnamed_workspaces: bool,
    #[serde(
        default = "default_ignore_numeric_sessions",
        alias = "ignoreNumericSessions",
        alias = "ignore_numeric_sessions"
    )]
    pub ignore_numeric_sessions: bool,
    #[serde(default = "default_theme")]
    #[cfg_attr(not(feature = "niri"), allow(dead_code))]
    pub theme: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            project: Vec::new(),
            ignore: Vec::new(),
            ignore_unnamed_workspaces: default_ignore_unnamed_workspaces(),
            ignore_numeric_sessions: default_ignore_numeric_sessions(),
            theme: default_theme(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_project_dir() -> String {
    "~/".to_string()
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

pub fn legacy_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("projects.toml")
}

#[cfg_attr(not(feature = "niri"), allow(dead_code))]
pub fn config_paths() -> Vec<PathBuf> {
    let primary = config_path();
    let legacy = legacy_config_path();
    if legacy == primary {
        vec![primary]
    } else {
        vec![primary, legacy]
    }
}

pub fn load_config() -> Result<Config, String> {
    let primary = config_path();
    match load_config_from_path(primary.as_path())? {
        Some(config) => Ok(config),
        None => {
            let legacy = legacy_config_path();
            if legacy == primary {
                Ok(Config::default())
            } else {
                Ok(load_config_from_path(legacy.as_path())?.unwrap_or_default())
            }
        }
    }
}

#[cfg(test)]
pub fn parse_config_or_default(content: &str) -> Config {
    toml::from_str(content).unwrap_or_default()
}

pub fn configured_projects(config: &Config) -> Vec<&Project> {
    let mut seen = std::collections::HashSet::new();
    let mut projects = Vec::new();

    for project in &config.project {
        let name = project_workspace_name(project);
        if seen.insert(name) {
            projects.push(project);
        }
    }

    projects
}

pub fn configured_project_names(config: &Config) -> Vec<String> {
    configured_projects(config)
        .into_iter()
        .map(project_workspace_name)
        .collect()
}

pub fn project_workspace_name(project: &Project) -> String {
    if let Some(name) = project.name.as_deref().map(str::trim)
        && !name.is_empty()
    {
        return name.to_string();
    }

    expanded_project_dir(project.dir.as_str())
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| project.dir.clone())
}

pub fn is_numeric_name(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
}

pub fn should_ignore_name(name: &str, config: &Config) -> bool {
    config.ignore.iter().any(|ignored| ignored == name)
        || (config.ignore_numeric_sessions && is_numeric_name(name))
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

fn expanded_project_dir(dir: &str) -> PathBuf {
    if dir == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(dir));
    }
    if let Some(rest) = dir.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_name_is_inferred_from_dir_when_missing() {
        let project = Project {
            key: None,
            name: None,
            dir: "~/code/agent-switch".to_string(),
            static_workspace: false,
            skip_first_column: true,
        };

        assert_eq!(project_workspace_name(&project), "agent-switch");
    }

    #[test]
    fn configured_projects_are_deduplicated_in_order() {
        let config: Config = toml::from_str(
            r#"
[[project]]
dir = "~/code/agent-switch"

[[project]]
name = "main"

[[project]]
dir = "~/code/agent-switch"

[[project]]
name = "main"

[[project]]
dir = "~/code/wayvoice"
"#,
        )
        .expect("config should parse");

        assert_eq!(
            configured_project_names(&config),
            vec!["agent-switch", "main", "wayvoice"]
        );
    }
}
