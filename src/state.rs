use crate::projects;
use log::warn;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const STALE_SESSION_MAX_AGE_SECS: f64 = 24.0 * 60.0 * 60.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowId {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub niri_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Waiting,
    Responding,
    Idle,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WaitingReason {
    PermissionPrompt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub agent: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub state: SessionState,
    pub state_updated: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<WaitingReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexSession {
    pub session_id: String,
    pub cwd: String,
    pub state: SessionState,
    pub state_updated: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStore {
    #[serde(default)]
    pub sessions: HashMap<String, Session>,
    #[serde(default)]
    pub codex_bindings: HashMap<String, CodexBinding>,
    #[serde(default)]
    pub codex_sessions: HashMap<String, CodexSession>,
}

pub type Result<T> = std::result::Result<T, StateError>;

#[derive(Debug)]
pub enum StateError {
    Io {
        op: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    Serialize(serde_json::Error),
}

#[derive(Debug)]
struct WindowProbeError {
    backend: &'static str,
    detail: String,
}

#[derive(Debug, Clone, Default)]
struct TmuxWindowInfo {
    title: String,
    command: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct NiriWindowInfo {
    title: Option<String>,
}

impl WindowProbeError {
    fn command_error(backend: &'static str, source: io::Error) -> Self {
        Self {
            backend,
            detail: source.to_string(),
        }
    }

    fn command_failed(backend: &'static str, output: &std::process::Output) -> Self {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("command exited with status {}", output.status)
        } else {
            format!("command exited with status {}: {}", output.status, stderr)
        };
        Self { backend, detail }
    }

    fn parse_error(backend: &'static str, source: serde_json::Error) -> Self {
        Self {
            backend,
            detail: format!("failed to parse backend output: {}", source),
        }
    }
}

impl fmt::Display for WindowProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} probe failed: {}", self.backend, self.detail)
    }
}

impl StateError {
    fn io(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            op,
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, path, source } => {
                write!(f, "{} {}: {}", op, path.display(), source)
            }
            Self::Parse { path, source } => {
                write!(f, "parse state file {}: {}", path.display(), source)
            }
            Self::Serialize(source) => write!(f, "serialize state store: {}", source),
        }
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Serialize(source) => Some(source),
        }
    }
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

struct StateLock {
    file: fs::File,
}

impl StateLock {
    fn acquire(state_path: &Path) -> Result<Self> {
        let lock_path = lock_file_path(state_path);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| StateError::io("create state directory", parent, err))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| StateError::io("open state lock", &lock_path, err))?;

        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if status != 0 {
            return Err(StateError::io(
                "lock state file",
                &lock_path,
                io::Error::last_os_error(),
            ));
        }

        Ok(Self { file })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn with_locked_store<T, F>(mutate: F) -> Result<T>
where
    F: FnOnce(&mut SessionStore) -> Result<T>,
{
    with_locked_store_at_path(&state_file(), mutate)
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
    let previously_bound_codex_sessions: HashSet<String> =
        store.codex_bindings.keys().cloned().collect();
    let codex_aliases = load_codex_aliases();
    let live_tmux_windows = store_uses_tmux_windows(store).then(get_live_tmux_windows);
    let live_niri_windows = store_uses_niri_windows(store).then(get_live_niri_windows);

    if let Some(Err(err)) = &live_tmux_windows {
        warn!("Skipping tmux stale cleanup: {}", err);
    }
    if let Some(Err(err)) = &live_niri_windows {
        warn!("Skipping niri stale cleanup: {}", err);
    }

    cleanup_stale_with_window_snapshots(
        store,
        live_tmux_windows
            .as_ref()
            .and_then(|result| result.as_ref().ok()),
        live_niri_windows
            .as_ref()
            .and_then(|result| result.as_ref().ok()),
        &codex_aliases,
    );

    let cutoff = now() - STALE_SESSION_MAX_AGE_SECS;
    store
        .sessions
        .retain(|_, session| session.state_updated > cutoff);
    store
        .codex_bindings
        .retain(|_, binding| binding.updated_at > cutoff);
    cleanup_unbound_codex_sessions(store, &previously_bound_codex_sessions, cutoff);
}

