use crate::state::{self, SessionStore};
use log::{debug, error, info, warn};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const CODEX_MAX_AGE_SECS: f64 = 7.0 * 24.0 * 60.0 * 60.0; // 7 days

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Waiting,
    Responding,
    Idle,
    #[serde(other)]
    Unknown,
}

impl AgentState {
    pub fn from_str(s: &str) -> Self {
        match s {
            "waiting" => Self::Waiting,
            "responding" => Self::Responding,
            "idle" => Self::Idle,
            _ => Self::Unknown,
        }
    }

    /// Get display label for the state (used by niri GTK overlay)
    #[cfg_attr(not(feature = "niri"), allow(dead_code))]
    pub fn icon(&self) -> &'static str {
        match self {
            Self::Responding => "\u{f013}", // nf-fa-cog (gear)
            Self::Waiting => "\u{f075}",    // nf-fa-comment
            Self::Idle => "\u{f186}",       // nf-fa-moon_o
            Self::Unknown => "?",
        }
    }
}

impl From<state::SessionState> for AgentState {
    fn from(value: state::SessionState) -> Self {
        match value {
            state::SessionState::Waiting => Self::Waiting,
            state::SessionState::Responding => Self::Responding,
            state::SessionState::Idle => Self::Idle,
            state::SessionState::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSession {
    pub agent: String,
    pub state: AgentState,
    pub cwd: Option<String>,
    pub state_updated: f64,
}

#[derive(Debug, Deserialize)]
struct CodexRecord {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    record_type: String,
    payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexSession {
    pub session_id: String,
    pub cwd: String,
    pub state: AgentState,
    pub state_updated: f64,
}

#[derive(Debug, Clone)]
struct LastMessage {
    role: String,
    text: String,
    timestamp: f64,
}

#[derive(Debug)]
pub enum DaemonMessage {
    Toggle,
    ToggleAgents,
    Track(TrackEvent),
    List(std::sync::mpsc::Sender<ListResponse>),
    SessionsChanged,
    CodexChanged,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrackEventKind {
    SessionStart,
    SessionEnd,
    PromptSubmit,
    Stop,
    Notification,
}

impl TrackEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::SessionEnd => "session-end",
            Self::PromptSubmit => "prompt-submit",
            Self::Stop => "stop",
            Self::Notification => "notification",
        }
    }
}

impl fmt::Display for TrackEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TrackEventKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "session-start" => Ok(Self::SessionStart),
            "session-end" => Ok(Self::SessionEnd),
            "prompt-submit" => Ok(Self::PromptSubmit),
            "stop" => Ok(Self::Stop),
            "notification" => Ok(Self::Notification),
            _ => Err(format!("unknown track event: {value}")),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrackEvent {
    pub event: TrackEventKind,
    #[serde(default)]
    pub agent: Option<String>,
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub tmux_id: Option<String>,
    #[serde(default)]
    pub niri_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListResponse {
    pub claude: Vec<ClaudeListEntry>,
    pub codex: Vec<CodexListEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeListEntry {
    pub session_id: String,
    pub agent: String,
    pub cwd: Option<String>,
    pub state: AgentState,
    pub state_updated: f64,
    pub window_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tmux_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub niri_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CodexListEntry {
    pub session_id: String,
    pub cwd: String,
    pub state: AgentState,
    pub state_updated: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tmux_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub niri_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct SessionCache {
    pub agent_sessions: HashMap<String, AgentSession>,
    pub codex_sessions: HashMap<String, CodexSession>,
    pub store: SessionStore,
}

#[derive(Debug, Clone)]
struct DaemonRuntimePaths {
    socket_path: PathBuf,
    lock_path: PathBuf,
}

#[derive(Debug)]
pub struct DaemonInstanceGuard {
    _lock_file: fs::File,
    socket_path: PathBuf,
}

impl Drop for DaemonInstanceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl SessionCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn sync_agent_sessions_from_store(&mut self) {
        self.agent_sessions.clear();

        for (key, session) in self.store.sessions.iter() {
            self.agent_sessions.insert(
                key.clone(),
                AgentSession {
                    agent: session.agent.clone(),
                    state: session.state.into(),
                    cwd: session.cwd.clone(),
                    state_updated: session.state_updated,
                },
            );
        }
    }

    pub fn replace_store(&mut self, mut store: SessionStore) {
        refresh_transcript_derived_states(&mut store);
        self.store = store;
        self.sync_agent_sessions_from_store();
    }

    pub fn refresh_dynamic_agent_states(&mut self) {
        if refresh_transcript_derived_states(&mut self.store) {
            self.sync_agent_sessions_from_store();
        }
    }

    pub fn reload_agent_sessions(&mut self) {
        let store = match state::load() {
            Ok(store) => store,
            Err(err) => {
                error!("Failed to load state: {}", err);
                return;
            }
        };
        self.replace_store(store);
    }

    pub fn reload_codex_sessions(&mut self) {
        self.codex_sessions = load_codex_sessions();
    }

    pub fn build_list_response(&mut self) -> ListResponse {
        self.refresh_dynamic_agent_states();
        let claude: Vec<ClaudeListEntry> = self
            .store
            .sessions
            .iter()
            .map(|(key, session)| ClaudeListEntry {
                session_id: session.session_id.clone(),
                agent: session.agent.clone(),
                cwd: session.cwd.clone(),
                state: session.state.into(),
                state_updated: session.state_updated,
                window_id: key.clone(),
                tmux_id: session.window.tmux_id.clone(),
                niri_id: session.window.niri_id.clone(),
            })
            .collect();

        let codex: Vec<CodexListEntry> = self
            .codex_sessions
            .values()
            .map(|session| {
                let binding = self.store.codex_bindings.get(&session.session_id);
                CodexListEntry {
                    session_id: session.session_id.clone(),
                    cwd: session.cwd.clone(),
                    state: session.state,
                    state_updated: session.state_updated,
                    tmux_id: binding.and_then(|entry| entry.window.tmux_id.clone()),
                    niri_id: binding.and_then(|entry| entry.window.niri_id.clone()),
                }
            })
            .collect();

        ListResponse { claude, codex }
    }
}

pub fn socket_path() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("agent-switch.sock")
}

fn daemon_runtime_paths() -> DaemonRuntimePaths {
    let socket_path = socket_path();
    let lock_path = socket_path
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("agent-switch.lock");
    DaemonRuntimePaths {
        socket_path,
        lock_path,
    }
}

fn write_daemon_metadata(lock_file: &mut fs::File, socket_path: &Path) -> io::Result<()> {
    lock_file.set_len(0)?;
    lock_file.rewind()?;
    writeln!(lock_file, "pid={}", std::process::id())?;
    writeln!(lock_file, "socket={}", socket_path.display())?;
    lock_file.sync_data()
}

fn acquire_daemon_instance(paths: &DaemonRuntimePaths) -> io::Result<DaemonInstanceGuard> {
    if let Some(parent) = paths.lock_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&paths.lock_path)?;

    let flock_result = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if flock_result != 0 {
        let err = io::Error::last_os_error();
        let already_running = matches!(
            err.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        );
        let message = format!(
            "agent-switch daemon already running (lock: {})",
            paths.lock_path.display()
        );
        return Err(if already_running {
            io::Error::new(io::ErrorKind::AlreadyExists, message)
        } else {
            io::Error::new(err.kind(), format!("{message}: {err}"))
        });
    }

    write_daemon_metadata(&mut lock_file, &paths.socket_path)?;

    if paths.socket_path.exists() {
        match UnixStream::connect(&paths.socket_path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "agent-switch daemon already listening on {}",
                        paths.socket_path.display()
                    ),
                ));
            }
            Err(_) => {
                if let Err(err) = fs::remove_file(&paths.socket_path)
                    && err.kind() != io::ErrorKind::NotFound
                {
                    return Err(err);
                }
            }
        }
    }

    Ok(DaemonInstanceGuard {
        _lock_file: lock_file,
        socket_path: paths.socket_path.clone(),
    })
}

