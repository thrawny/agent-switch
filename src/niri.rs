use crate::config;
use crate::daemon::{self, AgentSession, AgentState, DaemonMessage, SessionCache};
use crate::state;
use crate::themes;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Grid, Label, Orientation, PolicyType,
    ScrolledWindow, glib,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use log::debug;
use niri_ipc::{
    Action, Event, Request, Response, Window, Workspace, WorkspaceReferenceArg, socket::Socket,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use std::cell::RefCell;
use std::collections::HashMap;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const APP_ID: &str = "com.thrawny.agent-switch";
const KEYS: [char; 12] = ['h', 'j', 'k', 'l', 'u', 'i', 'o', 'p', 'n', 'm', ',', '.'];
const OVERLAY_WIDTH_RATIO: f64 = 0.45;
const OVERLAY_HEIGHT_RATIO: f64 = 0.70;
const OVERLAY_MIN_WIDTH: i32 = 340;
const OVERLAY_MAX_WIDTH: i32 = 1100;
const OVERLAY_MAX_HEIGHT: i32 = 900;
const OVERLAY_FALLBACK_WIDTH: i32 = 420;
const OVERLAY_FALLBACK_HEIGHT: i32 = 520;
const OVERLAY_WIDE_WIDTH_RATIO: f64 = 0.62;
const OVERLAY_WIDE_HEIGHT_RATIO: f64 = 0.82;
const OVERLAY_WIDE_MIN_WIDTH: i32 = 560;
const OVERLAY_WIDE_MAX_WIDTH: i32 = 1450;
const OVERLAY_WIDE_MAX_HEIGHT: i32 = 1100;
const OVERLAY_WIDE_FALLBACK_WIDTH: i32 = 780;
const OVERLAY_WIDE_FALLBACK_HEIGHT: i32 = 700;
const OVERLAY_MARGIN: i32 = 80;
const OVERLAY_STEP_SCROLL: f64 = 64.0;
const OVERLAY_PAGE_SCROLL: f64 = 320.0;
const WAITING_PRIORITY_WINDOW_SECS: f64 = 30.0 * 60.0;
const GTK_MAIN_LOOP_HEARTBEAT_MS: u64 = 100;
const GTK_MAIN_LOOP_STALL_WARN_MS: u128 = 200;
const GTK_MESSAGE_POLL_MS: u64 = 10;

// Use DaemonMessage as base, add niri-specific ReloadConfig
#[derive(Debug)]
enum NiriMessage {
    Daemon {
        msg: DaemonMessage,
        enqueued_at: Instant,
    },
    ReloadConfig,
    WorkspaceColumns {
        entries: Vec<WorkspaceColumn>,
        enqueued_at: Instant,
    },
}

#[derive(Debug, Clone)]
struct WorkspaceColumn {
    workspace_name: String,
    workspace_ref: WorkspaceReferenceArg,
    column_index: u32,
    window_title: Option<String>,
    window_id: Option<u64>,
}

struct AppState {
    config: config::Config,
    theme: &'static themes::Theme,
    agent_entries: Vec<WorkspaceColumn>,
    focused_at_open: Option<u64>,
    agent_sessions: HashMap<u64, AgentSession>,
    last_config_error: Option<String>,
}

#[derive(Clone, Copy)]
struct PendingOverlayFrame {
    presented_at: Instant,
}

#[derive(Default)]
struct UiDirtyState {
    sessions_dirty: AtomicBool,
}

impl UiDirtyState {
    fn mark_sessions(&self) {
        self.sessions_dirty.store(true, Ordering::Release);
    }

    fn take_sessions(&self) -> bool {
        self.sessions_dirty.swap(false, Ordering::AcqRel)
    }

    fn clear_all(&self) {
        self.sessions_dirty.store(false, Ordering::Release);
    }
}

fn notify_config_error(message: &str) {
    log::warn!("{}", message);
    let _ = Command::new("notify-send")
        .args(["agent-switch: config.toml error", message])
        .status();
}

fn agent_sessions_from_store(store: &state::SessionStore) -> HashMap<u64, AgentSession> {
    let mut sessions = HashMap::new();

    for (window_key, session) in store.sessions.iter() {
        let window_id = session_niri_window_id(window_key, session);
        let Some(window_id) = window_id else { continue };

        sessions.insert(
            window_id,
            AgentSession {
                agent: session.agent.clone(),
                session_name: session.session_name.clone(),
                state: session.state.into(),
                cwd: session.cwd.clone(),
                state_updated: session.state_updated,
            },
        );
    }
    sessions
}

fn session_niri_window_id(window_key: &str, session: &state::Session) -> Option<u64> {
    if let Ok(window_id) = window_key.parse::<u64>() {
        return Some(window_id);
    }

    session
        .window
        .niri_id
        .as_ref()
        .and_then(|id| id.parse::<u64>().ok())
}

fn overlay_snapshot_from_cache(cache: &Arc<Mutex<SessionCache>>) -> HashMap<u64, AgentSession> {
    let mut cache = cache.lock().unwrap();
    cache.refresh_dynamic_agent_states();
    agent_sessions_from_store(&cache.store)
}

fn load_clean_store_after_cleanup() -> state::Result<state::SessionStore> {
    state::with_locked_store(|store| {
        state::cleanup_stale(store);
        Ok(store.clone())
    })
}

fn replace_cache_store(cache: &Arc<Mutex<SessionCache>>, store: state::SessionStore) {
    let mut cache = cache.lock().unwrap();
    cache.replace_store(store);
}

fn refresh_cache_after_cleanup<F>(cache: &Arc<Mutex<SessionCache>>, cleanup: F) -> state::Result<()>
where
    F: FnOnce() -> state::Result<state::SessionStore>,
{
    let store = cleanup()?;
    replace_cache_store(cache, store);
    Ok(())
}

fn process_daemon_message(
    msg: DaemonMessage,
    cache: &Arc<Mutex<SessionCache>>,
    focused_window: &Arc<Mutex<Option<u64>>>,
) -> Option<NiriMessage> {
    match msg {
        DaemonMessage::Toggle => Some(NiriMessage::Daemon {
            msg,
            enqueued_at: Instant::now(),
        }),
        DaemonMessage::Shutdown => Some(NiriMessage::Daemon {
            msg,
            enqueued_at: Instant::now(),
        }),
        DaemonMessage::Track(event) => {
            let focused_id = *focused_window.lock().unwrap();
            daemon::handle_track_event(&event, focused_id);
            let mut cache = cache.lock().unwrap();
            cache.reload_agent_sessions();
            Some(NiriMessage::Daemon {
                msg: DaemonMessage::SessionsChanged,
                enqueued_at: Instant::now(),
            })
        }
        DaemonMessage::List(resp_tx) => {
            let mut cache = cache.lock().unwrap();
            let response = cache.build_list_response();
            let _ = resp_tx.send(response);
            None
        }
        DaemonMessage::SessionsChanged => {
            let mut cache = cache.lock().unwrap();
            cache.reload_agent_sessions();
            Some(NiriMessage::Daemon {
                msg: DaemonMessage::SessionsChanged,
                enqueued_at: Instant::now(),
            })
        }
    }
}

fn request_workspace_refresh(tx: mpsc::Sender<NiriMessage>, config: config::Config) {
    thread::spawn(move || {
        let refresh_start = Instant::now();
        let entries = get_agent_workspace_columns(&config);
        let elapsed = refresh_start.elapsed();
        log::debug!(
            "workspace refresh: {}ms entries={}",
            elapsed.as_millis(),
            entries.len(),
        );
        let _ = tx.send(NiriMessage::WorkspaceColumns {
            entries,
            enqueued_at: Instant::now(),
        });
    });
}

fn named_claude_title(title: &str) -> Option<String> {
    let name = title.trim().strip_prefix("✳ ")?.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("Claude Code") {
        return None;
    }
    Some(name.to_string())
}

fn named_pi_title(title: &str) -> Option<String> {
    let rest = title
        .trim()
        .strip_prefix("π - ")
        .or_else(|| title.trim().strip_prefix("Pi - "))
        .or_else(|| title.trim().strip_prefix("pi - "))?;
    let name = rest
        .rsplit_once(" - ")
        .map(|(name, _)| name)
        .unwrap_or(rest)
        .trim();
    if name.is_empty() || name.eq_ignore_ascii_case("pi") {
        return None;
    }
    Some(name.to_string())
}

fn title_duplicates_workspace(title: &str, workspace_name: &str) -> bool {
    title.trim().eq_ignore_ascii_case(workspace_name.trim())
}

fn tracked_session_title(entry: &WorkspaceColumn, session: &AgentSession) -> Option<String> {
    session
        .session_name
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .filter(|title| !title_duplicates_workspace(title, &entry.workspace_name))
        .map(str::to_string)
}

fn agent_fallback_from_window_title(entry: &WorkspaceColumn) -> Option<AgentInfo> {
    let title = entry.window_title.as_deref()?;
    if named_claude_title(title).is_some() {
        return Some(AgentInfo {
            agent: "claude".to_string(),
            state: AgentState::Idle,
            state_updated: None,
            title: None,
        });
    }
    if named_pi_title(title).is_some() {
        return Some(AgentInfo {
            agent: "pi".to_string(),
            state: AgentState::Idle,
            state_updated: None,
            title: None,
        });
    }
    None
}

fn niri_request(request: Request) -> Option<Response> {
    let request_name = niri_request_name(&request);
    let request_start = Instant::now();
    let mut socket = match Socket::connect() {
        Ok(socket) => socket,
        Err(err) => {
            log::debug!("niri request: {} connect failed: {}", request_name, err);
            return None;
        }
    };
    let response = match socket.send(request) {
        Ok(Ok(response)) => Some(response),
        Ok(Err(err)) => {
            log::debug!("niri request: {} failed: {}", request_name, err);
            None
        }
        Err(err) => {
            log::debug!("niri request: {} send failed: {}", request_name, err);
            None
        }
    };
    log::debug!(
        "niri request: {} {}ms success={}",
        request_name,
        request_start.elapsed().as_millis(),
        response.is_some(),
    );
    response
}

fn niri_request_name(request: &Request) -> &'static str {
    match request {
        Request::Windows => "windows",
        Request::Workspaces => "workspaces",
        Request::Action(Action::FocusWindow { .. }) => "action.focus-window",
        Request::Action(Action::FocusWorkspace { .. }) => "action.focus-workspace",
        Request::Action(Action::FocusColumn { .. }) => "action.focus-column",
        Request::Action(_) => "action.other",
        _ => "other",
    }
}