fn store_uses_tmux_windows(store: &SessionStore) -> bool {
    store
        .sessions
        .values()
        .any(|session| session.window.tmux_id.is_some())
        || store
            .codex_bindings
            .values()
            .any(|binding| binding.window.tmux_id.is_some())
}

fn store_uses_niri_windows(store: &SessionStore) -> bool {
    store
        .sessions
        .values()
        .any(|session| session.window.niri_id.is_some())
        || store
            .codex_bindings
            .values()
            .any(|binding| binding.window.niri_id.is_some())
}

fn cleanup_stale_with_window_snapshots(
    store: &mut SessionStore,
    live_tmux_windows: Option<&HashMap<String, TmuxWindowInfo>>,
    live_niri_windows: Option<&HashMap<String, NiriWindowInfo>>,
    codex_aliases: &[String],
) {
    store.sessions.retain(|_, session| {
        retain_window_binding(&mut session.window, &live_tmux_windows, &live_niri_windows)
    });

    store.codex_bindings.retain(|_, binding| {
        retain_window_binding(&mut binding.window, &live_tmux_windows, &live_niri_windows)
    });

    store.codex_bindings.retain(|_, binding| {
        codex_binding_matches_live_window(
            binding,
            live_tmux_windows,
            live_niri_windows,
            codex_aliases,
        )
    });
}

fn cleanup_unbound_codex_sessions(
    store: &mut SessionStore,
    previously_bound_session_ids: &HashSet<String>,
    cutoff: f64,
) {
    let still_bound_session_ids: HashSet<String> = store.codex_bindings.keys().cloned().collect();
    store.codex_sessions.retain(|session_id, session| {
        if still_bound_session_ids.contains(session_id) {
            return true;
        }
        if previously_bound_session_ids.contains(session_id) {
            return false;
        }
        session.state_updated > cutoff
    });
}

fn get_live_tmux_windows() -> std::result::Result<HashMap<String, TmuxWindowInfo>, WindowProbeError>
{
    let mut windows = HashMap::new();
    let output = Command::new("tmux")
        .args([
            "list-windows",
            "-a",
            "-F",
            "#{window_id}\t#{window_name}\t#{pane_current_command}",
        ])
        .output()
        .map_err(|err| WindowProbeError::command_error("tmux", err))?;
    if !output.status.success() {
        return Err(WindowProbeError::command_failed("tmux", &output));
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(3, '\t');
        let id = parts.next().unwrap_or("").trim();
        if !id.is_empty() {
            let title = parts.next().unwrap_or("").trim().to_string();
            let command = parts
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            windows.insert(id.to_string(), TmuxWindowInfo { title, command });
        }
    }

    Ok(windows)
}

fn get_live_niri_windows() -> std::result::Result<HashMap<String, NiriWindowInfo>, WindowProbeError>
{
    let mut windows = HashMap::new();
    let output = Command::new("niri")
        .args(["msg", "-j", "windows"])
        .output()
        .map_err(|err| WindowProbeError::command_error("niri", err))?;
    if !output.status.success() {
        return Err(WindowProbeError::command_failed("niri", &output));
    }

    let parsed_windows = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
        .map_err(|err| WindowProbeError::parse_error("niri", err))?;
    for window in parsed_windows {
        if let Some(id) = window.get("id").and_then(|v| v.as_u64()) {
            let title = window
                .get("title")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            windows.insert(id.to_string(), NiriWindowInfo { title });
        }
    }

    Ok(windows)
}

fn retain_window_binding(
    window: &mut WindowId,
    live_tmux_windows: &Option<&HashMap<String, TmuxWindowInfo>>,
    live_niri_windows: &Option<&HashMap<String, NiriWindowInfo>>,
) -> bool {
    let drop_tmux = matches!(
        (window.tmux_id.as_ref(), live_tmux_windows),
        (Some(id), Some(valid)) if !valid.contains_key(id)
    );
    let drop_niri = matches!(
        (window.niri_id.as_ref(), live_niri_windows),
        (Some(id), Some(valid)) if !valid.contains_key(id)
    );

    if drop_tmux {
        window.tmux_id = None;
    }
    if drop_niri {
        window.niri_id = None;
    }

    window.tmux_id.is_some() || window.niri_id.is_some()
}