pub fn start_socket_listener(
    tx: mpsc::Sender<DaemonMessage>,
    cache: Arc<Mutex<SessionCache>>,
) -> io::Result<DaemonInstanceGuard> {
    let paths = daemon_runtime_paths();
    let guard = acquire_daemon_instance(&paths)?;
    let listener = UnixListener::bind(&paths.socket_path)?;

    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let mut buf = [0u8; 4096];
                    if let Ok(count) = stream.read(&mut buf)
                        && count > 0
                    {
                        let cmd = String::from_utf8_lossy(&buf[..count]);
                        let cmd = cmd.trim();
                        if cmd == "toggle" {
                            debug!("toggle");
                            let _ = tx.send(DaemonMessage::Toggle);
                            let _ = stream.write_all(b"ok");
                        } else if cmd == "toggle-agents" {
                            debug!("toggle-agents");
                            let _ = tx.send(DaemonMessage::ToggleAgents);
                            let _ = stream.write_all(b"ok");
                        } else if cmd == "list" {
                            let (resp_tx, resp_rx) = mpsc::channel();
                            if tx.send(DaemonMessage::List(resp_tx)).is_ok() {
                                if let Ok(response) = resp_rx.recv() {
                                    if let Ok(json) = serde_json::to_string(&response) {
                                        let _ = stream.write_all(json.as_bytes());
                                    } else {
                                        let _ = stream.write_all(b"error: serialization failed");
                                    }
                                } else {
                                    // Daemon busy or shutting down, read cache directly
                                    let mut cache = cache.lock().unwrap();
                                    let response = cache.build_list_response();
                                    if let Ok(json) = serde_json::to_string(&response) {
                                        let _ = stream.write_all(json.as_bytes());
                                    } else {
                                        let _ = stream.write_all(b"error: serialization failed");
                                    }
                                }
                            } else {
                                let _ = stream.write_all(b"error: daemon not responding");
                            }
                        } else if let Some(json) = cmd.strip_prefix("track ") {
                            match serde_json::from_str::<TrackEvent>(json) {
                                Ok(event) => {
                                    info!(
                                        "track {} agent={} session={} tmux={:?} niri={:?} cwd={:?}",
                                        event.event,
                                        event.agent.as_deref().unwrap_or("claude"),
                                        event.session_id,
                                        event.tmux_id,
                                        event.niri_id,
                                        event.cwd
                                    );
                                    let _ = tx.send(DaemonMessage::Track(event));
                                    let _ = stream.write_all(b"ok");
                                }
                                Err(e) => {
                                    warn!("track parse error: {}", e);
                                    let _ = stream.write_all(format!("error: {}", e).as_bytes());
                                }
                            }
                        } else {
                            warn!("unknown command: {}", cmd);
                            let _ = stream.write_all(b"unknown command");
                        }
                    }
                }
                Err(e) => {
                    error!("Socket error: {}", e);
                }
            }
        }
    });

    Ok(guard)
}

pub fn start_sessions_watcher(tx: mpsc::Sender<DaemonMessage>) {
    let state_file = state::state_file();
    let state_dir = state_file.parent().map(|p| p.to_path_buf());
    let codex_dir = codex_sessions_root();

    thread::spawn(move || {
        let tx_clone = tx.clone();
        let state_filename = state_file.file_name().map(|s| s.to_os_string());

        let mut watcher = match RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let is_state_file = event
                        .paths
                        .iter()
                        .any(|p| p.file_name() == state_filename.as_deref());
                    let is_codex_file = event.paths.iter().any(|p| is_codex_rollout_file(p));
                    if is_state_file {
                        match event.kind {
                            notify::EventKind::Modify(_) | notify::EventKind::Create(_) => {
                                let _ = tx_clone.send(DaemonMessage::SessionsChanged);
                            }
                            _ => {}
                        }
                    } else if is_codex_file {
                        match event.kind {
                            notify::EventKind::Modify(_) | notify::EventKind::Create(_) => {
                                debug!(
                                    "codex file changed: {:?} ({:?})",
                                    event.paths.first().and_then(|p| p.file_name()),
                                    event.kind
                                );
                                let _ = tx_clone.send(DaemonMessage::CodexChanged);
                            }
                            _ => {}
                        }
                    }
                }
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                error!("Failed to create sessions watcher: {}", e);
                return;
            }
        };

        if let Some(dir) = state_dir {
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                error!("Failed to watch state directory: {}", e);
                return;
            }
        }

        if codex_dir.exists()
            && let Err(e) = watcher.watch(&codex_dir, RecursiveMode::Recursive)
        {
            error!("Failed to watch codex sessions directory: {}", e);
            return;
        }

        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    });
}