pub(crate) fn niri_action(action: Action) {
    let _ = niri_request(Request::Action(action));
}

pub(crate) fn niri_workspaces() -> Vec<Workspace> {
    match niri_request(Request::Workspaces) {
        Some(Response::Workspaces(workspaces)) => workspaces,
        _ => Vec::new(),
    }
}

pub(crate) fn niri_windows() -> Vec<Window> {
    match niri_request(Request::Windows) {
        Some(Response::Windows(windows)) => windows,
        _ => Vec::new(),
    }
}

fn should_skip_discovered_workspace(
    name_opt: Option<&str>,
    display_name: &str,
    config: &config::Config,
    seen_workspaces: &std::collections::HashSet<String>,
) -> bool {
    (name_opt.is_none() && config.ignore_unnamed_workspaces)
        || (config.ignore_numeric_sessions && config::is_numeric_name(display_name))
        || seen_workspaces.contains(display_name)
        || config.ignore.iter().any(|ignored| ignored == display_name)
}

fn get_agent_workspace_columns(config: &config::Config) -> Vec<WorkspaceColumn> {
    use std::collections::{BTreeMap, HashSet};

    let workspaces = niri_workspaces();
    let windows = niri_windows();

    let mut entries = Vec::new();
    let mut seen_workspaces: HashSet<String> = HashSet::new();

    let add_workspace_entries = |entries: &mut Vec<WorkspaceColumn>,
                                 ws_id: u64,
                                 ws_name: &str,
                                 workspace_ref: WorkspaceReferenceArg,
                                 windows_arr: &[&Window]| {
        let mut columns: BTreeMap<usize, Vec<&Window>> = BTreeMap::new();

        for window in windows_arr.iter() {
            if window.workspace_id != Some(ws_id) {
                continue;
            }
            let col_idx = window
                .layout
                .pos_in_scrolling_layout
                .map(|pos| pos.0)
                .unwrap_or(1);
            columns.entry(col_idx).or_default().push(*window);
        }

        for (&col_idx, col_windows) in &columns {
            let first_window = col_windows.first();

            entries.push(WorkspaceColumn {
                workspace_name: ws_name.to_string(),
                workspace_ref: workspace_ref.clone(),
                column_index: col_idx as u32,
                window_title: first_window.and_then(|w| w.title.clone()),
                window_id: first_window.map(|w| w.id),
            });
        }
    };

    let windows_refs: Vec<&Window> = windows.iter().collect();

    let mut discovered: Vec<_> = workspaces
        .iter()
        .filter_map(|ws| {
            let ws_id = ws.id;
            let name_opt = ws.name.as_deref();
            let idx = ws.idx;

            let display_name: String = match name_opt {
                Some(n) => n.to_string(),
                None => idx.to_string(),
            };

            if should_skip_discovered_workspace(name_opt, &display_name, config, &seen_workspaces) {
                return None;
            }
            seen_workspaces.insert(display_name.clone());

            let workspace_ref = match name_opt {
                Some(n) => WorkspaceReferenceArg::Name(n.to_string()),
                None => WorkspaceReferenceArg::Index(idx),
            };

            Some((idx, ws_id, display_name, workspace_ref))
        })
        .collect();

    discovered.sort_by_key(|(idx, _, _, _)| *idx);

    for (_, ws_id, display_name, workspace_ref) in discovered {
        add_workspace_entries(
            &mut entries,
            ws_id,
            &display_name,
            workspace_ref,
            &windows_refs,
        );
    }

    entries
}

fn focus_workspace(reference: WorkspaceReferenceArg) {
    niri_action(Action::FocusWorkspace { reference });
}

fn focus_column(index: u32) {
    niri_action(Action::FocusColumn {
        index: index as usize,
    });
}

pub(crate) fn focus_window(id: u64) -> bool {
    matches!(
        niri_request(Request::Action(Action::FocusWindow { id })),
        Some(Response::Handled)
    )
}

fn switch_to_entry(entry: &WorkspaceColumn) {
    let switch_start = Instant::now();
    if let Some(window_id) = entry.window_id {
        let focus_window_start = Instant::now();
        if focus_window(window_id) {
            log::info!(
                "switch_to_entry: {}ms path=focus-window workspace={} window_id={} focus_window={}ms",
                switch_start.elapsed().as_millis(),
                entry.workspace_name,
                window_id,
                focus_window_start.elapsed().as_millis(),
            );
            return;
        }
    }

    // Fallback when the window is gone: focus the workspace and column instead.
    focus_workspace(entry.workspace_ref.clone());
    std::thread::sleep(std::time::Duration::from_millis(100));
    focus_column(entry.column_index);
    log::info!(
        "switch_to_entry: {}ms path=focus-workspace workspace={} column={}",
        switch_start.elapsed().as_millis(),
        entry.workspace_name,
        entry.column_index,
    );
}

fn start_config_watcher(tx: mpsc::Sender<NiriMessage>) {
    let watched_paths = config::config_paths();
    let watched_dirs: Vec<_> = watched_paths
        .iter()
        .filter_map(|path| path.parent().map(|parent| parent.to_path_buf()))
        .collect();

    thread::spawn(move || {
        let tx_clone = tx.clone();
        let watched_paths = watched_paths.clone();
        let last_sent = std::sync::Mutex::new(Instant::now() - Duration::from_secs(1));

        let mut watcher = match RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let dominated_by_config = event
                        .paths
                        .iter()
                        .any(|path| watched_paths.iter().any(|entry| entry == path));
                    if dominated_by_config {
                        match event.kind {
                            notify::EventKind::Modify(_) | notify::EventKind::Create(_) => {
                                let mut last = last_sent.lock().unwrap();
                                if last.elapsed() >= Duration::from_millis(250) {
                                    *last = Instant::now();
                                    let _ = tx_clone.send(NiriMessage::ReloadConfig);
                                }
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
                log::error!("Failed to create config watcher: {}", e);
                return;
            }
        };

        for dir in watched_dirs {
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                log::error!("Failed to watch config directory: {}", e);
                return;
            }
        }

        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    });
}

