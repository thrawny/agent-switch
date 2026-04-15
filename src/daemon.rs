use crate::state::{self, SessionStore};
use log::{debug, error, info, warn};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Instant, UNIX_EPOCH};

const SOCKET_IO_TIMEOUT_SECS: u64 = 2;
const SOCKET_MAX_FRAME_BYTES: usize = 256 * 1024;

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
    pub session_name: Option<String>,
    pub state: AgentState,
    pub cwd: Option<String>,
    pub state_updated: f64,
}

#[derive(Debug)]
pub enum DaemonMessage {
    Toggle,
    ToggleAgents,
    Track(TrackEvent),
    List(std::sync::mpsc::Sender<ListResponse>),
    SessionsChanged,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackEvent {
    pub event: TrackEventKind,
    #[serde(default)]
    pub agent: Option<String>,
    pub session_id: String,
    #[serde(default)]
    pub session_name: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum SocketRequest {
    Toggle {
        #[serde(default)]
        requested_at_ms: Option<u64>,
    },
    ToggleAgents {
        #[serde(default)]
        requested_at_ms: Option<u64>,
    },
    List,
    Ping,
    Track {
        event: TrackEvent,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum SocketResponse {
    Ok,
    Pong { pid: u32 },
    List { response: ListResponse },
    Error { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListResponse {
    pub sessions: Vec<ListEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListEntry {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_name: Option<String>,
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

#[derive(Debug, Default)]
pub struct SessionCache {
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

    pub fn replace_store(&mut self, mut store: SessionStore) {
        refresh_transcript_derived_states(&mut store);
        self.store = store;
    }

    pub fn refresh_dynamic_agent_states(&mut self) {
        refresh_transcript_derived_states(&mut self.store);
    }

    pub fn reload_agent_sessions(&mut self) {
        self.reload_agent_sessions_from_path(&state::state_file());
    }

    fn reload_agent_sessions_from_path(&mut self, path: &Path) {
        let store = match state::load_from_path(path) {
            Ok(store) => store,
            Err(err) => {
                error!("Failed to load state: {}", err);
                return;
            }
        };
        self.replace_store(store);
    }

    pub fn build_list_response(&mut self) -> ListResponse {
        self.refresh_dynamic_agent_states();
        let sessions: Vec<ListEntry> = self
            .store
            .sessions
            .iter()
            .map(|(key, session)| ListEntry {
                session_id: session.session_id.clone(),
                session_name: session.session_name.clone(),
                agent: session.agent.clone(),
                cwd: session.cwd.clone(),
                state: session.state.into(),
                state_updated: session.state_updated,
                window_id: key.clone(),
                tmux_id: session.window.tmux_id.clone(),
                niri_id: session.window.niri_id.clone(),
            })
            .collect();

        ListResponse { sessions }
    }
}

fn resolve_socket_path(
    agent_switch_socket: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
) -> PathBuf {
    agent_switch_socket.unwrap_or_else(|| {
        xdg_runtime_dir
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("agent-switch.sock")
    })
}

pub fn socket_path() -> PathBuf {
    resolve_socket_path(
        std::env::var_os("AGENT_SWITCH_SOCKET").map(PathBuf::from),
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
    )
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

fn socket_io_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(SOCKET_IO_TIMEOUT_SECS)
}

fn configure_socket_timeouts(stream: &UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(socket_io_timeout()))?;
    stream.set_write_timeout(Some(socket_io_timeout()))
}

fn parse_socket_frame<T>(line: &str, context: &'static str) -> io::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(line.trim_end()).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {context}: {err}"),
        )
    })
}

fn read_socket_frame<T>(stream: &mut UnixStream, context: &'static str) -> io::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line)?;
    if bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("empty {context} frame"),
        ));
    }
    if bytes > SOCKET_MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{context} frame exceeds {SOCKET_MAX_FRAME_BYTES} bytes"),
        ));
    }
    parse_socket_frame(&line, context)
}

fn write_socket_frame<T>(
    stream: &mut UnixStream,
    value: &T,
    context: &'static str,
) -> io::Result<()>
where
    T: Serialize,
{
    serde_json::to_writer(&mut *stream, value).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialize {context}: {err}"),
        )
    })?;
    stream.write_all(b"\n")
}