/// Poll codex rollout files for changes (FSEvents doesn't fire for appends to open fds)
pub fn start_codex_poller(tx: mpsc::Sender<DaemonMessage>) {
    thread::spawn(move || {
        let root = codex_sessions_root();
        let mut sizes: HashMap<PathBuf, u64> = HashMap::new();

        loop {
            thread::sleep(std::time::Duration::from_secs(3));

            if !root.exists() {
                continue;
            }

            let mut files = Vec::new();
            walk_codex_files(root.as_path(), &mut files);

            let mut changed = false;
            for file in &files {
                let size = fs::metadata(file).map(|m| m.len()).unwrap_or(0);
                let prev = sizes.get(file).copied();
                if prev.is_some_and(|p| p != size) {
                    changed = true;
                }
                sizes.insert(file.clone(), size);
            }

            if changed {
                debug!("codex poll: file size changed");
                let _ = tx.send(DaemonMessage::CodexChanged);
            }
        }
    });
}

/// Monitor tmux sockets for daemon lifecycle (headless mode only)
pub fn start_tmux_monitor(tx: mpsc::Sender<DaemonMessage>) {
    thread::spawn(move || {
        loop {
            thread::sleep(std::time::Duration::from_secs(5));
            if !tmux_server_running() {
                info!("No tmux sockets found, shutting down daemon");
                let _ = tx.send(DaemonMessage::Shutdown);
                return;
            }
        }
    });
}

fn tmux_server_running() -> bool {
    let sockets = find_tmux_sockets();
    if !sockets.is_empty() {
        return true;
    }

    // Fallback to tmux's default lookup in case the socket lives in a custom dir
    std::process::Command::new("tmux")
        .arg("list-sessions")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn find_tmux_sockets() -> Vec<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let mut sockets: HashSet<PathBuf> = HashSet::new();

    if let Some(path) = tmux_socket_from_env() {
        sockets.insert(path);
    }

    for dir in tmux_socket_dirs(uid) {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                sockets.insert(entry.path());
            }
        }
    }

    sockets.into_iter().collect()
}

fn tmux_socket_from_env() -> Option<PathBuf> {
    let value = std::env::var("TMUX").ok()?;
    let socket_path = value.split(',').next()?.trim();
    if socket_path.is_empty() {
        return None;
    }
    Some(PathBuf::from(socket_path))
}

fn tmux_socket_dirs(uid: u32) -> Vec<PathBuf> {
    let mut bases: HashSet<PathBuf> = HashSet::new();

    // tmux defaults
    bases.insert(PathBuf::from("/tmp"));
    if cfg!(target_os = "macos") {
        bases.insert(PathBuf::from("/private/tmp"));
    }

    // common overrides
    if let Ok(tmpdir) = std::env::var("TMUX_TMPDIR")
        && !tmpdir.is_empty()
    {
        bases.insert(PathBuf::from(tmpdir));
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR")
        && !runtime_dir.is_empty()
    {
        bases.insert(PathBuf::from(runtime_dir));
    }

    // Linux systems typically place tmux sockets under /run/user/<uid>
    if cfg!(target_os = "linux") {
        bases.insert(PathBuf::from(format!("/run/user/{}", uid)));
    }

    bases
        .into_iter()
        .map(|base| base.join(format!("tmux-{}", uid)))
        .collect()
}

// Codex session loading functions

fn codex_sessions_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".codex")
        .join("sessions")
}

fn is_codex_rollout_file(path: &Path) -> bool {
    let filename = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => name,
        None => return false,
    };
    filename.starts_with("rollout-") && filename.ends_with(".jsonl")
}

fn walk_codex_files(dir: &Path, files: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_codex_files(&path, files);
            } else if is_codex_rollout_file(path.as_path()) {
                files.push(path);
            }
        }
    }
}

fn update_codex_session(
    codex: &mut HashMap<String, CodexSession>,
    session_id: &str,
    cwd: Option<&str>,
    state: &str,
    state_updated: Option<f64>,
) {
    let new_state = AgentState::from_str(state);
    let entry = codex.entry(session_id.to_string()).or_insert(CodexSession {
        session_id: session_id.to_string(),
        cwd: String::new(),
        state: new_state,
        state_updated: 0.0,
    });

    if entry.cwd.is_empty()
        && let Some(value) = cwd
    {
        entry.cwd = value.to_string();
    }
    entry.state = new_state;
    // Only update timestamp if we have one from the record
    if let Some(ts) = state_updated {
        entry.state_updated = ts;
    }
}

fn update_last_message(
    last_message: &mut HashMap<String, LastMessage>,
    session_id: &str,
    role: &str,
    text: &str,
    timestamp: f64,
) {
    let replace = match last_message.get(session_id) {
        Some(existing) => timestamp >= existing.timestamp,
        None => true,
    };
    if replace {
        last_message.insert(
            session_id.to_string(),
            LastMessage {
                role: role.to_string(),
                text: text.to_string(),
                timestamp,
            },
        );
    }
}