fn start_focus_tracker(focused_window: Arc<Mutex<Option<u64>>>) {
    thread::spawn(move || {
        loop {
            let mut socket = match Socket::connect() {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Failed to connect to niri: {}", e);
                    thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            match socket.send(Request::EventStream) {
                Ok(Ok(Response::Handled)) => {}
                Ok(Ok(_)) => {}
                result => {
                    log::error!("Failed to request event stream: {:?}", result);
                    thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            }

            let mut read_event = socket.read_events();
            while let Ok(event) = read_event() {
                match event {
                    Event::WindowsChanged { windows } => {
                        let focused = windows.iter().find(|w| w.is_focused).map(|w| w.id);
                        *focused_window.lock().unwrap() = focused;
                    }
                    Event::WindowOpenedOrChanged { window } if window.is_focused => {
                        *focused_window.lock().unwrap() = Some(window.id);
                    }
                    Event::WindowFocusChanged { id } => {
                        *focused_window.lock().unwrap() = id;
                    }
                    Event::WindowClosed { id } => {
                        let mut guard = focused_window.lock().unwrap();
                        if *guard == Some(id) {
                            *guard = None;
                        }
                    }
                    _ => {}
                }
            }

            log::warn!("niri IPC event stream ended; reconnecting after backoff");
            thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

fn clamp_i32(value: i32, min: i32, max: i32) -> i32 {
    value.max(min).min(max)
}

fn overlay_monitor(window: &ApplicationWindow) -> Option<gtk4::gdk::Monitor> {
    let display = gtk4::gdk::Display::default()?;

    if let Some(surface) = window.surface()
        && let Some(monitor) = display.monitor_at_surface(&surface)
    {
        return Some(monitor);
    }

    display
        .monitors()
        .item(0)?
        .downcast::<gtk4::gdk::Monitor>()
        .ok()
}

fn overlay_size_caps_for_geometry(width: i32, height: i32, wide_agents_layout: bool) -> (i32, i32) {
    let available_width = (width - OVERLAY_MARGIN).max(320);
    let available_height = (height - OVERLAY_MARGIN).max(200);
    let width_ratio = if wide_agents_layout {
        OVERLAY_WIDE_WIDTH_RATIO
    } else {
        OVERLAY_WIDTH_RATIO
    };
    let height_ratio = if wide_agents_layout {
        OVERLAY_WIDE_HEIGHT_RATIO
    } else {
        OVERLAY_HEIGHT_RATIO
    };
    let min_width = if wide_agents_layout {
        OVERLAY_WIDE_MIN_WIDTH
    } else {
        OVERLAY_MIN_WIDTH
    };
    let max_width_cap = if wide_agents_layout {
        OVERLAY_WIDE_MAX_WIDTH
    } else {
        OVERLAY_MAX_WIDTH
    };
    let max_height_cap = if wide_agents_layout {
        OVERLAY_WIDE_MAX_HEIGHT
    } else {
        OVERLAY_MAX_HEIGHT
    };
    let max_width = clamp_i32(
        (width as f64 * width_ratio).round() as i32,
        min_width.min(available_width),
        max_width_cap.min(available_width),
    );
    let max_height = clamp_i32(
        (height as f64 * height_ratio).round() as i32,
        1,
        max_height_cap.min(available_height),
    );

    (max_width, max_height)
}

fn input_char_for_key(keyval: gtk4::gdk::Key) -> Option<char> {
    keyval.to_unicode().map(|ch| ch.to_ascii_lowercase())
}

fn selection_key_for_input(keyval: gtk4::gdk::Key) -> Option<char> {
    input_char_for_key(keyval).filter(|ch| KEYS.contains(ch))
}

fn agent_selection_key_for_index(index: usize) -> Option<char> {
    KEYS.get(index).copied()
}

fn same_workspace_entry(a: &WorkspaceColumn, b: &WorkspaceColumn) -> bool {
    a.workspace_name == b.workspace_name
        && a.column_index == b.column_index
        && a.window_id == b.window_id
}

fn workspace_entries_changed(current: &[WorkspaceColumn], updated: &[WorkspaceColumn]) -> bool {
    current.len() != updated.len()
        || current
            .iter()
            .zip(updated.iter())
            .any(|(left, right)| !same_workspace_entry(left, right))
}

fn update_overlay_size(
    window: &ApplicationWindow,
    scroller: &ScrolledWindow,
    outer_box: &GtkBox,
    wide_agents_layout: bool,
) {
    let (max_width, max_height) = overlay_monitor(window)
        .map(|monitor| {
            let geometry = monitor.geometry();
            overlay_size_caps_for_geometry(geometry.width(), geometry.height(), wide_agents_layout)
        })
        .unwrap_or(if wide_agents_layout {
            (OVERLAY_WIDE_FALLBACK_WIDTH, OVERLAY_WIDE_FALLBACK_HEIGHT)
        } else {
            (OVERLAY_FALLBACK_WIDTH, OVERLAY_FALLBACK_HEIGHT)
        });

    scroller.set_max_content_width(max_width);
    scroller.set_max_content_height(max_height);

    // Reset any previous size request so preferred_size reflects actual content
    window.set_size_request(-1, -1);
    outer_box.set_size_request(-1, -1);

    let (_, natural) = outer_box.preferred_size();
    let width = clamp_i32(
        natural.width(),
        if wide_agents_layout {
            OVERLAY_WIDE_MIN_WIDTH.min(max_width)
        } else {
            OVERLAY_MIN_WIDTH.min(max_width)
        },
        max_width,
    );
    let height = clamp_i32(natural.height().max(1), 1, max_height);

    // Reset to 1x1 first — set_default_size won't shrink past a previous larger value
    window.set_default_size(1, 1);
    window.set_default_size(width, height);
    outer_box.set_size_request(width, height);
    window.queue_resize();
}

fn scroll_overlay(scroller: &ScrolledWindow, delta: f64) {
    let adjustment = scroller.vadjustment();
    let lower = adjustment.lower();
    let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
    let next = (adjustment.value() + delta).clamp(lower, upper);
    adjustment.set_value(next);
}

fn scroll_overlay_by_step(scroller: &ScrolledWindow, direction: f64) {
    let adjustment = scroller.vadjustment();
    let delta = adjustment.step_increment().max(OVERLAY_STEP_SCROLL) * direction;
    scroll_overlay(scroller, delta);
}

fn scroll_overlay_by_page(scroller: &ScrolledWindow, direction: f64) {
    let adjustment = scroller.vadjustment();
    let delta = adjustment
        .page_increment()
        .max(adjustment.page_size() * 0.9)
        .max(OVERLAY_PAGE_SCROLL)
        * direction;
    scroll_overlay(scroller, delta);
}

fn reset_overlay_scroll(scroller: &ScrolledWindow) {
    let adjustment = scroller.vadjustment();
    adjustment.set_value(adjustment.lower());
}

fn scroll_overlay_to_end(scroller: &ScrolledWindow) {
    let adjustment = scroller.vadjustment();
    let upper = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
    adjustment.set_value(upper);
}

fn apply_theme_css(css_provider: &gtk4::CssProvider, theme: &themes::Theme) {
    let base_css = include_str!("niri.css");
    css_provider.load_from_data(&format!("{}\n{}", theme.css, base_css));
}

fn load_overlay_css(theme: &themes::Theme) -> gtk4::CssProvider {
    let display = gtk4::gdk::Display::default().unwrap();

    let css_provider = gtk4::CssProvider::new();
    apply_theme_css(&css_provider, theme);
    gtk4::style_context_add_provider_for_display(
        &display,
        &css_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // User overrides
    if let Some(path) = dirs::config_dir().map(|p| p.join("agent-switch").join("style.css"))
        && let Ok(user_css) = std::fs::read_to_string(&path)
    {
        let user_provider = gtk4::CssProvider::new();
        user_provider.load_from_data(&user_css);
        gtk4::style_context_add_provider_for_display(
            &display,
            &user_provider,
            gtk4::STYLE_PROVIDER_PRIORITY_USER,
        );
    }

    css_provider
}

fn build_ui(
    app: &Application,
    rx: mpsc::Receiver<NiriMessage>,
    tx: mpsc::Sender<NiriMessage>,
    focused_window: Arc<Mutex<Option<u64>>>,
    cache: Arc<Mutex<SessionCache>>,
    dirty_state: Arc<UiDirtyState>,
) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(OVERLAY_FALLBACK_WIDTH)
        .default_height(OVERLAY_FALLBACK_HEIGHT)
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_anchor(Edge::Top, false);
    window.set_anchor(Edge::Bottom, false);
    window.set_anchor(Edge::Left, false);
    window.set_anchor(Edge::Right, false);

    let (config, last_config_error) = match config::load_config() {
        Ok(config) => (config, None),
        Err(err) => {
            notify_config_error(&err);
            (config::Config::default(), Some(err))
        }
    };
    let theme = themes::get(&config.theme);
    let agent_entries = get_agent_workspace_columns(&config);
    let agent_sessions = overlay_snapshot_from_cache(&cache);

    let state = Rc::new(RefCell::new(AppState {
        config,
        theme,
        agent_entries,
        focused_at_open: None,
        agent_sessions,
        last_config_error,
    }));

    let outer_box = GtkBox::new(Orientation::Vertical, 0);
    outer_box.add_css_class("outer");
    let pending_overlay_frame = Rc::new(RefCell::new(None::<PendingOverlayFrame>));

    let scroller = ScrolledWindow::new();
    scroller.set_policy(PolicyType::Never, PolicyType::Automatic);
    scroller.set_propagate_natural_width(true);
    scroller.set_propagate_natural_height(true);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);

    let main_box = GtkBox::new(Orientation::Vertical, 10);
    main_box.set_margin_top(20);
    main_box.set_margin_start(28);
    main_box.set_margin_end(28);
    main_box.set_margin_bottom(20);
    main_box.set_halign(gtk4::Align::Fill);
    main_box.set_hexpand(true);
    scroller.set_child(Some(&main_box));

    {
        let state_ref = state.borrow();
        build_agents_list(
            &main_box,
            &state_ref.agent_entries,
            &state_ref.agent_sessions,
            state_ref.focused_at_open,
            state_ref.theme,
        );
    }
    outer_box.append(&scroller);

    let css_provider = load_overlay_css(theme);

    window.set_child(Some(&outer_box));
    let pending_overlay_frame_for_tick = pending_overlay_frame.clone();
    window.add_tick_callback(move |_, _| {
        if let Some(pending) = pending_overlay_frame_for_tick.borrow_mut().take() {
            log::info!(
                "overlay first frame: {}ms",
                pending.presented_at.elapsed().as_millis(),
            );
        }
        glib::ControlFlow::Continue
    });

    // Helper: rebuild the agents view
    let rebuild_current_view = |main_box: &GtkBox, state: &AppState, _lock_label_widths: bool| {
        build_agents_list(
            main_box,
            &state.agent_entries,
            &state.agent_sessions,
            state.focused_at_open,
            state.theme,
        );
    };

    let rebuild_for_poll = rebuild_current_view;

    let key_controller = gtk4::EventControllerKey::new();
    let state_clone = state.clone();
    let window_clone = window.clone();
    let scroller_clone = scroller.clone();

    key_controller.connect_key_pressed(move |_, keyval, _, _| {
        let input_char = input_char_for_key(keyval);
        let selection_key = selection_key_for_input(keyval);
        let key_name = keyval.name().map(|s| s.to_lowercase());
        let key = match key_name.as_deref() {
            Some(key) => key,
            None if selection_key.is_some() => "",
            None => return glib::Propagation::Proceed,
        };

        match key {
            "up" => {
                scroll_overlay_by_step(&scroller_clone, -1.0);
                return glib::Propagation::Stop;
            }
            "down" => {
                scroll_overlay_by_step(&scroller_clone, 1.0);
                return glib::Propagation::Stop;
            }
            "page_up" => {
                scroll_overlay_by_page(&scroller_clone, -1.0);
                return glib::Propagation::Stop;
            }
            "page_down" => {
                scroll_overlay_by_page(&scroller_clone, 1.0);
                return glib::Propagation::Stop;
            }
            "home" => {
                reset_overlay_scroll(&scroller_clone);
                return glib::Propagation::Stop;
            }
            "end" => {
                scroll_overlay_to_end(&scroller_clone);
                return glib::Propagation::Stop;
            }
            _ => {}
        }

        if input_char == Some('q') || key == "escape" {
            window_clone.set_visible(false);
            return glib::Propagation::Stop;
        }

        if key == "space" {
            let state = state_clone.borrow();
            if let Some(target) = find_smart_jump_target(
                &state.agent_entries,
                &state.agent_sessions,
                state.focused_at_open,
            ) {
                let target = target.clone();
                drop(state);
                window_clone.set_visible(false);
                switch_to_entry(&target);
            }
            return glib::Propagation::Stop;
        }

        if let Some(key_char) = selection_key {
            let state = state_clone.borrow();
            if let Some(target) = find_agent_entry_for_selection_key(
                &state.agent_entries,
                &state.agent_sessions,
                state.focused_at_open,
                key_char,
            ) {
                let target = target.clone();
                drop(state);
                window_clone.set_visible(false);
                switch_to_entry(&target);
            }
        }

        glib::Propagation::Stop
    });

    window.add_controller(key_controller);
    window.set_visible(false);
    window.present();
    update_overlay_size(&window, &scroller, &outer_box, false);
    window.set_visible(false);

    let window_for_poll = window.clone();
    let state_for_poll = state.clone();
    let main_box_for_poll = main_box.clone();
    let outer_box_for_poll = outer_box.clone();
    let scroller_for_poll = scroller.clone();
    let focused_window_for_poll = focused_window.clone();
    let cache_for_poll = cache.clone();
    let css_provider_for_poll = css_provider.clone();
    let tx_for_poll = tx.clone();
    let dirty_state_for_poll = dirty_state.clone();
    let pending_overlay_frame_for_poll = pending_overlay_frame.clone();
    let main_loop_last_tick = Rc::new(RefCell::new(Instant::now()));
    let main_loop_last_tick_for_monitor = main_loop_last_tick.clone();

    glib::timeout_add_local(
        std::time::Duration::from_millis(GTK_MAIN_LOOP_HEARTBEAT_MS),
        move || {
            let now = Instant::now();
            let mut last_tick = main_loop_last_tick_for_monitor.borrow_mut();
            let elapsed = now.duration_since(*last_tick);
            let stall_ms = elapsed
                .as_millis()
                .saturating_sub(GTK_MAIN_LOOP_HEARTBEAT_MS as u128);
            if stall_ms >= GTK_MAIN_LOOP_STALL_WARN_MS {
                log::warn!(
                    "gtk main loop stalled: {}ms elapsed={}ms",
                    stall_ms,
                    elapsed.as_millis(),
                );
            }
            *last_tick = now;
            glib::ControlFlow::Continue
        },
    );

    glib::timeout_add_local(
        std::time::Duration::from_millis(GTK_MESSAGE_POLL_MS),
        move || {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    NiriMessage::Daemon {
                        msg: DaemonMessage::Toggle,
                        enqueued_at,
                    } => {
                        log::info!(
                            "toggle queue delay: {}ms",
                            enqueued_at.elapsed().as_millis(),
                        );
                        let is_visible = window_for_poll.is_visible();
                        if is_visible {
                            window_for_poll.set_visible(false);
                            pending_overlay_frame_for_poll.borrow_mut().take();
                        } else {
                            let open_start = Instant::now();
                            let snapshot_start = Instant::now();
                            let agent_sessions = overlay_snapshot_from_cache(&cache_for_poll);
                            let snapshot_elapsed = snapshot_start.elapsed();
                            dirty_state_for_poll.clear_all();

                            let mut state = state_for_poll.borrow_mut();
                            state.agent_sessions = agent_sessions;
                            state.focused_at_open = *focused_window_for_poll.lock().unwrap();
                            // First pass: natural label widths for size computation
                            let first_rebuild_start = Instant::now();
                            rebuild_for_poll(&main_box_for_poll, &state, false);
                            let first_rebuild_elapsed = first_rebuild_start.elapsed();
                            let wide_agents_layout = uses_wide_layout(&state);
                            drop(state);
                            reset_overlay_scroll(&scroller_for_poll);
                            let resize_start = Instant::now();
                            update_overlay_size(
                                &window_for_poll,
                                &scroller_for_poll,
                                &outer_box_for_poll,
                                wide_agents_layout,
                            );
                            let resize_elapsed = resize_start.elapsed();
                            // Second pass: locked label widths
                            let state = state_for_poll.borrow();
                            let second_rebuild_start = Instant::now();
                            rebuild_for_poll(&main_box_for_poll, &state, true);
                            let second_rebuild_elapsed = second_rebuild_start.elapsed();
                            drop(state);
                            *pending_overlay_frame_for_poll.borrow_mut() =
                                Some(PendingOverlayFrame {
                                    presented_at: Instant::now(),
                                });
                            window_for_poll.set_visible(true);
                            window_for_poll.present();
                            let total_elapsed = open_start.elapsed();
                            log::info!(
                                "overlay open: {}ms snapshot={}ms rebuild1={}ms resize={}ms rebuild2={}ms",
                                total_elapsed.as_millis(),
                                snapshot_elapsed.as_millis(),
                                first_rebuild_elapsed.as_millis(),
                                resize_elapsed.as_millis(),
                                second_rebuild_elapsed.as_millis(),
                            );
                            let config = state_for_poll.borrow().config.clone();
                            request_workspace_refresh(tx_for_poll.clone(), config);
                        }
                    }
                    NiriMessage::ReloadConfig => {
                        let mut state = state_for_poll.borrow_mut();
                        let reloaded = match config::load_config() {
                            Ok(config) => {
                                state.theme = themes::get(&config.theme);
                                apply_theme_css(&css_provider_for_poll, state.theme);
                                state.config = config;
                                state.last_config_error = None;
                                debug!("config reloaded");
                                true
                            }
                            Err(err) => {
                                let should_notify =
                                    state.last_config_error.as_deref() != Some(err.as_str());
                                if should_notify {
                                    notify_config_error(&err);
                                }
                                state.last_config_error = Some(err);
                                false
                            }
                        };

                        if reloaded && window_for_poll.is_visible() {
                            rebuild_for_poll(&main_box_for_poll, &state, false);
                            let wide_agents_layout = uses_wide_layout(&state);
                            let config = state.config.clone();
                            drop(state);
                            update_overlay_size(
                                &window_for_poll,
                                &scroller_for_poll,
                                &outer_box_for_poll,
                                wide_agents_layout,
                            );
                            let state = state_for_poll.borrow();
                            rebuild_for_poll(&main_box_for_poll, &state, true);
                            reset_overlay_scroll(&scroller_for_poll);
                            drop(state);
                            request_workspace_refresh(tx_for_poll.clone(), config);
                        }
                    }
                    NiriMessage::WorkspaceColumns {
                        entries,
                        enqueued_at,
                    } => {
                        log::debug!(
                            "workspace refresh queue delay: {}ms entries={}",
                            enqueued_at.elapsed().as_millis(),
                            entries.len(),
                        );
                        let mut state = state_for_poll.borrow_mut();
                        let needs_rebuild = window_for_poll.is_visible()
                            && workspace_entries_changed(&state.agent_entries, &entries);
                        state.agent_entries = entries;
                        if needs_rebuild {
                            let apply_start = Instant::now();
                            rebuild_for_poll(&main_box_for_poll, &state, false);
                            let wide_agents_layout = uses_wide_layout(&state);
                            drop(state);
                            update_overlay_size(
                                &window_for_poll,
                                &scroller_for_poll,
                                &outer_box_for_poll,
                                wide_agents_layout,
                            );
                            let state = state_for_poll.borrow();
                            rebuild_for_poll(&main_box_for_poll, &state, true);
                            let elapsed = apply_start.elapsed();
                            log::debug!(
                                "workspace refresh apply: {}ms entries={}",
                                elapsed.as_millis(),
                                state.agent_entries.len(),
                            );
                        }
                    }
                    NiriMessage::Daemon {
                        msg: DaemonMessage::SessionsChanged,
                        enqueued_at,
                    } => {
                        log::debug!(
                            "sessions-changed queue delay: {}ms",
                            enqueued_at.elapsed().as_millis(),
                        );
                        let agent_sessions = overlay_snapshot_from_cache(&cache_for_poll);
                        let mut state = state_for_poll.borrow_mut();
                        state.agent_sessions = agent_sessions;
                        if window_for_poll.is_visible() {
                            rebuild_for_poll(&main_box_for_poll, &state, false);
                            let wide_agents_layout = uses_wide_layout(&state);
                            drop(state);
                            update_overlay_size(
                                &window_for_poll,
                                &scroller_for_poll,
                                &outer_box_for_poll,
                                wide_agents_layout,
                            );
                            let state = state_for_poll.borrow();
                            rebuild_for_poll(&main_box_for_poll, &state, true);
                        }
                    }
                    NiriMessage::Daemon {
                        msg: DaemonMessage::Track(_),
                        ..
                    }
                    | NiriMessage::Daemon {
                        msg: DaemonMessage::List(_),
                        ..
                    } => {}
                    NiriMessage::Daemon {
                        msg: DaemonMessage::Shutdown,
                        ..
                    } => {
                        // Exit GTK app
                        std::process::exit(0);
                    }
                }
            }

            if window_for_poll.is_visible() {
                let sessions_dirty = dirty_state_for_poll.take_sessions();
                if sessions_dirty {
                    let refresh_start = Instant::now();
                    let mut state = state_for_poll.borrow_mut();

                    let snapshot_start = Instant::now();
                    let agent_sessions = overlay_snapshot_from_cache(&cache_for_poll);
                    let snapshot_elapsed = snapshot_start.elapsed();
                    state.agent_sessions = agent_sessions;
                    log::debug!(
                        "dirty refresh snapshot: {}ms sessions_dirty=true",
                        snapshot_elapsed.as_millis(),
                    );

                    rebuild_for_poll(&main_box_for_poll, &state, false);
                    let wide_agents_layout = uses_wide_layout(&state);
                    drop(state);
                    update_overlay_size(
                        &window_for_poll,
                        &scroller_for_poll,
                        &outer_box_for_poll,
                        wide_agents_layout,
                    );
                    let state = state_for_poll.borrow();
                    rebuild_for_poll(&main_box_for_poll, &state, true);
                    log::debug!(
                        "dirty refresh apply: {}ms sessions_dirty=true",
                        refresh_start.elapsed().as_millis(),
                    );
                }
            }
            glib::ControlFlow::Continue
        },
    );
}

fn format_duration(state_updated: f64) -> String {
    let elapsed = (state::now() - state_updated).max(0.0) as u64;
    if elapsed < 60 {
        format!("{}s", elapsed)
    } else if elapsed < 3600 {
        format!("{}m", elapsed / 60)
    } else {
        let h = elapsed / 3600;
        let m = (elapsed % 3600) / 60;
        if m == 0 {
            format!("{}h", h)
        } else {
            format!("{}h{}m", h, m)
        }
    }
}

struct AgentInfo {
    agent: String,
    state: AgentState,
    state_updated: Option<f64>,
    title: Option<String>,
}

fn agent_info_for_entry(
    entry: &WorkspaceColumn,
    agent_sessions: &HashMap<u64, AgentSession>,
) -> Option<AgentInfo> {
    if let Some(window_id) = entry.window_id
        && let Some(session) = agent_sessions.get(&window_id)
    {
        return Some(AgentInfo {
            agent: session.agent.clone(),
            state: session.state,
            state_updated: Some(session.state_updated),
            title: tracked_session_title(entry, session),
        });
    }

    if let Some(info) = agent_fallback_from_window_title(entry) {
        return Some(info);
    }

    if entry
        .window_title
        .as_deref()
        .is_some_and(is_untracked_claude_title)
    {
        return Some(AgentInfo {
            agent: "claude".to_string(),
            state: AgentState::Idle,
            state_updated: None,
            title: None,
        });
    }

    None
}

/// A window whose title is exactly "Claude Code" (ignoring leading status
/// glyphs like "✳") is a Claude session we have no hook data for yet.
fn is_untracked_claude_title(title: &str) -> bool {
    title
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim()
        == "Claude Code"
}

fn agents_view_has_titles(
    entries: &[WorkspaceColumn],
    agent_sessions: &HashMap<u64, AgentSession>,
) -> bool {
    sorted_agent_entries(entries, agent_sessions)
        .into_iter()
        .any(|entry| {
            agent_info_for_entry(entry, agent_sessions).is_some_and(|info| info.title.is_some())
        })
}

fn uses_wide_layout(state: &AppState) -> bool {
    agents_view_has_titles(&state.agent_entries, &state.agent_sessions)
}

#[allow(clippy::too_many_arguments)]
fn build_agents_list(
    container: &GtkBox,
    entries: &[WorkspaceColumn],
    agent_sessions: &HashMap<u64, AgentSession>,
    focused_window_id: Option<u64>,
    theme: &themes::Theme,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let agent_entries = sorted_agent_entries(entries, agent_sessions);

    let jump_target_id = agent_entries
        .iter()
        .find(|e| e.window_id != focused_window_id)
        .and_then(|e| e.window_id);
    let show_title_column = agent_entries.iter().any(|entry| {
        agent_info_for_entry(entry, agent_sessions).is_some_and(|info| info.title.is_some())
    });

    let grid = Grid::new();
    grid.set_column_spacing(14);
    grid.set_row_spacing(6);
    grid.set_halign(gtk4::Align::Start);

    for (row, entry) in agent_entries.iter().enumerate() {
        let row = row as i32;
        let selection_key = agent_selection_key_for_entry(entry, &agent_entries, focused_window_id);

        let is_current = entry.window_id == focused_window_id;
        let is_jump_target = entry.window_id.is_some() && entry.window_id == jump_target_id;

        let marker = if is_current {
            "·"
        } else if is_jump_target {
            "▸"
        } else {
            ""
        };
        let marker_label = Label::new(Some(marker));
        marker_label.add_css_class("key");
        grid.attach(&marker_label, 0, row, 1, 1);

        let key_text = selection_key
            .map(|key| format!("[{key}]"))
            .unwrap_or_else(|| "   ".to_string());
        let key_label = Label::new(Some(&key_text));
        key_label.add_css_class("key");
        grid.attach(&key_label, 1, row, 1, 1);

        let ws_label = Label::new(Some(&entry.workspace_name));
        ws_label.add_css_class("workspace-title");
        ws_label.set_xalign(0.0);
        grid.attach(&ws_label, 2, row, 1, 1);

        if let Some(info) = agent_info_for_entry(entry, agent_sessions) {
            let agent_column = if show_title_column { 4 } else { 3 };
            let icon_column = if show_title_column { 5 } else { 4 };
            let duration_column = if show_title_column { 6 } else { 5 };

            if show_title_column {
                let title_text = info.title.clone().unwrap_or_default();
                let title_label = Label::new(Some(&title_text));
                title_label.add_css_class("agent-title");
                title_label.set_xalign(0.0);
                title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                title_label.set_max_width_chars(40);
                grid.attach(&title_label, 3, row, 1, 1);
            }

            let agent_label = Label::new(Some(&info.agent));
            agent_label.add_css_class("workspace-title");
            agent_label.set_xalign(0.0);
            grid.attach(&agent_label, agent_column, row, 1, 1);

            let color = theme.state_color(info.state);
            let icon_label = Label::new(None);
            icon_label.set_markup(&format!(
                "<span color=\"{color}\">{}</span>",
                info.state.icon()
            ));
            grid.attach(&icon_label, icon_column, row, 1, 1);

            if let Some(updated) = info.state_updated {
                let dur_label = Label::new(None);
                dur_label.set_markup(&format!(
                    "<span color=\"{color}\">{}</span>",
                    format_duration(updated)
                ));
                dur_label.set_xalign(1.0);
                grid.attach(&dur_label, duration_column, row, 1, 1);
            }
        }
    }

    container.append(&grid);
}

fn sorted_agent_entries<'a>(
    entries: &'a [WorkspaceColumn],
    agent_sessions: &HashMap<u64, AgentSession>,
) -> Vec<&'a WorkspaceColumn> {
    let mut result: Vec<_> = entries
        .iter()
        .filter(|e| {
            agent_info_for_entry(e, agent_sessions)
                .is_some_and(|info| info.state_updated.is_some() || info.title.is_some())
        })
        .collect();

    result.sort_by(|a, b| {
        let info_a = agent_info_for_entry(a, agent_sessions);
        let info_b = agent_info_for_entry(b, agent_sessions);
        let updated_a = info_a.as_ref().and_then(|i| i.state_updated).unwrap_or(0.0);
        let updated_b = info_b.as_ref().and_then(|i| i.state_updated).unwrap_or(0.0);
        let rank_a = info_a
            .as_ref()
            .map(|info| agent_sort_rank(info, updated_a))
            .unwrap_or(0);
        let rank_b = info_b
            .as_ref()
            .map(|info| agent_sort_rank(info, updated_b))
            .unwrap_or(0);

        rank_b
            .cmp(&rank_a)
            .then_with(|| {
                updated_b
                    .partial_cmp(&updated_a)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.workspace_name.cmp(&b.workspace_name))
            .then_with(|| a.column_index.cmp(&b.column_index))
            .then_with(|| {
                a.window_title
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.window_title.as_deref().unwrap_or(""))
            })
    });

    result
}

fn agent_sort_rank(info: &AgentInfo, state_updated: f64) -> u8 {
    match info.state {
        AgentState::Waiting if state::now() - state_updated <= WAITING_PRIORITY_WINDOW_SECS => 3,
        AgentState::Responding => 2,
        AgentState::Waiting => 1,
        _ => 0,
    }
}

fn agent_selection_key_for_entry(
    entry: &WorkspaceColumn,
    sorted_entries: &[&WorkspaceColumn],
    focused_window_id: Option<u64>,
) -> Option<char> {
    let jump_target_id = sorted_entries
        .iter()
        .find(|candidate| candidate.window_id != focused_window_id)
        .and_then(|candidate| candidate.window_id);

    sorted_entries
        .iter()
        .filter(|candidate| {
            candidate.window_id != focused_window_id && candidate.window_id != jump_target_id
        })
        .position(|candidate| same_workspace_entry(candidate, entry))
        .and_then(agent_selection_key_for_index)
}

fn find_agent_entry_for_selection_key<'a>(
    entries: &'a [WorkspaceColumn],
    agent_sessions: &HashMap<u64, AgentSession>,
    focused_window_id: Option<u64>,
    key: char,
) -> Option<&'a WorkspaceColumn> {
    let sorted = sorted_agent_entries(entries, agent_sessions);

    sorted
        .iter()
        .copied()
        .find(|entry| agent_selection_key_for_entry(entry, &sorted, focused_window_id) == Some(key))
}