pub(crate) fn send_socket_request(request: &SocketRequest) -> io::Result<SocketResponse> {
    send_socket_request_to_path(&socket_path(), request)
}

pub(crate) fn send_socket_request_to_path(
    path: &Path,
    request: &SocketRequest,
) -> io::Result<SocketResponse> {
    let mut stream = UnixStream::connect(path)?;
    configure_socket_timeouts(&stream)?;
    write_socket_frame(&mut stream, request, "request")?;
    read_socket_frame(&mut stream, "response")
}

#[cfg_attr(not(feature = "niri"), allow(dead_code))]
pub(crate) fn send_toggle_request(agents_only: bool) -> io::Result<()> {
    let request = if agents_only {
        SocketRequest::ToggleAgents {
            requested_at_ms: Some(unix_now_ms()),
        }
    } else {
        SocketRequest::Toggle {
            requested_at_ms: Some(unix_now_ms()),
        }
    };
    match send_socket_request(&request)? {
        SocketResponse::Ok => Ok(()),
        SocketResponse::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected toggle response: {other:?}"),
        )),
    }
}

pub(crate) fn send_track_request(event: &TrackEvent) -> io::Result<()> {
    match send_socket_request(&SocketRequest::Track {
        event: event.clone(),
    })? {
        SocketResponse::Ok => Ok(()),
        SocketResponse::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected track response: {other:?}"),
        )),
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