fn handle_codex_record(
    codex: &mut HashMap<String, CodexSession>,
    last_message: &mut HashMap<String, LastMessage>,
    record: CodexRecord,
    file_session_id: Option<&str>,
    file_cwd: Option<&str>,
) {
    let record_ts = record_timestamp(&record);
    match record.record_type.as_str() {
        "session_meta" => {
            let session_id = record
                .payload
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if session_id.is_empty() {
                return;
            }
            if let Some(primary_session_id) = file_session_id
                && session_id != primary_session_id
            {
                return;
            }
            let cwd = record
                .payload
                .get("cwd")
                .and_then(|v| v.as_str())
                .or(file_cwd);
            update_codex_session(codex, session_id, cwd, "idle", record_ts);
        }
        "event_msg" => {
            let event_type = record
                .payload
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let session_id = record
                .payload
                .get("session_id")
                .and_then(|v| v.as_str())
                .or(file_session_id)
                .unwrap_or("");
            if session_id.is_empty() {
                return;
            }
            let Some(ts) = record_ts else {
                return;
            };
            match event_type {
                "user_message" => {
                    update_codex_session(codex, session_id, file_cwd, "responding", record_ts);
                    update_last_message(last_message, session_id, "user", "", ts);
                }
                "agent_message" => {
                    update_codex_session(codex, session_id, file_cwd, "idle", record_ts);
                    update_last_message(last_message, session_id, "assistant", "", ts);
                }
                // Agent is actively thinking/reasoning
                "agent_reasoning" => {
                    update_codex_session(codex, session_id, file_cwd, "responding", record_ts);
                }
                _ => {}
            }
        }
        "response_item" => {
            let role = record
                .payload
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let item_type = record
                .payload
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let session_id = record
                .payload
                .get("session_id")
                .and_then(|v| v.as_str())
                .or(file_session_id)
                .unwrap_or("");
            if session_id.is_empty() {
                return;
            }
            // Agent is actively working: function calls, reasoning (but NOT final message)
            if item_type == "function_call"
                || item_type == "reasoning"
                || item_type == "function_call_output"
            {
                update_codex_session(codex, session_id, file_cwd, "responding", record_ts);
            }
            // Capture assistant message text for "waiting" detection (ends with ?)
            if role == "assistant" && item_type == "message" {
                let Some(ts) = record_ts else {
                    return;
                };
                let text = extract_assistant_text(&record.payload).unwrap_or_default();
                update_last_message(last_message, session_id, "assistant", &text, ts);
            }
        }
        _ => {}
    }
}

fn process_codex_file(
    path: &Path,
    codex: &mut HashMap<String, CodexSession>,
    last_message: &mut HashMap<String, LastMessage>,
) {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return,
    };
    let reader = BufReader::new(file);
    let file_meta = read_codex_file_meta(path);
    let file_session_id = file_meta.as_ref().map(|(id, _)| id.as_str());
    let file_cwd = file_meta.as_ref().map(|(_, cwd)| cwd.as_str());

    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record = match serde_json::from_str::<CodexRecord>(trimmed) {
            Ok(record) => record,
            Err(_) => continue,
        };
        handle_codex_record(codex, last_message, record, file_session_id, file_cwd);
    }
}

fn read_codex_file_meta(path: &Path) -> Option<(String, String)> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record = serde_json::from_str::<CodexRecord>(trimmed).ok()?;
        if record.record_type != "session_meta" {
            continue;
        }
        let session_id = record
            .payload
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let cwd = record
            .payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if session_id.is_empty() || cwd.is_empty() {
            return None;
        }
        return Some((session_id, cwd));
    }
    None
}

fn load_codex_sessions_from_root(root: &Path) -> HashMap<String, CodexSession> {
    let mut codex: HashMap<String, CodexSession> = HashMap::new();
    let mut last_message: HashMap<String, LastMessage> = HashMap::new();
    if root.exists() {
        let mut files = Vec::new();
        walk_codex_files(root, &mut files);

        let now = state::now();
        for file in files {
            let meta = match fs::metadata(&file).and_then(|m| m.modified()) {
                Ok(modified) => modified,
                Err(_) => continue,
            };
            let mtime = meta
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let age = now - mtime;
            // Skip files older than cutoff
            if age > CODEX_MAX_AGE_SECS {
                continue;
            }
            process_codex_file(file.as_path(), &mut codex, &mut last_message);
        }
    }

    apply_codex_stale_timeout(&mut codex);
    apply_codex_waiting(&mut codex, &last_message);

    let now = state::now();
    codex.retain(|_, entry| {
        !entry.cwd.is_empty() && now - entry.state_updated <= CODEX_MAX_AGE_SECS
    });
    codex
}

pub fn load_codex_sessions() -> HashMap<String, CodexSession> {
    let root = codex_sessions_root();
    load_codex_sessions_from_root(root.as_path())
}

// Note: We previously had apply_codex_recent_activity() that used file mtime
// to guess "responding" state, but this caused issues when agent_message
// correctly set state to "idle" but the recent mtime overrode it.
// Now we rely solely on record content (function_call, reasoning, agent_message, etc.)

fn record_timestamp(record: &CodexRecord) -> Option<f64> {
    record
        .timestamp
        .as_deref()
        .and_then(parse_rfc3339_epoch)
        .or_else(|| {
            record
                .payload
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(parse_rfc3339_epoch)
        })
}

fn parse_rfc3339_epoch(value: &str) -> Option<f64> {
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|dt| dt.unix_timestamp_nanos() as f64 / 1_000_000_000.0)
}

fn apply_codex_stale_timeout(codex: &mut HashMap<String, CodexSession>) {
    let now = state::now();
    for entry in codex.values_mut() {
        // If "responding" but no updates for 10s, we don't know the actual state
        if entry.state == AgentState::Responding && now - entry.state_updated > 10.0 {
            entry.state = AgentState::Unknown;
        }
    }
}

fn apply_codex_waiting(
    codex: &mut HashMap<String, CodexSession>,
    last_message: &HashMap<String, LastMessage>,
) {
    for entry in codex.values_mut() {
        if entry.state != AgentState::Idle {
            continue;
        }
        if let Some(message) = last_message.get(&entry.session_id)
            && message.role == "assistant"
            && message.text.trim_end().ends_with('?')
        {
            entry.state = AgentState::Waiting;
        }
    }
}

fn extract_assistant_text(payload: &serde_json::Value) -> Option<String> {
    let content = payload.get("content")?.as_array()?;
    let mut last_text: Option<String> = None;
    for item in content {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if (item_type == "text" || item_type == "output_text" || item_type == "input_text")
            && let Some(text) = item.get("text").and_then(|v| v.as_str())
        {
            last_text = Some(text.to_string());
        }
    }
    last_text
}