#[allow(clippy::too_many_arguments)]
fn find_smart_jump_target<'a>(
    entries: &'a [WorkspaceColumn],
    agent_sessions: &HashMap<u64, AgentSession>,
    focused_window_id: Option<u64>,
) -> Option<&'a WorkspaceColumn> {
    let sorted = sorted_agent_entries(entries, agent_sessions);
    sorted
        .into_iter()
        .find(|e| e.window_id != focused_window_id)
}

/// Run the niri daemon with GTK overlay (new `serve --niri` mode)
pub fn run_with_daemon() -> glib::ExitCode {
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonMessage>();
    let (niri_tx, niri_rx) = mpsc::channel::<NiriMessage>();
    let cache = Arc::new(Mutex::new(SessionCache::new()));
    let focused_window: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let dirty_state = Arc::new(UiDirtyState::default());

    {
        let mut cache = cache.lock().unwrap();
        cache.reload_agent_sessions();
    }

    let _daemon_instance = match daemon::start_socket_listener(daemon_tx.clone(), cache.clone()) {
        Ok(guard) => guard,
        Err(err) => {
            log::error!("Failed to start niri daemon: {}", err);
            return glib::ExitCode::FAILURE;
        }
    };

    log::info!(
        "Starting niri daemon with overlay, listening on {:?}",
        daemon::socket_path()
    );

    daemon::start_sessions_watcher(daemon_tx.clone());

    start_config_watcher(niri_tx.clone());
    start_focus_tracker(focused_window.clone());

    let niri_tx_clone = niri_tx.clone();
    let cache_clone = cache.clone();
    let focused_window_for_bridge = focused_window.clone();
    let dirty_state_for_bridge = dirty_state.clone();
    thread::spawn(move || {
        loop {
            let msg = match daemon_rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            };
            let is_toggle = matches!(msg, DaemonMessage::Toggle);

            let forwarded = match msg {
                DaemonMessage::Track(_) | DaemonMessage::SessionsChanged => {
                    let _ = process_daemon_message(msg, &cache_clone, &focused_window_for_bridge);
                    dirty_state_for_bridge.mark_sessions();
                    None
                }
                other => process_daemon_message(other, &cache_clone, &focused_window_for_bridge),
            };

            if let Some(niri_msg) = forwarded
                && niri_tx_clone.send(niri_msg).is_err()
            {
                break;
            }

            if is_toggle {
                if let Err(err) =
                    refresh_cache_after_cleanup(&cache_clone, load_clean_store_after_cleanup)
                {
                    log::error!("Failed to refresh state after overlay toggle: {}", err);
                } else {
                    dirty_state_for_bridge.mark_sessions();
                }
            }
        }
    });

    let rx = Rc::new(RefCell::new(Some(niri_rx)));
    let focused_window_rc = Rc::new(RefCell::new(Some(focused_window)));
    let cache_rc = Rc::new(RefCell::new(Some(cache)));
    let dirty_state_rc = Rc::new(RefCell::new(Some(dirty_state)));

    let app = Application::builder()
        .application_id(APP_ID)
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let rx_clone = rx.clone();
    let focused_clone = focused_window_rc.clone();
    let cache_clone = cache_rc.clone();
    let dirty_clone = dirty_state_rc.clone();
    app.connect_activate(move |app| {
        if let (Some(rx), Some(focused), Some(cache), Some(dirty_state)) = (
            rx_clone.borrow_mut().take(),
            focused_clone.borrow_mut().take(),
            cache_clone.borrow_mut().take(),
            dirty_clone.borrow_mut().take(),
        ) {
            build_ui(app, rx, niri_tx.clone(), focused, cache, dirty_state);
        }
    });

    app.run_with_args::<&str>(&[])
}