fn handle_socket_request(
    request: SocketRequest,
    tx: &mpsc::Sender<DaemonMessage>,
    cache: &Arc<Mutex<SessionCache>>,
) -> SocketResponse {
    match request {
        SocketRequest::Toggle { requested_at_ms } => {
            log_toggle_request_delay(requested_at_ms, false);
            match tx.send(DaemonMessage::Toggle) {
                Ok(()) => SocketResponse::Ok,
                Err(err) => SocketResponse::Error {
                    message: format!("daemon not responding: {err}"),
                },
            }
        }
        SocketRequest::ToggleAgents { requested_at_ms } => {
            log_toggle_request_delay(requested_at_ms, true);
            match tx.send(DaemonMessage::ToggleAgents) {
                Ok(()) => SocketResponse::Ok,
                Err(err) => SocketResponse::Error {
                    message: format!("daemon not responding: {err}"),
                },
            }
        }
        SocketRequest::Ping => SocketResponse::Pong {
            pid: std::process::id(),
        },
        SocketRequest::List => {
            let (resp_tx, resp_rx) = mpsc::channel();
            if tx.send(DaemonMessage::List(resp_tx)).is_ok() {
                match resp_rx.recv_timeout(socket_io_timeout()) {
                    Ok(response) => SocketResponse::List { response },
                    Err(_) => {
                        let mut cache = cache.lock().unwrap();
                        SocketResponse::List {
                            response: cache.build_list_response(),
                        }
                    }
                }
            } else {
                SocketResponse::Error {
                    message: "daemon not responding".to_string(),
                }
            }
        }
        SocketRequest::Track { event } => {
            info!(
                "track {} agent={} session={} tmux={:?} niri={:?} cwd={:?}",
                event.event,
                event.agent.as_deref().unwrap_or("claude"),
                event.session_id,
                event.tmux_id,
                event.niri_id,
                event.cwd
            );
            match tx.send(DaemonMessage::Track(event)) {
                Ok(()) => SocketResponse::Ok,
                Err(err) => SocketResponse::Error {
                    message: format!("daemon not responding: {err}"),
                },
            }
        }
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn log_toggle_request_delay(requested_at_ms: Option<u64>, agents_only: bool) {
    let Some(requested_at_ms) = requested_at_ms else {
        return;
    };
    let now_ms = unix_now_ms();
    let delay_ms = now_ms.saturating_sub(requested_at_ms);
    debug!(
        "toggle request delay: {}ms agents_only={}",
        delay_ms, agents_only
    );
}

fn start_socket_listener_at_paths(
    tx: mpsc::Sender<DaemonMessage>,
    cache: Arc<Mutex<SessionCache>>,
    paths: &DaemonRuntimePaths,
) -> io::Result<DaemonInstanceGuard> {
    let guard = acquire_daemon_instance(paths)?;
    let listener = UnixListener::bind(&paths.socket_path)?;

    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    if let Err(err) = configure_socket_timeouts(&stream) {
                        error!("Socket timeout setup failed: {}", err);
                        continue;
                    }

                    let response = match read_socket_frame::<SocketRequest>(&mut stream, "request")
                    {
                        Ok(request) => handle_socket_request(request, &tx, &cache),
                        Err(err) => {
                            warn!("socket request error: {}", err);
                            SocketResponse::Error {
                                message: err.to_string(),
                            }
                        }
                    };

                    if let Err(err) = write_socket_frame(&mut stream, &response, "response") {
                        error!("Socket write error: {}", err);
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

pub fn start_socket_listener(
    tx: mpsc::Sender<DaemonMessage>,
    cache: Arc<Mutex<SessionCache>>,
) -> io::Result<DaemonInstanceGuard> {
    let paths = daemon_runtime_paths();
    start_socket_listener_at_paths(tx, cache, &paths)
}

pub fn start_sessions_watcher(tx: mpsc::Sender<DaemonMessage>) {
    let state_file = state::state_file();
    let state_dir = state_file.parent().map(|p| p.to_path_buf());

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
                    if is_state_file {
                        match event.kind {
                            notify::EventKind::Modify(_) | notify::EventKind::Create(_) => {
                                let _ = tx_clone.send(DaemonMessage::SessionsChanged);
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

        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
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

/// Run the headless daemon
pub fn run_headless() {
    let (tx, rx) = mpsc::channel();
    let cache = Arc::new(Mutex::new(SessionCache::new()));

    // Initial load
    {
        let mut cache = cache.lock().unwrap();
        cache.reload_agent_sessions();
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
            DaemonMessage::Shutdown => {
                info!("daemon shutting down");
                break;
            }
        }
    }
}

fn handle_track_event_at_path(event: &TrackEvent, focused_niri_id: Option<u64>, state_path: &Path) {
    let Some(agent) = event.agent.as_deref() else {
        error!(
            "Rejecting track event {} for session={}: missing agent",
            event.event, event.session_id
        );
        return;
    };
    let session_id = &event.session_id;
    let explicit_niri_id = event
        .niri_id
        .as_deref()
        .and_then(|id| id.parse::<u64>().ok());
    let focused_niri_id = explicit_niri_id.or(match event.event {
        TrackEventKind::SessionStart => None,
        _ => focused_niri_id,
    });

    if let Err(err) = state::with_locked_store_at_path(state_path, |store| {
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
                                        .or(session.transcript_path.as_ref())
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
                    session_name: normalized_session_name(event.session_name.as_deref()),
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
                        session_name: normalized_session_name(event.session_name.as_deref()),
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
                        .or(session.transcript_path.as_ref())
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

pub(crate) fn handle_track_event(event: &TrackEvent, focused_niri_id: Option<u64>) {
    handle_track_event_at_path(event, focused_niri_id, &state::state_file());
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

fn normalized_session_name(session_name: Option<&str>) -> Option<String> {
    session_name.and_then(|name| {
        let trimmed = name.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn update_session_metadata(session: &mut state::Session, event: &TrackEvent) {
    if let Some(session_name) = normalized_session_name(event.session_name.as_deref()) {
        session.session_name = Some(session_name);
    }
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

fn maybe_clear_stale_question_waiting(session: &mut state::Session) -> bool {
    if session.state != state::SessionState::Waiting || session.waiting_reason.is_some() {
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
    if ends_with_question(transcript_path) {
        return false;
    }

    session.state = state::SessionState::Idle;
    true
}

pub fn refresh_transcript_derived_states(store: &mut state::SessionStore) -> bool {
    let refresh_start = Instant::now();
    let mut changed = false;
    let mut permission_candidates = 0usize;
    let mut question_candidates = 0usize;
    for session in store.sessions.values_mut() {
        if session.state == state::SessionState::Waiting {
            match session.waiting_reason {
                Some(state::WaitingReason::PermissionPrompt) => permission_candidates += 1,
                None => question_candidates += 1,
            }
        }
        changed |= maybe_clear_permission_prompt_waiting(session);
        changed |= maybe_clear_stale_question_waiting(session);
    }
    let elapsed = refresh_start.elapsed();
    debug!(
        "transcript refresh: {}ms sessions={} permission_candidates={} question_candidates={} changed={}",
        elapsed.as_millis(),
        store.sessions.len(),
        permission_candidates,
        question_candidates,
        changed,
    );
    changed
}

fn ends_with_question(transcript_path: &str) -> bool {
    use std::process::Command;

    let check_start = Instant::now();
    let output = match Command::new("tail")
        .args(["-n", "120", transcript_path])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };

    let content = String::from_utf8_lossy(&output.stdout);
    let mut awaiting_user_response = false;

    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            match entry.get("type").and_then(|t| t.as_str()) {
                Some("assistant") => {
                    if let Some(content_arr) = entry
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    {
                        for item in content_arr {
                            match item.get("type").and_then(|t| t.as_str()) {
                                Some("text") => {
                                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                        awaiting_user_response = text.trim_end().ends_with('?');
                                    }
                                }
                                Some("tool_use") => {
                                    awaiting_user_response = false;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some("user") => {
                    awaiting_user_response = false;
                }
                _ => {}
            }
        }
    }

    let elapsed = check_start.elapsed();
    debug!(
        "transcript question check: {}ms path={}",
        elapsed.as_millis(),
        transcript_path
    );

    awaiting_user_response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::ErrorKind;
    use std::sync::atomic::{AtomicU64, Ordering};
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

    fn json_line(value: serde_json::Value) -> String {
        serde_json::to_string(&value).expect("test json should serialize")
    }

    fn test_runtime_paths(test_name: &str) -> DaemonRuntimePaths {
        let dir = test_codex_root(test_name);
        DaemonRuntimePaths {
            socket_path: dir.join("agent-switch.sock"),
            lock_path: dir.join("agent-switch.lock"),
        }
    }

    fn test_state_path(test_name: &str) -> PathBuf {
        test_codex_root(test_name)
            .join("state")
            .join("agent-switch")
            .join("sessions.json")
    }

    fn spawn_test_socket_daemon(
        test_name: &str,
    ) -> (
        DaemonRuntimePaths,
        PathBuf,
        mpsc::Sender<DaemonMessage>,
        thread::JoinHandle<()>,
        DaemonInstanceGuard,
    ) {
        let paths = test_runtime_paths(test_name);
        let state_path = test_state_path(test_name);
        let (tx, rx) = mpsc::channel();
        let cache = Arc::new(Mutex::new(SessionCache::new()));
        let guard = start_socket_listener_at_paths(tx.clone(), cache.clone(), &paths)
            .expect("test socket listener should start");
        let worker_state_path = state_path.clone();
        let worker_cache = cache.clone();

        let worker = thread::spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    DaemonMessage::Track(event) => {
                        handle_track_event_at_path(&event, None, &worker_state_path);
                        let mut cache = worker_cache.lock().unwrap();
                        cache.reload_agent_sessions_from_path(&worker_state_path);
                    }
                    DaemonMessage::List(resp_tx) => {
                        let mut cache = worker_cache.lock().unwrap();
                        let response = cache.build_list_response();
                        let _ = resp_tx.send(response);
                    }
                    DaemonMessage::SessionsChanged => {
                        let mut cache = worker_cache.lock().unwrap();
                        cache.reload_agent_sessions_from_path(&worker_state_path);
                    }
                    DaemonMessage::Toggle | DaemonMessage::ToggleAgents => {}
                    DaemonMessage::Shutdown => break,
                }
            }
        });

        (paths, state_path, tx, worker, guard)
    }

    fn write_test_transcript(test_name: &str, contents: &str) -> String {
        let dir = test_codex_root(test_name);
        let path = dir.join("transcript.jsonl");
        fs::write(&path, contents).expect("test transcript should be written");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn socket_path_prefers_agent_switch_socket_override() {
        let path = resolve_socket_path(
            Some(PathBuf::from("/tmp/custom-agent-switch.sock")),
            Some(PathBuf::from("/run/user/1000")),
        );

        assert_eq!(path, PathBuf::from("/tmp/custom-agent-switch.sock"));
    }

    #[test]
    fn socket_path_falls_back_to_xdg_runtime_dir() {
        let path = resolve_socket_path(None, Some(PathBuf::from("/run/user/1000")));

        assert_eq!(path, PathBuf::from("/run/user/1000/agent-switch.sock"));
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
            session_name: None,
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
            session_name: None,
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
                session_name: None,
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
                session_name: None,
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
                session_name: None,
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
    fn session_start_without_explicit_niri_id_does_not_bind_focused_window() {
        let state_path = test_state_path("session-start-no-focused-fallback");
        let session_id = "session-1";

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::SessionStart,
                session_id: session_id.to_string(),
                session_name: None,
                agent: Some("claude".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: None,
            },
            Some(19),
            &state_path,
        );

        let store =
            state::load_from_path(&state_path).expect("state should load after session-start");
        assert!(store.sessions.is_empty());

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::PromptSubmit,
                session_id: session_id.to_string(),
                session_name: None,
                agent: Some("claude".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("47".to_string()),
            },
            Some(19),
            &state_path,
        );

        let store =
            state::load_from_path(&state_path).expect("state should load after prompt-submit");
        let session = store
            .sessions
            .get("47")
            .expect("prompt-submit should backfill the actual niri window");
        assert_eq!(session.session_id, session_id);
        assert_eq!(session.window.niri_id.as_deref(), Some("47"));
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
                session_name: None,
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

        assert_eq!(response.sessions[0].state, AgentState::Responding);
        assert_eq!(session.state, state::SessionState::Responding);
        assert_eq!(session.waiting_reason, None);
        assert!(session.state_updated >= modified_at);
    }

    #[test]
    fn agent_hooks_persist_live_state_and_binding() {
        let state_path = test_state_path("codex-hooks-persist");
        let session_id = "codex-session-1";

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::SessionStart,
                session_id: session_id.to_string(),
                session_name: None,
                agent: Some("codex".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("47".to_string()),
            },
            Some(19),
            &state_path,
        );

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::PromptSubmit,
                session_id: session_id.to_string(),
                session_name: None,
                agent: Some("codex".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("56".to_string()),
            },
            Some(56),
            &state_path,
        );

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::Stop,
                session_id: session_id.to_string(),
                session_name: None,
                agent: Some("codex".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("56".to_string()),
            },
            Some(56),
            &state_path,
        );

        let store =
            state::load_from_path(&state_path).expect("state should load after codex hooks");
        let session = store
            .sessions
            .get("47")
            .expect("session should be persisted at the original window");

        assert_eq!(session.session_id, session_id);
        assert_eq!(session.window.niri_id.as_deref(), Some("47"));
        assert_eq!(session.cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(session.state, state::SessionState::Idle);
    }

    #[test]
    fn prompt_submit_backfills_binding_when_missing() {
        let state_path = test_state_path("codex-backfill-binding");
        let session_id = "codex-session-1";

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::SessionStart,
                session_id: session_id.to_string(),
                session_name: None,
                agent: Some("codex".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: None,
            },
            Some(19),
            &state_path,
        );

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::PromptSubmit,
                session_id: session_id.to_string(),
                session_name: None,
                agent: Some("codex".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("47".to_string()),
            },
            Some(47),
            &state_path,
        );

        let store =
            state::load_from_path(&state_path).expect("state should load after prompt-submit");
        let session = store
            .sessions
            .get("47")
            .expect("prompt-submit should create missing session");

        assert_eq!(session.session_id, session_id);
        assert_eq!(session.window.niri_id.as_deref(), Some("47"));
        assert_eq!(session.state, state::SessionState::Responding);
    }

    #[test]
    fn cache_reload_loads_sessions_from_state_file() {
        let state_path = test_state_path("codex-cache-reload");
        let session_id = "codex-session-1";

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::PromptSubmit,
                session_id: session_id.to_string(),
                session_name: None,
                agent: Some("codex".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("47".to_string()),
            },
            Some(47),
            &state_path,
        );

        let mut cache = SessionCache::new();
        cache.reload_agent_sessions_from_path(&state_path);

        let session = cache
            .store
            .sessions
            .get("47")
            .expect("cache should load session from state");
        assert_eq!(session.session_id, session_id);
        assert_eq!(session.state, state::SessionState::Responding);
        assert_eq!(session.cwd.as_deref(), Some("/tmp/project"));
    }

    #[test]
    fn refresh_transcript_derived_states_keeps_unanswered_question_waiting() {
        let transcript_path = write_test_transcript(
            "question-waiting-sticks",
            &format!(
                "{}\n",
                json_line(serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "content": [
                            { "type": "text", "text": "Want me to commit?" }
                        ]
                    }
                }))
            ),
        );
        let mut store = state::SessionStore::default();
        store.sessions.insert(
            "148".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-148".to_string(),
                session_name: None,
                cwd: Some("/tmp/project".to_string()),
                state: state::SessionState::Waiting,
                state_updated: 1.0,
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
    fn refresh_transcript_derived_states_clears_answered_question_waiting() {
        let transcript_path = write_test_transcript(
            "question-waiting-clears-after-answer",
            &[
                json_line(serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "content": [
                            { "type": "text", "text": "Done. Want me to commit both changes?" }
                        ]
                    }
                })),
                json_line(serde_json::json!({
                    "type": "user",
                    "message": {
                        "content": [
                            { "type": "text", "text": "/gc" }
                        ]
                    }
                })),
                json_line(serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "content": [
                            { "type": "tool_use", "id": "tool-1", "name": "Bash", "input": { "command": "git commit" } }
                        ]
                    }
                })),
            ]
            .join("\n"),
        );
        let mut store = state::SessionStore::default();
        store.sessions.insert(
            "148".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-148".to_string(),
                session_name: None,
                cwd: Some("/tmp/project".to_string()),
                state: state::SessionState::Waiting,
                state_updated: 1.0,
                waiting_reason: None,
                transcript_path: Some(transcript_path),
                window: state::WindowId {
                    tmux_id: None,
                    niri_id: Some("148".to_string()),
                },
            },
        );

        assert!(refresh_transcript_derived_states(&mut store));
        assert_eq!(
            store.sessions.get("148").map(|session| session.state),
            Some(state::SessionState::Idle)
        );
    }

    #[test]
    fn refresh_transcript_derived_states_keeps_question_waiting_without_new_transcript_activity() {
        let transcript_path = write_test_transcript(
            "question-waiting-needs-new-activity",
            &[
                json_line(serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "content": [
                            { "type": "text", "text": "Done. Want me to commit both changes?" }
                        ]
                    }
                })),
                json_line(serde_json::json!({
                    "type": "user",
                    "message": {
                        "content": [
                            { "type": "text", "text": "/gc" }
                        ]
                    }
                })),
            ]
            .join("\n"),
        );
        let modified_at = transcript_modified_at(&transcript_path).unwrap_or(0.0);
        let mut store = state::SessionStore::default();
        store.sessions.insert(
            "148".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-148".to_string(),
                session_name: None,
                cwd: Some("/tmp/project".to_string()),
                state: state::SessionState::Waiting,
                state_updated: modified_at + 5.0,
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
    fn socket_protocol_ping_returns_pid() {
        let (paths, _state_path, tx, worker, guard) = spawn_test_socket_daemon("socket-ping");

        let response = send_socket_request_to_path(&paths.socket_path, &SocketRequest::Ping)
            .expect("ping should succeed");

        assert!(matches!(
            response,
            SocketResponse::Pong { pid } if pid == std::process::id()
        ));

        let _ = tx.send(DaemonMessage::Shutdown);
        drop(guard);
        worker.join().expect("worker should exit cleanly");
    }

    #[test]
    fn socket_protocol_handles_large_track_request_and_list_round_trip() {
        let (paths, _state_path, tx, worker, guard) =
            spawn_test_socket_daemon("socket-large-track");
        let event = TrackEvent {
            event: TrackEventKind::PromptSubmit,
            agent: Some("claude".to_string()),
            session_id: "session-large".to_string(),
            session_name: None,
            cwd: Some(format!("/tmp/{}", "deep-project/".repeat(512))),
            transcript_path: None,
            notification_type: None,
            tmux_id: Some("@999".to_string()),
            niri_id: None,
        };

        let response = send_socket_request_to_path(
            &paths.socket_path,
            &SocketRequest::Track {
                event: event.clone(),
            },
        )
        .expect("large track request should succeed");
        assert!(matches!(response, SocketResponse::Ok));

        let response = send_socket_request_to_path(&paths.socket_path, &SocketRequest::List)
            .expect("list request should succeed");
        let SocketResponse::List { response } = response else {
            panic!("expected list response");
        };

        assert!(
            response
                .sessions
                .iter()
                .any(|entry| entry.session_id == event.session_id
                    && entry.tmux_id.as_deref() == Some("@999")
                    && entry.cwd.as_deref() == event.cwd.as_deref())
        );

        let _ = tx.send(DaemonMessage::Shutdown);
        drop(guard);
        worker.join().expect("worker should exit cleanly");
    }

    #[test]
    fn stop_without_event_transcript_path_falls_back_to_session_transcript_path() {
        let transcript_path = write_test_transcript(
            "stop-fallback-question",
            &format!(
                "{}\n",
                json_line(serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "content": [
                            { "type": "text", "text": "Want me to proceed with the refactor?" }
                        ]
                    }
                }))
            ),
        );
        let state_path = test_state_path("stop-fallback-question");

        // session-start includes transcript_path (like Claude hooks that set it up front)
        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::SessionStart,
                session_id: "claude-session-1".to_string(),
                session_name: None,
                agent: Some("claude".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: Some(transcript_path),
                notification_type: None,
                tmux_id: None,
                niri_id: Some("42".to_string()),
            },
            None,
            &state_path,
        );

        // prompt-submit (no transcript_path in event, like Claude hooks)
        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::PromptSubmit,
                session_id: "claude-session-1".to_string(),
                session_name: None,
                agent: Some("claude".to_string()),
                cwd: None,
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("42".to_string()),
            },
            Some(42),
            &state_path,
        );

        // stop event also has no transcript_path (the bug scenario)
        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::Stop,
                session_id: "claude-session-1".to_string(),
                session_name: None,
                agent: Some("claude".to_string()),
                cwd: None,
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("42".to_string()),
            },
            Some(42),
            &state_path,
        );

        let store = state::load_from_path(&state_path).expect("state should load after stop");
        let session = store.sessions.get("42").expect("session should exist");
        assert_eq!(
            session.state,
            state::SessionState::Waiting,
            "stop should detect question from session's transcript_path when event has none"
        );
    }

    #[test]
    fn stop_without_any_transcript_path_defaults_to_idle() {
        let state_path = test_state_path("stop-no-transcript");

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::SessionStart,
                session_id: "session-no-tp".to_string(),
                session_name: None,
                agent: Some("claude".to_string()),
                cwd: Some("/tmp/project".to_string()),
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("43".to_string()),
            },
            None,
            &state_path,
        );

        handle_track_event_at_path(
            &TrackEvent {
                event: TrackEventKind::Stop,
                session_id: "session-no-tp".to_string(),
                session_name: None,
                agent: Some("claude".to_string()),
                cwd: None,
                transcript_path: None,
                notification_type: None,
                tmux_id: None,
                niri_id: Some("43".to_string()),
            },
            Some(43),
            &state_path,
        );

        let store = state::load_from_path(&state_path).expect("state should load");
        let session = store.sessions.get("43").expect("session should exist");
        assert_eq!(
            session.state,
            state::SessionState::Idle,
            "stop without any transcript path should default to idle"
        );
    }

    #[test]
    fn socket_protocol_returns_structured_error_for_malformed_request() {
        let (paths, _state_path, tx, worker, guard) = spawn_test_socket_daemon("socket-malformed");
        let mut stream =
            UnixStream::connect(&paths.socket_path).expect("test socket should accept connections");
        configure_socket_timeouts(&stream).expect("socket timeouts should be configured");
        stream
            .write_all(b"{not-json}\n")
            .expect("malformed request should be sent");

        let response = read_socket_frame::<SocketResponse>(&mut stream, "response")
            .expect("daemon should return a structured error");
        assert!(matches!(response, SocketResponse::Error { .. }));

        let _ = tx.send(DaemonMessage::Shutdown);
        drop(guard);
        worker.join().expect("worker should exit cleanly");
    }
}