/// Codex directory matching helpers (for tmux picker)
pub fn match_codex_by_dir<'a>(
    entry_dir: &str,
    entries: &'a HashMap<String, CodexSession>,
) -> Option<&'a CodexSession> {
    let mut best: Option<(&CodexSession, usize)> = None;
    for entry in entries.values() {
        if !should_match_dir(entry_dir, &entry.cwd) {
            continue;
        }
        let depth = path_depth(&entry.cwd);
        if best
            .as_ref()
            .map(|(current, current_depth)| {
                depth > *current_depth
                    || (depth == *current_depth && entry.state_updated > current.state_updated)
            })
            .unwrap_or(true)
        {
            best = Some((entry, depth));
        }
    }
    best.map(|(entry, _)| entry)
}

fn should_match_dir(entry_dir: &str, cwd: &str) -> bool {
    if cwd.is_empty() || cwd == "/" {
        return false;
    }
    if let Some(home) = dirs::home_dir()
        && cwd == home.to_string_lossy()
    {
        return false;
    }
    // Match if entry_dir is equal to or a subdirectory of cwd
    let entry_path = Path::new(entry_dir);
    let cwd_path = Path::new(cwd);
    entry_path.starts_with(cwd_path)
}

fn path_depth(path: &str) -> usize {
    Path::new(path)
        .components()
        .filter(|c| !matches!(c, std::path::Component::RootDir))
        .count()
}

/// Run the headless daemon
pub fn run_headless() {
    let (tx, rx) = mpsc::channel();
    let cache = Arc::new(Mutex::new(SessionCache::new()));

    // Initial load
    {
        let mut cache = cache.lock().unwrap();
        cache.reload_agent_sessions();
        cache.reload_codex_sessions();
    }

    let _daemon_instance = match start_socket_listener(tx.clone(), cache.clone()) {
        Ok(guard) => guard,
        Err(err) => {
            error!("Failed to start daemon: {}", err);
            return;
        }
    };
    info!("daemon started, socket={:?}", socket_path());

    start_sessions_watcher(tx.clone());
    start_codex_poller(tx.clone());
    start_tmux_monitor(tx.clone());

    loop {
        let msg = match rx.recv() {
            Ok(msg) => msg,
            Err(_) => break,
        };

        match msg {
            DaemonMessage::Toggle | DaemonMessage::ToggleAgents => {
                // No-op in headless mode
            }
            DaemonMessage::Track(event) => {
                handle_track_event(&event, None);
                let mut cache = cache.lock().unwrap();
                cache.reload_agent_sessions();
            }
            DaemonMessage::List(resp_tx) => {
                let mut cache = cache.lock().unwrap();
                let response = cache.build_list_response();
                let _ = resp_tx.send(response);
            }
            DaemonMessage::SessionsChanged => {
                let mut cache = cache.lock().unwrap();
                cache.reload_agent_sessions();
            }
            DaemonMessage::CodexChanged => {
                let mut cache = cache.lock().unwrap();
                cache.reload_codex_sessions();
                for entry in cache.codex_sessions.values() {
                    debug!(
                        "codex session: cwd={} state={:?} state_updated={}",
                        entry.cwd, entry.state, entry.state_updated
                    );
                }
            }
            DaemonMessage::Shutdown => {
                info!("daemon shutting down");
                break;
            }
        }
    }
}