fn mock_workspace_columns() -> Vec<WorkspaceColumn> {
    let projects = [
        ("agent-switch", 2),
        ("wayvoice", 3),
        ("kanel", 2),
        ("dotfiles", 1),
        ("rollout", 2),
        ("website", 1),
        ("infra", 2),
        ("notes", 1),
    ];

    let titles = [
        "ghostty", "claude", "codex", "ghostty", "firefox", "zed", "ghostty", "codex",
    ];

    let mut entries = Vec::new();
    let mut window_id = 100u64;

    for (proj_idx, &(name, num_columns)) in projects.iter().enumerate() {
        for col in 0..num_columns {
            let title_idx = (proj_idx + col) % titles.len();
            entries.push(WorkspaceColumn {
                workspace_name: name.to_string(),
                workspace_ref: WorkspaceReferenceArg::Name(name.to_string()),
                column_index: (col + 2) as u32,
                window_title: Some(titles[title_idx].to_string()),
                window_id: Some(window_id),
            });
            window_id += 1;
        }
    }

    entries
}

fn mock_agent_sessions(entries: &[WorkspaceColumn], cycle: usize) -> HashMap<u64, AgentSession> {
    let states = [
        AgentState::Waiting,
        AgentState::Responding,
        AgentState::Idle,
    ];
    let agents = ["claude", "codex", "opencode"];

    let mut sessions = HashMap::new();

    for (i, entry) in entries.iter().enumerate() {
        let Some(window_id) = entry.window_id else {
            continue;
        };
        // Only some entries have agent sessions
        if i % 3 != 0 && i % 5 != 0 {
            continue;
        }
        let state_idx = (i + cycle) % states.len();
        let agent_idx = i % agents.len();
        sessions.insert(
            window_id,
            AgentSession {
                agent: agents[agent_idx].to_string(),
                session_name: None,
                state: states[state_idx],
                cwd: Some(format!("~/code/{}", entry.workspace_name)),
                state_updated: state::now() - [30.0, 125.0, 3600.0, 45.0, 900.0][i % 5],
            },
        );
    }

    sessions
}

