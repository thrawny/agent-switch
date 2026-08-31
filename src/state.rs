use crate::compositor;
use log::warn;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STALE_SESSION_MAX_AGE_SECS: f64 = 24.0 * 60.0 * 60.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowId {
    /// Compositor window handle: niri's numeric id or a Hyprland `0x…`
    /// address. The serde name stays `niri_id` for compatibility with
    /// sessions.json files and hook payloads written before Hyprland support.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub niri_id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStore {
    #[serde(default)]
    pub sessions: HashMap<String, Session>,
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
    let compositor = compositor::get();
    let live_windows = store_uses_windows(store).then(|| compositor.live_window_ids());

    if let Some(Err(err)) = &live_windows {
        // A failed probe is not evidence that the windows are gone, so the
        // binding sweep is skipped entirely rather than run against nothing.
        warn!("Skipping {} stale cleanup: {}", compositor.name(), err);
    }

    cleanup_stale_with_window_snapshots(
        store,
        live_windows
            .as_ref()
            .and_then(|result| result.as_ref().ok()),
    );

    let cutoff = now() - STALE_SESSION_MAX_AGE_SECS;
    store
        .sessions
        .retain(|_, session| session.state_updated > cutoff);
}

fn store_uses_windows(store: &SessionStore) -> bool {
    store
        .sessions
        .values()
        .any(|session| session.window.niri_id.is_some())
}

fn cleanup_stale_with_window_snapshots(
    store: &mut SessionStore,
    live_windows: Option<&HashSet<String>>,
) {
    store
        .sessions
        .retain(|_, session| retain_window_binding(&mut session.window, &live_windows));
}

fn retain_window_binding(window: &mut WindowId, live_windows: &Option<&HashSet<String>>) -> bool {
    let drop_binding = matches!(
        (window.niri_id.as_ref(), live_windows),
        (Some(id), Some(valid)) if !valid.contains(id)
    );

    if drop_binding {
        window.niri_id = None;
    }

    window.niri_id.is_some()
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

    fn live_windows(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn cleanup_stale_drops_session_when_known_window_ids_are_all_invalid() {
        let mut store = SessionStore::default();
        store.sessions.insert(
            "@9".to_string(),
            Session {
                agent: "claude".to_string(),
                session_id: "session-9".to_string(),
                session_name: None,
                cwd: Some("/tmp/project".to_string()),
                state: SessionState::Idle,
                state_updated: now(),
                waiting_reason: None,
                transcript_path: None,
                window: WindowId { niri_id: None },
            },
        );

        let valid_windows = live_windows(&[]);
        cleanup_stale_with_window_snapshots(&mut store, Some(&valid_windows));

        assert!(store.sessions.is_empty());
    }

    #[test]
    fn store_uses_windows_checks_sessions() {
        let mut store = SessionStore::default();
        assert!(!store_uses_windows(&store));

        store.sessions.insert(
            "42".to_string(),
            Session {
                agent: "claude".to_string(),
                session_id: "session-42".to_string(),
                session_name: None,
                cwd: Some("/tmp/project".to_string()),
                state: SessionState::Idle,
                state_updated: 1.0,
                waiting_reason: None,
                transcript_path: None,
                window: WindowId {
                    niri_id: Some("42".to_string()),
                },
            },
        );
        assert!(store_uses_windows(&store));
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
                session_name: None,
                cwd: Some("/tmp/project".to_string()),
                state: SessionState::Idle,
                state_updated: 1.0,
                waiting_reason: None,
                transcript_path: None,
                window: WindowId { niri_id: None },
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
                    session_name: None,
                    cwd: None,
                    state: SessionState::Responding,
                    state_updated: 9.0,
                    waiting_reason: None,
                    transcript_path: None,
                    window: WindowId { niri_id: None },
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
            "window": { "niri_id": "1" }
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
            "window": { "niri_id": "1" }
        }))
        .expect("unknown session state should still deserialize");

        assert_eq!(session.state, SessionState::Unknown);
    }
}