pub(crate) fn handle_track_event(event: &TrackEvent, focused_niri_id: Option<u64>) {
    let agent = event.agent.as_deref().unwrap_or("claude");
    let session_id = &event.session_id;
    let focused_niri_id = event
        .niri_id
        .as_deref()
        .and_then(|id| id.parse::<u64>().ok())
        .or(focused_niri_id);

    if let Err(err) = state::with_locked_store(|store| {
        if agent == "codex" {
            match event.event {
                TrackEventKind::SessionStart => {
                    let window = match (&event.tmux_id, focused_niri_id) {
                        (Some(tmux), niri) => state::WindowId {
                            tmux_id: Some(tmux.clone()),
                            niri_id: niri.map(|n| n.to_string()),
                        },
                        (None, Some(niri)) => state::WindowId {
                            tmux_id: None,
                            niri_id: Some(niri.to_string()),
                        },
                        (None, None) => return Ok(()),
                    };
                    state::upsert_codex_binding(
                        store,
                        state::CodexBinding {
                            session_id: session_id.to_string(),
                            cwd: event.cwd.clone(),
                            updated_at: state::now(),
                            window,
                        },
                    );
                }
                TrackEventKind::SessionEnd => {
                    store.codex_bindings.remove(session_id);
                }
                _ => {}
            }
            return Ok(());
        }

        // Determine window key and IDs - prefer tmux_id, fall back to niri_id
        let (window_key, window_id) = match (&event.tmux_id, focused_niri_id) {
            (Some(tmux), niri) => (
                tmux.clone(),
                state::WindowId {
                    tmux_id: Some(tmux.clone()),
                    niri_id: niri.map(|n| n.to_string()),
                },
            ),
            (None, Some(niri)) => (
                niri.to_string(),
                state::WindowId {
                    tmux_id: None,
                    niri_id: Some(niri.to_string()),
                },
            ),
            (None, None) => {
                // No window info - can only update existing sessions
                match event.event {
                    TrackEventKind::SessionEnd => {
                        let key = store
                            .sessions
                            .iter()
                            .find(|(_, s)| s.agent == agent && s.session_id == *session_id)
                            .map(|(k, _)| k.clone());
                        if let Some(key) = key {
                            store.sessions.remove(&key);
                        }
                    }
                    TrackEventKind::PromptSubmit
                    | TrackEventKind::Stop
                    | TrackEventKind::Notification => {
                        if let Some(session) =
                            state::find_by_session_id_mut(store, agent, session_id)
                        {
                            update_session_metadata(session, event);
                            match event.event {
                                TrackEventKind::PromptSubmit => {
                                    session.state = state::SessionState::Responding;
                                    session.state_updated = state::now();
                                    clear_waiting_reason(session);
                                }
                                TrackEventKind::Stop => {
                                    let is_question = event
                                        .transcript_path
                                        .as_ref()
                                        .map(|p| ends_with_question(p))
                                        .unwrap_or(false);
                                    session.state = if is_question {
                                        state::SessionState::Waiting
                                    } else {
                                        state::SessionState::Idle
                                    };
                                    session.state_updated = state::now();
                                    clear_waiting_reason(session);
                                }
                                TrackEventKind::Notification
                                    if event.notification_type.as_deref()
                                        == Some("permission_prompt") =>
                                {
                                    set_permission_prompt_waiting(session);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }
        };

        match event.event {
            TrackEventKind::SessionStart => {
                remove_other_session_bindings(store, agent, session_id, &window_key);
                let session = state::Session {
                    agent: agent.to_string(),
                    session_id: session_id.to_string(),
                    cwd: event.cwd.clone(),
                    state: state::SessionState::Idle,
                    state_updated: state::now(),
                    waiting_reason: None,
                    transcript_path: event.transcript_path.clone(),
                    window: window_id,
                };
                store.sessions.insert(window_key, session);
            }
            TrackEventKind::SessionEnd => {
                let key = store
                    .sessions
                    .iter()
                    .find(|(_, s)| s.agent == agent && s.session_id == *session_id)
                    .map(|(k, _)| k.clone());
                if let Some(key) = key {
                    store.sessions.remove(&key);
                }
            }
            TrackEventKind::PromptSubmit => {
                if let Some(session) = state::find_by_session_id_mut(store, agent, session_id) {
                    update_session_metadata(session, event);
                    session.state = state::SessionState::Responding;
                    session.state_updated = state::now();
                    clear_waiting_reason(session);
                    update_session_window_binding(
                        session,
                        event.tmux_id.as_deref(),
                        focused_niri_id,
                    );
                } else {
                    let session = state::Session {
                        agent: agent.to_string(),
                        session_id: session_id.to_string(),
                        cwd: event.cwd.clone(),
                        state: state::SessionState::Responding,
                        state_updated: state::now(),
                        waiting_reason: None,
                        transcript_path: event.transcript_path.clone(),
                        window: window_id,
                    };
                    store.sessions.insert(window_key, session);
                }
            }
            TrackEventKind::Stop => {
                if let Some(session) = state::find_by_session_id_mut(store, agent, session_id) {
                    update_session_metadata(session, event);
                    let is_question = event
                        .transcript_path
                        .as_ref()
                        .map(|p| ends_with_question(p))
                        .unwrap_or(false);
                    session.state = if is_question {
                        state::SessionState::Waiting
                    } else {
                        state::SessionState::Idle
                    };
                    session.state_updated = state::now();
                    clear_waiting_reason(session);
                }
            }
            TrackEventKind::Notification => {
                if event.notification_type.as_deref() == Some("permission_prompt")
                    && let Some(session) = state::find_by_session_id_mut(store, agent, session_id)
                {
                    update_session_metadata(session, event);
                    set_permission_prompt_waiting(session);
                }
            }
        }

        Ok(())
    }) {
        error!(
            "Failed to persist track event {} for agent={} session={}: {}",
            event.event, agent, session_id, err
        );
    }
}

fn remove_other_session_bindings(
    store: &mut state::SessionStore,
    agent: &str,
    session_id: &str,
    keep_window_key: &str,
) {
    store.sessions.retain(|window_key, session| {
        window_key == keep_window_key || session.agent != agent || session.session_id != session_id
    });
}

fn update_session_window_binding(
    session: &mut state::Session,
    tmux_id: Option<&str>,
    niri_id: Option<u64>,
) {
    if let Some(tmux_id) = tmux_id {
        match session.window.tmux_id.as_deref() {
            Some(existing) if existing != tmux_id => {
                debug!(
                    "Ignoring tmux rebinding for session {}: keeping {} over {}",
                    session.session_id, existing, tmux_id
                );
            }
            None => {
                session.window.tmux_id = Some(tmux_id.to_string());
            }
            _ => {}
        }
    }

    if let Some(niri_id) = niri_id {
        let niri_id = niri_id.to_string();
        match session.window.niri_id.as_deref() {
            Some(existing) if existing != niri_id => {
                debug!(
                    "Ignoring niri rebinding for session {}: keeping {} over {}",
                    session.session_id, existing, niri_id
                );
            }
            None => {
                session.window.niri_id = Some(niri_id);
            }
            _ => {}
        }
    }
}

fn update_session_metadata(session: &mut state::Session, event: &TrackEvent) {
    if let Some(cwd) = &event.cwd {
        session.cwd = Some(cwd.clone());
    }
    if let Some(transcript_path) = &event.transcript_path {
        session.transcript_path = Some(transcript_path.clone());
    }
}

fn set_permission_prompt_waiting(session: &mut state::Session) {
    session.state = state::SessionState::Waiting;
    session.state_updated = state::now();
    session.waiting_reason = Some(state::WaitingReason::PermissionPrompt);
}

fn clear_waiting_reason(session: &mut state::Session) {
    session.waiting_reason = None;
}

fn transcript_modified_at(path: &str) -> Option<f64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs_f64())
}

fn maybe_clear_permission_prompt_waiting(session: &mut state::Session) -> bool {
    if session.state != state::SessionState::Waiting
        || session.waiting_reason != Some(state::WaitingReason::PermissionPrompt)
    {
        return false;
    }

    let Some(transcript_path) = session.transcript_path.as_deref() else {
        return false;
    };
    let Some(modified_at) = transcript_modified_at(transcript_path) else {
        return false;
    };
    if modified_at <= session.state_updated {
        return false;
    }

    session.state = state::SessionState::Responding;
    session.state_updated = modified_at;
    session.waiting_reason = None;
    true
}

pub fn refresh_transcript_derived_states(store: &mut state::SessionStore) -> bool {
    let mut changed = false;
    for session in store.sessions.values_mut() {
        changed |= maybe_clear_permission_prompt_waiting(session);
    }
    changed
}

fn ends_with_question(transcript_path: &str) -> bool {
    use std::process::Command;

    let output = match Command::new("tail")
        .args(["-n", "20", transcript_path])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };

    let content = String::from_utf8_lossy(&output.stdout);
    let mut last_text: Option<String> = None;

    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            if entry.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                continue;
            }
            if let Some(content_arr) = entry
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for item in content_arr {
                    if item.get("type").and_then(|t| t.as_str()) == Some("text")
                        && let Some(text) = item.get("text").and_then(|t| t.as_str())
                    {
                        last_text = Some(text.to_string());
                    }
                }
            }
        }
    }

    last_text
        .map(|t| t.trim_end().ends_with('?'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::ErrorKind;
    use std::sync::atomic::{AtomicU64, Ordering};
    use time::Duration;

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_codex_root(test_name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("agent-switch-daemon-tests")
            .join(format!(
                "{}-{}-{}",
                test_name,
                std::process::id(),
                TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&dir).expect("test codex dir should be created");
        dir
    }

    fn write_rollout(path: &Path, lines: &[String]) {
        let content = lines.join("\n");
        fs::write(path, format!("{content}\n")).expect("test rollout file should be written");
    }

    fn json_line(value: serde_json::Value) -> String {
        serde_json::to_string(&value).expect("test json should serialize")
    }

    fn ts(seconds_ago: i64) -> String {
        let now = OffsetDateTime::now_utc() - Duration::seconds(seconds_ago);
        now.format(&Rfc3339).expect("timestamp should format")
    }

    fn test_runtime_paths(test_name: &str) -> DaemonRuntimePaths {
        let dir = test_codex_root(test_name);
        DaemonRuntimePaths {
            socket_path: dir.join("agent-switch.sock"),
            lock_path: dir.join("agent-switch.lock"),
        }
    }

    fn write_test_transcript(test_name: &str, contents: &str) -> String {
        let dir = test_codex_root(test_name);
        let path = dir.join("transcript.jsonl");
        fs::write(&path, contents).expect("test transcript should be written");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn daemon_instance_lock_rejects_second_holder() {
        let paths = test_runtime_paths("daemon-lock");

        let first = acquire_daemon_instance(&paths).expect("first daemon lock should succeed");
        let err = acquire_daemon_instance(&paths).expect_err("second daemon lock should fail");

        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        drop(first);

        acquire_daemon_instance(&paths).expect("lock should be released after first guard drops");
    }

    #[test]
    fn daemon_instance_lock_removes_stale_socket_path() {
        let paths = test_runtime_paths("stale-socket");
        let listener =
            UnixListener::bind(&paths.socket_path).expect("stale socket path should be created");
        drop(listener);
        assert!(paths.socket_path.exists());

        let _guard =
            acquire_daemon_instance(&paths).expect("daemon should recover from stale socket path");

        assert!(!paths.socket_path.exists());
    }

    #[test]
    fn daemon_instance_lock_rejects_live_socket_listener() {
        let paths = test_runtime_paths("live-socket");
        let _listener =
            UnixListener::bind(&paths.socket_path).expect("live socket listener should bind");

        let err =
            acquire_daemon_instance(&paths).expect_err("active socket listener should be refused");

        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    }

    #[test]
    fn track_event_kind_round_trips_kebab_case() {
        let kind: TrackEventKind =
            serde_json::from_str("\"prompt-submit\"").expect("track event should deserialize");

        assert_eq!(kind, TrackEventKind::PromptSubmit);
        assert_eq!(
            serde_json::to_string(&kind).expect("track event should serialize"),
            "\"prompt-submit\""
        );
        assert_eq!(kind.to_string(), "prompt-submit");
    }

    #[test]
    fn track_event_deserializes_typed_event_kind() {
        let event: TrackEvent = serde_json::from_value(serde_json::json!({
            "event": "notification",
            "session_id": "session-1",
            "notification_type": "permission_prompt"
        }))
        .expect("track event payload should deserialize");

        assert_eq!(event.event, TrackEventKind::Notification);
        assert_eq!(event.session_id, "session-1");
        assert_eq!(
            event.notification_type.as_deref(),
            Some("permission_prompt")
        );
    }

    #[test]
    fn prompt_submit_preserves_existing_niri_binding() {
        let mut session = state::Session {
            agent: "claude".to_string(),
            session_id: "session-1".to_string(),
            cwd: Some("/tmp/project".to_string()),
            state: state::SessionState::Idle,
            state_updated: 1.0,
            waiting_reason: None,
            transcript_path: None,
            window: state::WindowId {
                tmux_id: None,
                niri_id: Some("122".to_string()),
            },
        };

        update_session_window_binding(&mut session, None, Some(56));

        assert_eq!(session.window.niri_id.as_deref(), Some("122"));
    }

    #[test]
    fn prompt_submit_backfills_missing_niri_binding() {
        let mut session = state::Session {
            agent: "claude".to_string(),
            session_id: "session-1".to_string(),
            cwd: Some("/tmp/project".to_string()),
            state: state::SessionState::Idle,
            state_updated: 1.0,
            waiting_reason: None,
            transcript_path: None,
            window: state::WindowId {
                tmux_id: None,
                niri_id: None,
            },
        };

        update_session_window_binding(&mut session, None, Some(56));

        assert_eq!(session.window.niri_id.as_deref(), Some("56"));
    }

    #[test]
    fn session_start_replaces_existing_binding_for_same_session() {
        let mut store = state::SessionStore::default();
        store.sessions.insert(
            "122".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-1".to_string(),
                cwd: Some("/tmp/old".to_string()),
                state: state::SessionState::Idle,
                state_updated: 1.0,
                waiting_reason: None,
                transcript_path: None,
                window: state::WindowId {
                    tmux_id: None,
                    niri_id: Some("122".to_string()),
                },
            },
        );
        store.sessions.insert(
            "219".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "other-session".to_string(),
                cwd: Some("/tmp/other".to_string()),
                state: state::SessionState::Idle,
                state_updated: 1.0,
                waiting_reason: None,
                transcript_path: None,
                window: state::WindowId {
                    tmux_id: None,
                    niri_id: Some("219".to_string()),
                },
            },
        );

        remove_other_session_bindings(&mut store, "claude", "session-1", "56");
        store.sessions.insert(
            "56".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-1".to_string(),
                cwd: Some("/tmp/new".to_string()),
                state: state::SessionState::Idle,
                state_updated: 2.0,
                waiting_reason: None,
                transcript_path: None,
                window: state::WindowId {
                    tmux_id: None,
                    niri_id: Some("56".to_string()),
                },
            },
        );

        assert!(!store.sessions.contains_key("122"));
        assert_eq!(
            store
                .sessions
                .get("56")
                .map(|session| session.session_id.as_str()),
            Some("session-1")
        );
        assert!(store.sessions.contains_key("219"));
    }

    #[test]
    fn build_list_response_clears_permission_prompt_waiting_after_transcript_progress() {
        let transcript_path = write_test_transcript("permission-prompt-progress", "{}\n");
        let modified_at =
            transcript_modified_at(&transcript_path).expect("transcript mtime should be available");
        let mut cache = SessionCache::new();
        cache.store.sessions.insert(
            "148".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-148".to_string(),
                cwd: Some("/tmp/project".to_string()),
                state: state::SessionState::Waiting,
                state_updated: modified_at - 1.0,
                waiting_reason: Some(state::WaitingReason::PermissionPrompt),
                transcript_path: Some(transcript_path),
                window: state::WindowId {
                    tmux_id: None,
                    niri_id: Some("148".to_string()),
                },
            },
        );

        let response = cache.build_list_response();
        let session = cache
            .store
            .sessions
            .get("148")
            .expect("session should remain in cache");

        assert_eq!(response.claude[0].state, AgentState::Responding);
        assert_eq!(session.state, state::SessionState::Responding);
        assert_eq!(session.waiting_reason, None);
        assert!(session.state_updated >= modified_at);
    }

    #[test]
    fn refresh_transcript_derived_states_keeps_question_waiting_without_permission_reason() {
        let transcript_path = write_test_transcript("question-waiting-sticks", "{}\n");
        let modified_at =
            transcript_modified_at(&transcript_path).expect("transcript mtime should be available");
        let mut store = state::SessionStore::default();
        store.sessions.insert(
            "148".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-148".to_string(),
                cwd: Some("/tmp/project".to_string()),
                state: state::SessionState::Waiting,
                state_updated: modified_at - 1.0,
                waiting_reason: None,
                transcript_path: Some(transcript_path),
                window: state::WindowId {
                    tmux_id: None,
                    niri_id: Some("148".to_string()),
                },
            },
        );

        assert!(!refresh_transcript_derived_states(&mut store));
        assert_eq!(
            store.sessions.get("148").map(|session| session.state),
            Some(state::SessionState::Waiting)
        );
    }

    #[test]
    fn test_should_match_dir_exact_match() {
        assert!(should_match_dir(
            "/Users/jonas/code/project",
            "/Users/jonas/code/project"
        ));
    }

    #[test]
    fn test_should_match_dir_rejects_parent() {
        // Entry in parent dir should NOT match codex session in child
        assert!(!should_match_dir(
            "/Users/jonas/code",
            "/Users/jonas/code/project"
        ));
    }

    #[test]
    fn test_should_match_dir_allows_subdirectory() {
        // Entry in subdirectory SHOULD match codex session in parent
        assert!(should_match_dir(
            "/Users/jonas/code/project/src",
            "/Users/jonas/code/project"
        ));
    }

    #[test]
    fn test_should_match_dir_rejects_empty() {
        assert!(!should_match_dir("/Users/jonas/code", ""));
    }

    #[test]
    fn test_should_match_dir_rejects_root() {
        assert!(!should_match_dir("/Users/jonas/code", "/"));
    }

    #[test]
    fn test_should_match_dir_rejects_sibling() {
        assert!(!should_match_dir(
            "/Users/jonas/code/other",
            "/Users/jonas/code/project"
        ));
    }

    #[test]
    fn match_codex_by_dir_uses_entry_cwd_not_map_key() {
        let mut entries = HashMap::new();
        entries.insert(
            "session-123".to_string(),
            CodexSession {
                session_id: "session-123".to_string(),
                cwd: "/Users/jonas/code/project".to_string(),
                state: AgentState::Idle,
                state_updated: 1.0,
            },
        );

        let matched = match_codex_by_dir("/Users/jonas/code/project/src", &entries)
            .expect("entry should match by cwd");
        assert_eq!(matched.session_id, "session-123");
    }

    #[test]
    fn load_codex_sessions_ignores_embedded_parent_session_meta_in_subagent_files() {
        let root = test_codex_root("codex-subagent-parent-meta");
        let day_dir = root.join("2026").join("03").join("14");
        fs::create_dir_all(&day_dir).expect("test day dir should be created");

        let parent_id = "parent-session";
        let child_id = "child-session";
        let cwd = "/tmp/project";

        write_rollout(
            &day_dir.join("rollout-parent.jsonl"),
            &[
                json_line(serde_json::json!({
                    "timestamp": ts(8),
                    "type": "session_meta",
                    "payload": { "id": parent_id, "cwd": cwd, "timestamp": ts(8) }
                })),
                json_line(serde_json::json!({
                    "timestamp": ts(3),
                    "type": "event_msg",
                    "payload": { "type": "user_message" }
                })),
                json_line(serde_json::json!({
                    "timestamp": ts(1),
                    "type": "response_item",
                    "payload": { "type": "function_call" }
                })),
            ],
        );

        write_rollout(
            &day_dir.join("rollout-child.jsonl"),
            &[
                json_line(serde_json::json!({
                    "timestamp": ts(7),
                    "type": "session_meta",
                    "payload": {
                        "id": child_id,
                        "forked_from_id": parent_id,
                        "cwd": cwd,
                        "timestamp": ts(7)
                    }
                })),
                json_line(serde_json::json!({
                    "timestamp": ts(7),
                    "type": "session_meta",
                    "payload": { "id": parent_id, "cwd": cwd, "timestamp": ts(8) }
                })),
            ],
        );

        let sessions = load_codex_sessions_from_root(root.as_path());
        let parent = sessions
            .get(parent_id)
            .expect("parent session should be loaded");
        let child = sessions
            .get(child_id)
            .expect("child session should be loaded");

        assert_eq!(parent.state, AgentState::Responding);
        assert!(parent.state_updated > child.state_updated);
        assert_eq!(child.state, AgentState::Idle);
    }
}