fn load_codex_aliases() -> Vec<String> {
    match projects::load_config() {
        Ok(config) => projects::normalized_codex_aliases(&config.codex_aliases),
        Err(err) => {
            warn!("Failed to load config for Codex aliases: {}", err);
            projects::normalized_codex_aliases(&[])
        }
    }
}

fn codex_binding_matches_live_window(
    binding: &CodexBinding,
    live_tmux_windows: Option<&HashMap<String, TmuxWindowInfo>>,
    live_niri_windows: Option<&HashMap<String, NiriWindowInfo>>,
    codex_aliases: &[String],
) -> bool {
    let tmux_checked = binding.window.tmux_id.is_some() && live_tmux_windows.is_some();
    let tmux_matches = binding
        .window
        .tmux_id
        .as_ref()
        .and_then(|id| live_tmux_windows.and_then(|windows| windows.get(id)))
        .is_some_and(|window| tmux_window_matches_codex_aliases(window, codex_aliases));

    let niri_checked = binding.window.niri_id.is_some() && live_niri_windows.is_some();
    let niri_matches = binding
        .window
        .niri_id
        .as_ref()
        .and_then(|id| live_niri_windows.and_then(|windows| windows.get(id)))
        .is_some_and(|window| niri_window_matches_codex_aliases(window, codex_aliases));

    let checked_any = tmux_checked || niri_checked;
    !checked_any || tmux_matches || niri_matches
}

fn tmux_window_matches_codex_aliases(window: &TmuxWindowInfo, codex_aliases: &[String]) -> bool {
    projects::contains_alias_token(&window.title, codex_aliases)
        || window
            .command
            .as_deref()
            .is_some_and(|command| projects::contains_alias_token(command, codex_aliases))
}

fn niri_window_matches_codex_aliases(window: &NiriWindowInfo, codex_aliases: &[String]) -> bool {
    window
        .title
        .as_deref()
        .is_some_and(|title| projects::contains_alias_token(title, codex_aliases))
}

pub(crate) fn with_locked_store_at_path<T, F>(path: &Path, mutate: F) -> Result<T>
where
    F: FnOnce(&mut SessionStore) -> Result<T>,
{
    let _lock = StateLock::acquire(path)?;
    let mut store = load_from_path(path)?;
    let output = mutate(&mut store)?;
    save_to_path(path, &store)?;
    Ok(output)
}

pub(crate) fn load_from_path(path: &Path) -> Result<SessionStore> {
    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|source| StateError::Parse {
            path: path.to_path_buf(),
            source,
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(SessionStore::default()),
        Err(err) => Err(StateError::io("read state file", path, err)),
    }
}

fn save_to_path(path: &Path, store: &SessionStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| StateError::io("create state directory", parent, err))?;
    }

    let json = serde_json::to_vec_pretty(store).map_err(StateError::Serialize)?;
    let temp_path = temp_file_path(path);

    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|err| StateError::io("create temp state file", &temp_path, err))?;
        file.write_all(&json)
            .map_err(|err| StateError::io("write temp state file", &temp_path, err))?;
        file.sync_all()
            .map_err(|err| StateError::io("sync temp state file", &temp_path, err))?;
        fs::rename(&temp_path, path)
            .map_err(|err| StateError::io("rename temp state file", path, err))?;
        sync_parent_dir(path)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    write_result
}

fn lock_file_path(state_path: &Path) -> PathBuf {
    path_with_suffix(state_path, ".lock")
}

