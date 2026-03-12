use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowId {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub niri_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub agent: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub state: String,
    pub state_updated: f64,
    pub window: WindowId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexBinding {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub updated_at: f64,
    pub window: WindowId,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionStore {
    #[serde(default)]
    pub sessions: HashMap<String, Session>,
    #[serde(default)]
    pub codex_bindings: HashMap<String, CodexBinding>,
}

pub fn state_file() -> PathBuf {
    if let Ok(state_home) = env::var("XDG_STATE_HOME") {
        return PathBuf::from(state_home)
            .join("agent-switch")
            .join("sessions.json");
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("state")
        .join("agent-switch")
        .join("sessions.json")
}

pub fn load() -> SessionStore {
    let path = state_file();
    if let Ok(content) = fs::read_to_string(&path)
        && let Ok(store) = serde_json::from_str(&content)
    {
        return store;
    }
    SessionStore::default()
}

pub fn save(store: &SessionStore) {
    let path = state_file();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(store) {
        let _ = fs::write(path, json);
    }
}

pub fn upsert_codex_binding(store: &mut SessionStore, binding: CodexBinding) {
    let tmux_id = binding.window.tmux_id.clone();
    let niri_id = binding.window.niri_id.clone();

    store.codex_bindings.retain(|existing_id, existing| {
        if existing_id == &binding.session_id {
            return false;
        }

        if tmux_id.is_some() && existing.window.tmux_id == tmux_id {
            return false;
        }

        if niri_id.is_some() && existing.window.niri_id == niri_id {
            return false;
        }

        true
    });

    store
        .codex_bindings
        .insert(binding.session_id.clone(), binding);
}

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Find a session by session_id (for events that don't capture window)
#[allow(dead_code)]
pub fn find_by_session_id<'a>(
    store: &'a SessionStore,
    agent: &str,
    session_id: &str,
) -> Option<(&'a String, &'a Session)> {
    store
        .sessions
        .iter()
        .find(|(_, s)| s.agent == agent && s.session_id == session_id)
}

/// Find a session by session_id (mutable)
pub fn find_by_session_id_mut<'a>(
    store: &'a mut SessionStore,
    agent: &str,
    session_id: &str,
) -> Option<&'a mut Session> {
    store
        .sessions
        .values_mut()
        .find(|s| s.agent == agent && s.session_id == session_id)
}

/// Remove stale sessions (windows that no longer exist)
pub fn cleanup_stale(store: &mut SessionStore) {
    let valid_tmux = get_valid_tmux_windows();
    let valid_niri = get_valid_niri_windows();

    store
        .sessions
        .retain(|_, session| retain_window_binding(&mut session.window, &valid_tmux, &valid_niri));

    store
        .codex_bindings
        .retain(|_, binding| retain_window_binding(&mut binding.window, &valid_tmux, &valid_niri));

    // Also remove sessions older than 24h
    let cutoff = now() - 86400.0;
    store
        .sessions
        .retain(|_, session| session.state_updated > cutoff);
    store
        .codex_bindings
        .retain(|_, binding| binding.updated_at > cutoff);
}

fn get_valid_tmux_windows() -> std::collections::HashSet<String> {
    let mut valid = std::collections::HashSet::new();
    if let Ok(output) = Command::new("tmux")
        .args(["list-windows", "-a", "-F", "#{window_id}"])
        .output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let id = line.trim();
            if !id.is_empty() {
                valid.insert(id.to_string());
            }
        }
    }
    valid
}

fn get_valid_niri_windows() -> std::collections::HashSet<String> {
    let mut valid = std::collections::HashSet::new();
    if let Ok(output) = Command::new("niri").args(["msg", "-j", "windows"]).output()
        && output.status.success()
        && let Ok(windows) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
    {
        for window in windows {
            if let Some(id) = window.get("id").and_then(|v| v.as_u64()) {
                valid.insert(id.to_string());
            }
        }
    }
    valid
}

fn retain_window_binding(
    window: &mut WindowId,
    valid_tmux: &HashSet<String>,
    valid_niri: &HashSet<String>,
) -> bool {
    let keep_tmux = window
        .tmux_id
        .as_ref()
        .is_some_and(|id| valid_tmux.contains(id));
    let keep_niri = window
        .niri_id
        .as_ref()
        .is_some_and(|id| valid_niri.contains(id));

    if window.tmux_id.is_some() && !keep_tmux {
        window.tmux_id = None;
    }
    if window.niri_id.is_some() && !keep_niri {
        window.niri_id = None;
    }

    window.tmux_id.is_some() || window.niri_id.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_binding_replaces_existing_binding_for_same_window() {
        let mut store = SessionStore::default();

        upsert_codex_binding(
            &mut store,
            CodexBinding {
                session_id: "first".to_string(),
                cwd: Some("/tmp/project".to_string()),
                updated_at: 1.0,
                window: WindowId {
                    niri_id: Some("42".to_string()),
                    tmux_id: None,
                },
            },
        );

        upsert_codex_binding(
            &mut store,
            CodexBinding {
                session_id: "second".to_string(),
                cwd: Some("/tmp/project".to_string()),
                updated_at: 2.0,
                window: WindowId {
                    niri_id: Some("42".to_string()),
                    tmux_id: None,
                },
            },
        );

        assert!(!store.codex_bindings.contains_key("first"));
        assert_eq!(
            store
                .codex_bindings
                .get("second")
                .and_then(|binding| binding.window.niri_id.as_deref()),
            Some("42")
        );
    }

    #[test]
    fn retain_window_binding_keeps_tmux_binding_when_niri_window_is_gone() {
        let mut window = WindowId {
            niri_id: Some("42".to_string()),
            tmux_id: Some("@7".to_string()),
        };
        let valid_tmux = HashSet::from(["@7".to_string()]);
        let valid_niri = HashSet::new();

        assert!(retain_window_binding(&mut window, &valid_tmux, &valid_niri));
        assert_eq!(window.tmux_id.as_deref(), Some("@7"));
        assert_eq!(window.niri_id, None);
    }
}