fn build_demo_ui(app: &Application, theme_override: Option<String>) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(OVERLAY_FALLBACK_WIDTH)
        .default_height(OVERLAY_FALLBACK_HEIGHT)
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_anchor(Edge::Top, false);
    window.set_anchor(Edge::Bottom, false);
    window.set_anchor(Edge::Left, false);
    window.set_anchor(Edge::Right, false);

    let mut config = config::load_config().unwrap_or_default();
    if let Some(t) = theme_override {
        config.theme = t;
    }
    let theme = themes::get(&config.theme);
    let agent_entries = mock_workspace_columns();
    let agent_sessions = mock_agent_sessions(&agent_entries, 0);

    let state = Rc::new(RefCell::new(AppState {
        config,
        theme,
        agent_entries,
        focused_at_open: None,
        agent_sessions,
        last_config_error: None,
    }));

    let outer_box = GtkBox::new(Orientation::Vertical, 0);
    outer_box.add_css_class("outer");

    let scroller = ScrolledWindow::new();
    scroller.set_policy(PolicyType::Never, PolicyType::Automatic);
    scroller.set_propagate_natural_width(true);
    scroller.set_propagate_natural_height(true);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);

    let main_box = GtkBox::new(Orientation::Vertical, 10);
    main_box.set_margin_top(20);
    main_box.set_margin_start(28);
    main_box.set_margin_end(28);
    main_box.set_margin_bottom(20);
    main_box.set_halign(gtk4::Align::Fill);
    main_box.set_hexpand(true);
    scroller.set_child(Some(&main_box));

    let rebuild = |main_box: &GtkBox, state: &AppState| {
        build_agents_list(
            main_box,
            &state.agent_entries,
            &state.agent_sessions,
            state.focused_at_open,
            state.theme,
        );
    };

    {
        let s = state.borrow();
        rebuild(&main_box, &s);
    }
    outer_box.append(&scroller);

    load_overlay_css(theme);

    window.set_child(Some(&outer_box));

    // Key handler: q/Escape to quit
    let window_clone = window.clone();

    let key_controller = gtk4::EventControllerKey::new();
    key_controller.connect_key_pressed(move |_, keyval, _, _| {
        let input_char = input_char_for_key(keyval);
        let key_name = keyval.name().map(|s| s.to_lowercase());

        if input_char == Some('q') || key_name.as_deref() == Some("escape") {
            window_clone.close();
        }

        glib::Propagation::Stop
    });

    window.add_controller(key_controller);

    // Cycle agent states every 2 seconds
    let state_for_timer = state.clone();
    let main_box_for_timer = main_box.clone();
    let cycle = Rc::new(RefCell::new(0usize));
    glib::timeout_add_local(std::time::Duration::from_secs(2), move || {
        let mut c = cycle.borrow_mut();
        *c += 1;
        let mut s = state_for_timer.borrow_mut();
        s.agent_sessions = mock_agent_sessions(&s.agent_entries, *c);
        drop(s);
        let s = state_for_timer.borrow();
        rebuild(&main_box_for_timer, &s);
        glib::ControlFlow::Continue
    });

    // First pass used natural widths for sizing; now compute overlay size
    let wide = uses_wide_layout(&state.borrow());
    update_overlay_size(&window, &scroller, &outer_box, wide);

    // Second pass: lock label widths so content changes don't shift layout
    {
        let s = state.borrow();
        rebuild(&main_box, &s);
    }

    window.present();
}