fn temp_file_path(state_path: &Path) -> PathBuf {
    let suffix = format!(
        ".tmp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    path_with_suffix(state_path, &suffix)
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|value| value.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let dir = fs::File::open(parent)
        .map_err(|err| StateError::io("open state directory", parent, err))?;
    dir.sync_all()
        .map_err(|err| StateError::io("sync state directory", parent, err))
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_state_path(test_name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("agent-switch-tests")
            .join(format!(
                "{}-{}-{}",
                test_name,
                std::process::id(),
                TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&dir).expect("test state dir should be created");
        dir.join("sessions.json")
    }

    fn codex_aliases() -> Vec<String> {
        projects::normalized_codex_aliases(&["cx".to_string(), "cxy".to_string()])
    }

    fn tmux_windows(ids: &[&str]) -> HashMap<String, TmuxWindowInfo> {
        ids.iter()
            .map(|id| (id.to_string(), TmuxWindowInfo::default()))
            .collect()
    }

    fn niri_windows(ids: &[&str]) -> HashMap<String, NiriWindowInfo> {
        ids.iter()
            .map(|id| (id.to_string(), NiriWindowInfo::default()))
            .collect()
    }

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
        let valid_tmux = tmux_windows(&["@7"]);
        let valid_niri = niri_windows(&[]);

        assert!(retain_window_binding(
            &mut window,
            &Some(&valid_tmux),
            &Some(&valid_niri)
        ));
        assert_eq!(window.tmux_id.as_deref(), Some("@7"));
        assert_eq!(window.niri_id, None);
    }

    #[test]
    fn retain_window_binding_keeps_tmux_binding_when_tmux_probe_fails() {
        let mut window = WindowId {
            niri_id: None,
            tmux_id: Some("@7".to_string()),
        };
        let valid_niri = niri_windows(&[]);

        assert!(retain_window_binding(
            &mut window,
            &None,
            &Some(&valid_niri)
        ));
        assert_eq!(window.tmux_id.as_deref(), Some("@7"));
    }

    #[test]
    fn cleanup_stale_keeps_codex_binding_when_niri_probe_fails() {
        let mut store = SessionStore::default();
        store.codex_bindings.insert(
            "codex-1".to_string(),
            CodexBinding {
                session_id: "codex-1".to_string(),
                cwd: Some("/tmp/project".to_string()),
                updated_at: now(),
                window: WindowId {
                    niri_id: Some("42".to_string()),
                    tmux_id: None,
                },
            },
        );

        let valid_tmux = tmux_windows(&[]);
        cleanup_stale_with_window_snapshots(&mut store, Some(&valid_tmux), None, &codex_aliases());

        let binding = store
            .codex_bindings
            .get("codex-1")
            .expect("binding should be preserved when niri probe fails");
        assert_eq!(binding.window.niri_id.as_deref(), Some("42"));
    }

    #[test]
    fn cleanup_stale_drops_session_when_known_window_ids_are_all_invalid() {
        let mut store = SessionStore::default();
        store.sessions.insert(
            "@9".to_string(),
            Session {
                agent: "claude".to_string(),
                session_id: "session-9".to_string(),
                cwd: Some("/tmp/project".to_string()),
                state: SessionState::Idle,
                state_updated: now(),
                waiting_reason: None,
                transcript_path: None,
                window: WindowId {
                    niri_id: None,
                    tmux_id: Some("@9".to_string()),
                },
            },
        );

        let valid_tmux = tmux_windows(&[]);
        let valid_niri = niri_windows(&[]);
        cleanup_stale_with_window_snapshots(
            &mut store,
            Some(&valid_tmux),
            Some(&valid_niri),
            &codex_aliases(),
        );

        assert!(store.sessions.is_empty());
    }

    #[test]
    fn cleanup_stale_drops_codex_binding_when_niri_title_no_longer_matches_alias() {
        let mut store = SessionStore::default();
        store.codex_bindings.insert(
            "codex-1".to_string(),
            CodexBinding {
                session_id: "codex-1".to_string(),
                cwd: Some("/tmp/project".to_string()),
                updated_at: now(),
                window: WindowId {
                    niri_id: Some("42".to_string()),
                    tmux_id: None,
                },
            },
        );

        let live_niri_windows = HashMap::from([(
            "42".to_string(),
            NiriWindowInfo {
                title: Some("bash".to_string()),
            },
        )]);

        cleanup_stale_with_window_snapshots(
            &mut store,
            None,
            Some(&live_niri_windows),
            &codex_aliases(),
        );

        assert!(store.codex_bindings.is_empty());
    }

    #[test]
    fn cleanup_stale_keeps_codex_binding_when_tmux_command_matches_alias() {
        let mut store = SessionStore::default();
        store.codex_bindings.insert(
            "codex-1".to_string(),
            CodexBinding {
                session_id: "codex-1".to_string(),
                cwd: Some("/tmp/project".to_string()),
                updated_at: now(),
                window: WindowId {
                    niri_id: None,
                    tmux_id: Some("@9".to_string()),
                },
            },
        );

        let live_tmux_windows = HashMap::from([(
            "@9".to_string(),
            TmuxWindowInfo {
                title: "shell".to_string(),
                command: Some("cxy".to_string()),
            },
        )]);

        cleanup_stale_with_window_snapshots(
            &mut store,
            Some(&live_tmux_windows),
            None,
            &codex_aliases(),
        );

        assert!(store.codex_bindings.contains_key("codex-1"));
    }

    #[test]
    fn cleanup_unbound_codex_sessions_drops_session_that_lost_binding() {
        let mut store = SessionStore::default();
        store.codex_sessions.insert(
            "codex-1".to_string(),
            CodexSession {
                session_id: "codex-1".to_string(),
                cwd: "/tmp/project".to_string(),
                state: SessionState::Idle,
                state_updated: now(),
            },
        );

        cleanup_unbound_codex_sessions(
            &mut store,
            &HashSet::from(["codex-1".to_string()]),
            now() - STALE_SESSION_MAX_AGE_SECS,
        );

        assert!(store.codex_sessions.is_empty());
    }

    #[test]
    fn cleanup_unbound_codex_sessions_keeps_recent_unbound_session() {
        let mut store = SessionStore::default();
        store.codex_sessions.insert(
            "codex-1".to_string(),
            CodexSession {
                session_id: "codex-1".to_string(),
                cwd: "/tmp/project".to_string(),
                state: SessionState::Idle,
                state_updated: now(),
            },
        );

        cleanup_unbound_codex_sessions(
            &mut store,
            &HashSet::new(),
            now() - STALE_SESSION_MAX_AGE_SECS,
        );

        assert!(store.codex_sessions.contains_key("codex-1"));
    }

    #[test]
    fn cleanup_unbound_codex_sessions_drops_old_unbound_session() {
        let mut store = SessionStore::default();
        store.codex_sessions.insert(
            "codex-1".to_string(),
            CodexSession {
                session_id: "codex-1".to_string(),
                cwd: "/tmp/project".to_string(),
                state: SessionState::Idle,
                state_updated: now() - STALE_SESSION_MAX_AGE_SECS - 1.0,
            },
        );

        cleanup_unbound_codex_sessions(
            &mut store,
            &HashSet::new(),
            now() - STALE_SESSION_MAX_AGE_SECS,
        );

        assert!(store.codex_sessions.is_empty());
    }

    #[test]
    fn store_uses_tmux_windows_checks_sessions_and_bindings() {
        let mut store = SessionStore::default();
        assert!(!store_uses_tmux_windows(&store));

        store.sessions.insert(
            "@9".to_string(),
            Session {
                agent: "claude".to_string(),
                session_id: "session-9".to_string(),
                cwd: Some("/tmp/project".to_string()),
                state: SessionState::Idle,
                state_updated: 1.0,
                waiting_reason: None,
                transcript_path: None,
                window: WindowId {
                    niri_id: None,
                    tmux_id: Some("@9".to_string()),
                },
            },
        );
        assert!(store_uses_tmux_windows(&store));

        store.sessions.clear();
        store.codex_bindings.insert(
            "codex".to_string(),
            CodexBinding {
                session_id: "codex".to_string(),
                cwd: Some("/tmp/project".to_string()),
                updated_at: 1.0,
                window: WindowId {
                    niri_id: None,
                    tmux_id: Some("@10".to_string()),
                },
            },
        );
        assert!(store_uses_tmux_windows(&store));
    }

    #[test]
    fn store_uses_niri_windows_checks_sessions_and_bindings() {
        let mut store = SessionStore::default();
        assert!(!store_uses_niri_windows(&store));

        store.sessions.insert(
            "42".to_string(),
            Session {
                agent: "claude".to_string(),
                session_id: "session-42".to_string(),
                cwd: Some("/tmp/project".to_string()),
                state: SessionState::Idle,
                state_updated: 1.0,
                waiting_reason: None,
                transcript_path: None,
                window: WindowId {
                    niri_id: Some("42".to_string()),
                    tmux_id: None,
                },
            },
        );
        assert!(store_uses_niri_windows(&store));

        store.sessions.clear();
        store.codex_bindings.insert(
            "codex".to_string(),
            CodexBinding {
                session_id: "codex".to_string(),
                cwd: Some("/tmp/project".to_string()),
                updated_at: 1.0,
                window: WindowId {
                    niri_id: Some("43".to_string()),
                    tmux_id: None,
                },
            },
        );
        assert!(store_uses_niri_windows(&store));
    }

    #[test]
    fn load_from_path_returns_parse_error_for_invalid_json() {
        let path = test_state_path("parse-error");
        fs::write(&path, "{ definitely not json").expect("invalid test file should be written");

        let err = load_from_path(&path).expect_err("invalid json should fail loudly");
        assert!(matches!(err, StateError::Parse { .. }));
    }

    #[test]
    fn save_to_path_round_trips_without_leaking_temp_files() {
        let path = test_state_path("atomic-save");
        let mut store = SessionStore::default();
        store.sessions.insert(
            "@1".to_string(),
            Session {
                agent: "claude".to_string(),
                session_id: "session-1".to_string(),
                cwd: Some("/tmp/project".to_string()),
                state: SessionState::Idle,
                state_updated: 1.0,
                waiting_reason: None,
                transcript_path: None,
                window: WindowId {
                    niri_id: None,
                    tmux_id: Some("@1".to_string()),
                },
            },
        );

        save_to_path(&path, &store).expect("save should succeed");
        let loaded = load_from_path(&path).expect("saved state should load");

        assert_eq!(loaded.sessions.len(), 1);
        let tmp_files: Vec<_> =
            fs::read_dir(path.parent().expect("state file parent should exist"))
                .expect("temp dir should be readable")
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("sessions.json.tmp-"))
                .collect();
        assert!(tmp_files.is_empty(), "temp files leaked: {:?}", tmp_files);
    }

    #[test]
    fn with_locked_store_persists_mutations() {
        let path = test_state_path("locked-mutation");

        with_locked_store_at_path(&path, |store| {
            store.sessions.insert(
                "@9".to_string(),
                Session {
                    agent: "claude".to_string(),
                    session_id: "session-9".to_string(),
                    cwd: None,
                    state: SessionState::Responding,
                    state_updated: 9.0,
                    waiting_reason: None,
                    transcript_path: None,
                    window: WindowId {
                        niri_id: None,
                        tmux_id: Some("@9".to_string()),
                    },
                },
            );
            Ok(())
        })
        .expect("locked mutation should succeed");

        let loaded = load_from_path(&path).expect("mutated state should load");
        assert!(loaded.sessions.contains_key("@9"));
    }

    #[test]
    fn session_state_deserializes_legacy_lowercase_strings() {
        let session: Session = serde_json::from_value(serde_json::json!({
            "agent": "claude",
            "session_id": "session-1",
            "cwd": "/tmp/project",
            "state": "responding",
            "state_updated": 1.0,
            "window": { "tmux_id": "@1" }
        }))
        .expect("legacy lowercase session state should deserialize");

        assert_eq!(session.state, SessionState::Responding);
    }

    #[test]
    fn session_state_treats_unknown_values_as_unknown() {
        let session: Session = serde_json::from_value(serde_json::json!({
            "agent": "claude",
            "session_id": "session-1",
            "cwd": "/tmp/project",
            "state": "mystery",
            "state_updated": 1.0,
            "window": { "tmux_id": "@1" }
        }))
        .expect("unknown session state should still deserialize");

        assert_eq!(session.state, SessionState::Unknown);
    }
}