pub fn run_demo(theme_override: Option<&str>) -> glib::ExitCode {
    let app = Application::builder()
        .application_id(format!("{APP_ID}.demo"))
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let theme_override = theme_override.map(|s| s.to_string());
    app.connect_activate(move |app| build_demo_ui(app, theme_override.clone()));
    app.run_with_args::<&str>(&[])
}

pub fn run_toggle() -> glib::ExitCode {
    if let Err(e) = daemon::send_toggle_request() {
        log::error!("Failed to toggle agents: {} (is daemon running?)", e);
        std::process::exit(1);
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn workspace_entry(name: &str, window_id: u64) -> WorkspaceColumn {
        workspace_entry_with_window(name, window_id, "Claude Code")
    }

    fn workspace_entry_with_window(
        name: &str,
        window_id: u64,
        window_title: &str,
    ) -> WorkspaceColumn {
        WorkspaceColumn {
            workspace_name: name.to_string(),
            workspace_ref: WorkspaceReferenceArg::Name(name.to_string()),
            column_index: 2,
            window_title: Some(window_title.to_string()),
            window_id: Some(window_id),
        }
    }

    #[test]
    fn unnamed_workspaces_are_ignored_by_default() {
        let config: config::Config = toml::from_str("").expect("default config should parse");
        let seen = HashSet::new();

        assert!(should_skip_discovered_workspace(
            None, // unnamed workspace
            "3",  // fallback display name from index
            &config, &seen,
        ));
    }

    #[test]
    fn ignore_numeric_sessions_hides_numeric_named_workspaces() {
        let config: config::Config = toml::from_str(
            r#"
ignoreUnnamedWorkspaces = false
ignoreNumericSessions = true
"#,
        )
        .expect("config should parse");
        let seen = HashSet::new();

        assert!(should_skip_discovered_workspace(
            Some("12"),
            "12",
            &config,
            &seen,
        ));
        assert!(!should_skip_discovered_workspace(
            Some("web"),
            "web",
            &config,
            &seen,
        ));
    }

    #[test]
    fn ignore_list_and_seen_workspace_names_are_filtered() {
        let config: config::Config = toml::from_str(
            r#"
ignoreUnnamedWorkspaces = false
ignore = ["web"]
"#,
        )
        .expect("config should parse");

        let mut seen = HashSet::new();
        seen.insert("company".to_string());

        assert!(should_skip_discovered_workspace(
            Some("web"),
            "web",
            &config,
            &seen,
        ));
        assert!(should_skip_discovered_workspace(
            Some("company"),
            "company",
            &config,
            &seen,
        ));
        assert!(!should_skip_discovered_workspace(
            Some("agent-switch"),
            "agent-switch",
            &config,
            &seen,
        ));
    }

    #[test]
    fn named_agent_titles_are_extracted_for_claude_and_pi() {
        assert_eq!(
            named_claude_title("✳ debug-helium-launch-issues").as_deref(),
            Some("debug-helium-launch-issues")
        );
        assert_eq!(named_claude_title("✳ Claude Code"), None);
        assert_eq!(
            named_pi_title("π - test-foo - agent-switch").as_deref(),
            Some("test-foo")
        );
    }

    #[test]
    fn agent_info_uses_session_name_for_tracked_session() {
        let entry = workspace_entry_with_window("dotfiles", 292, "✳ debug-helium-launch-issues");
        let agent_sessions = HashMap::from([(
            292,
            AgentSession {
                agent: "claude".to_string(),
                session_name: Some("review-fixes".to_string()),
                state: AgentState::Responding,
                cwd: Some("/tmp/dotfiles".to_string()),
                state_updated: 42.0,
            },
        )]);

        let info = agent_info_for_entry(&entry, &agent_sessions)
            .expect("tracked claude session should be detected");

        assert_eq!(info.agent, "claude");
        assert_eq!(info.title.as_deref(), Some("review-fixes"));
    }

    #[test]
    fn agent_info_hides_redundant_title_matching_workspace_name() {
        let entry = workspace_entry_with_window("dotfiles", 292, "✳ dotfiles");
        let agent_sessions = HashMap::from([(
            292,
            AgentSession {
                agent: "claude".to_string(),
                session_name: None,
                state: AgentState::Responding,
                cwd: Some("/tmp/dotfiles".to_string()),
                state_updated: 42.0,
            },
        )]);

        let info = agent_info_for_entry(&entry, &agent_sessions)
            .expect("tracked claude session should be detected");

        assert_eq!(info.agent, "claude");
        assert_eq!(info.title, None);
        assert!(!agents_view_has_titles(
            std::slice::from_ref(&entry),
            &agent_sessions,
        ));
    }

    #[test]
    fn pi_window_title_fallback_does_not_show_title_without_tracking() {
        let entry = workspace_entry_with_window("agent-switch", 293, "π - test-foo - agent-switch");

        let info = agent_info_for_entry(&entry, &HashMap::new())
            .expect("pi title should be treated as an agent window");
        assert_eq!(info.agent, "pi");
        assert_eq!(info.title, None);
        assert_eq!(info.state, AgentState::Idle);
    }

    #[test]
    fn agents_view_only_uses_wide_layout_when_titles_are_present() {
        let untitled = workspace_entry("kanel-backend", 148);
        let titled =
            workspace_entry_with_window("agent-switch", 293, "π - test-foo - agent-switch");
        let agent_sessions = HashMap::from([(
            148,
            AgentSession {
                agent: "claude".to_string(),
                session_name: None,
                state: AgentState::Idle,
                cwd: Some("/tmp/kanel-backend".to_string()),
                state_updated: 42.0,
            },
        )]);

        assert!(!agents_view_has_titles(
            std::slice::from_ref(&untitled),
            &agent_sessions,
        ));
        assert!(!agents_view_has_titles(
            std::slice::from_ref(&titled),
            &HashMap::new(),
        ));
    }

    #[test]
    fn punctuation_bindings_map_to_selection_keys() {
        let comma = gtk4::gdk::Key::from_name("comma").expect("comma key should exist");
        let period = gtk4::gdk::Key::from_name("period").expect("period key should exist");

        assert_eq!(input_char_for_key(comma), Some(','));
        assert_eq!(selection_key_for_input(comma), Some(','));
        assert_eq!(selection_key_for_input(period), Some('.'));
    }

    #[test]
    fn overlay_size_caps_allow_compact_windows() {
        let (max_width, max_height) = overlay_size_caps_for_geometry(2560, 1440, false);
        let compact_width = clamp_i32(380, OVERLAY_MIN_WIDTH.min(max_width), max_width);
        let compact_height = clamp_i32(170, 1, max_height);

        assert_eq!(compact_width, 380);
        assert_eq!(compact_height, 170);
        assert!(compact_width < max_width);
        assert!(compact_height < max_height);
    }

    #[test]
    fn overlay_size_caps_expand_for_wide_agents_layout() {
        let regular = overlay_size_caps_for_geometry(2560, 1440, false);
        let agents = overlay_size_caps_for_geometry(2560, 1440, true);

        assert!(agents.0 > regular.0);
        assert!(agents.1 > regular.1);
    }

    #[test]
    fn sorted_agent_entries_demotes_stale_waiting_below_responding() {
        let entries = vec![
            workspace_entry("fresh-waiting", 1),
            workspace_entry("responding", 2),
            workspace_entry("stale-waiting", 3),
        ];
        let now = state::now();
        let agent_sessions = HashMap::from([
            (
                1,
                AgentSession {
                    agent: "claude".to_string(),
                    session_name: None,
                    state: AgentState::Waiting,
                    cwd: Some("/tmp/fresh".to_string()),
                    state_updated: now - 60.0,
                },
            ),
            (
                2,
                AgentSession {
                    agent: "claude".to_string(),
                    session_name: None,
                    state: AgentState::Responding,
                    cwd: Some("/tmp/responding".to_string()),
                    state_updated: now - 120.0,
                },
            ),
            (
                3,
                AgentSession {
                    agent: "claude".to_string(),
                    session_name: None,
                    state: AgentState::Waiting,
                    cwd: Some("/tmp/stale".to_string()),
                    state_updated: now - WAITING_PRIORITY_WINDOW_SECS - 1.0,
                },
            ),
        ]);

        let sorted = sorted_agent_entries(&entries, &agent_sessions);
        let ids: Vec<_> = sorted
            .into_iter()
            .filter_map(|entry| entry.window_id)
            .collect();

        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn agent_selection_keys_skip_current_and_space_target() {
        let entries = vec![
            workspace_entry("current", 1),
            workspace_entry("space-target", 2),
            workspace_entry("first-direct", 3),
            workspace_entry("second-direct", 4),
        ];
        let now = state::now();
        let agent_sessions = HashMap::from([
            (
                1,
                AgentSession {
                    agent: "claude".to_string(),
                    session_name: None,
                    state: AgentState::Responding,
                    cwd: Some("/tmp/current".to_string()),
                    state_updated: now - 30.0,
                },
            ),
            (
                2,
                AgentSession {
                    agent: "claude".to_string(),
                    session_name: None,
                    state: AgentState::Responding,
                    cwd: Some("/tmp/space-target".to_string()),
                    state_updated: now - 60.0,
                },
            ),
            (
                3,
                AgentSession {
                    agent: "claude".to_string(),
                    session_name: None,
                    state: AgentState::Responding,
                    cwd: Some("/tmp/first-direct".to_string()),
                    state_updated: now - 90.0,
                },
            ),
            (
                4,
                AgentSession {
                    agent: "claude".to_string(),
                    session_name: None,
                    state: AgentState::Responding,
                    cwd: Some("/tmp/second-direct".to_string()),
                    state_updated: now - 120.0,
                },
            ),
        ]);
        let sorted = sorted_agent_entries(&entries, &agent_sessions);

        assert_eq!(
            agent_selection_key_for_entry(sorted[0], &sorted, Some(1)),
            None,
            "the focused window is not directly selectable"
        );
        assert_eq!(
            agent_selection_key_for_entry(sorted[1], &sorted, Some(1)),
            None,
            "space owns the smart-jump target"
        );
        assert_eq!(
            agent_selection_key_for_entry(sorted[2], &sorted, Some(1)),
            Some('h')
        );
        assert_eq!(
            agent_selection_key_for_entry(sorted[3], &sorted, Some(1)),
            Some('j')
        );

        assert_eq!(
            find_agent_entry_for_selection_key(&entries, &agent_sessions, Some(1), 'h')
                .and_then(|entry| entry.window_id),
            Some(3)
        );
        assert_eq!(
            find_agent_entry_for_selection_key(&entries, &agent_sessions, Some(1), 'j')
                .and_then(|entry| entry.window_id),
            Some(4)
        );
    }

    #[test]
    fn process_daemon_message_answers_list_without_forwarding_to_gtk() {
        let cache = Arc::new(Mutex::new(SessionCache::new()));
        cache.lock().unwrap().store.sessions.insert(
            "42".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-42".to_string(),
                session_name: None,
                cwd: Some("/tmp/project".to_string()),
                state: state::SessionState::Idle,
                state_updated: 42.0,
                waiting_reason: None,
                transcript_path: None,
                window: state::WindowId {
                    niri_id: Some("42".to_string()),
                },
            },
        );
        let focused_window = Arc::new(Mutex::new(None));
        let (resp_tx, resp_rx) = mpsc::channel();

        let forwarded =
            process_daemon_message(DaemonMessage::List(resp_tx), &cache, &focused_window);

        assert!(forwarded.is_none());
        let response = resp_rx.recv().expect("list response should be sent");
        assert_eq!(response.sessions.len(), 1);
        assert_eq!(response.sessions[0].session_id, "session-42");
    }

    #[test]
    fn agent_sessions_from_store_prefers_niri_session_key_for_niri_only_sessions() {
        let mut store = state::SessionStore::default();
        store.sessions.insert(
            "122".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-122".to_string(),
                session_name: None,
                cwd: Some("/tmp/project".to_string()),
                state: state::SessionState::Responding,
                state_updated: 42.0,
                waiting_reason: None,
                transcript_path: None,
                window: state::WindowId {
                    niri_id: Some("56".to_string()),
                },
            },
        );

        let sessions = agent_sessions_from_store(&store);

        assert!(sessions.contains_key(&122));
        assert!(!sessions.contains_key(&56));
    }

    #[test]
    fn process_daemon_message_forwards_toggle_to_gtk() {
        let cache = Arc::new(Mutex::new(SessionCache::new()));
        let focused_window = Arc::new(Mutex::new(None));

        let forwarded = process_daemon_message(DaemonMessage::Toggle, &cache, &focused_window);

        assert!(matches!(
            forwarded,
            Some(NiriMessage::Daemon {
                msg: DaemonMessage::Toggle,
                ..
            })
        ));
    }

    #[test]
    fn ui_dirty_state_coalesces_updates_until_taken() {
        let dirty = UiDirtyState::default();

        dirty.mark_sessions();
        dirty.mark_sessions();

        assert!(dirty.take_sessions());
        assert!(!dirty.take_sessions());

        dirty.mark_sessions();
        dirty.clear_all();

        assert!(!dirty.take_sessions());
    }

    #[test]
    fn refresh_cache_after_cleanup_updates_cache() {
        let cache = Arc::new(Mutex::new(SessionCache::new()));
        let mut refreshed_store = state::SessionStore::default();
        refreshed_store.sessions.insert(
            "42".to_string(),
            state::Session {
                agent: "claude".to_string(),
                session_id: "session-42".to_string(),
                session_name: None,
                cwd: Some("/tmp/project".to_string()),
                state: state::SessionState::Responding,
                state_updated: 42.0,
                waiting_reason: None,
                transcript_path: None,
                window: state::WindowId {
                    niri_id: Some("42".to_string()),
                },
            },
        );

        refresh_cache_after_cleanup(&cache, || Ok(refreshed_store))
            .expect("cleanup refresh should succeed");
        let agent_sessions = overlay_snapshot_from_cache(&cache);
        assert_eq!(
            agent_sessions.get(&42).map(|session| session.state),
            Some(AgentState::Responding)
        );
    }
}
