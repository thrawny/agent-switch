use crate::daemon::{
    self, AgentSession, AgentState, CodexSession, DaemonMessage, SessionCache, TrackEvent,
};
use crate::state;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Label, Orientation, PolicyType, ScrolledWindow,
    Separator, glib,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use niri_ipc::{
    Action, Event, Request, Response, Window, Workspace, WorkspaceReferenceArg, socket::Socket,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

const APP_ID: &str = "com.thrawny.agent-switch";
const KEYS: [char; 12] = ['h', 'j', 'k', 'l', 'u', 'i', 'o', 'p', 'n', 'm', ',', '.'];
const NIRI_OVERLAY_WIDTH_RATIO: f64 = 0.45;
const NIRI_OVERLAY_HEIGHT_RATIO: f64 = 0.70;
const NIRI_OVERLAY_MIN_WIDTH: i32 = 340;
const NIRI_OVERLAY_MAX_WIDTH: i32 = 1100;
const NIRI_OVERLAY_MAX_HEIGHT: i32 = 900;
const NIRI_OVERLAY_FALLBACK_WIDTH: i32 = 420;
const NIRI_OVERLAY_FALLBACK_HEIGHT: i32 = 520;
const NIRI_OVERLAY_MARGIN: i32 = 80;
const NIRI_OVERLAY_STEP_SCROLL: f64 = 64.0;
const NIRI_OVERLAY_PAGE_SCROLL: f64 = 320.0;

// Use DaemonMessage as base, add niri-specific ReloadConfig
#[derive(Debug)]
enum NiriMessage {
    Daemon(DaemonMessage),
    ReloadConfig,
}

#[derive(Debug, Clone, Deserialize)]
struct Project {
    #[allow(dead_code)]
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default = "default_project_dir")]
    dir: String,
    #[serde(default)]
    static_workspace: bool,
    #[serde(default = "default_true", alias = "skip_first_column")]
    skip_first_column: bool,
}

fn default_true() -> bool {
    true
}

fn default_project_dir() -> String {
    "~/".to_string()
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    project: Vec<Project>,
    #[serde(default)]
    ignore: Vec<String>,
    #[serde(default, alias = "codexAliases", alias = "codex_aliases")]
    codex_aliases: Vec<String>,
    #[serde(
        default = "default_ignore_unnamed_workspaces",
        alias = "ignoreUnnamedWorkspaces",
        alias = "ignore_unnamed",
        alias = "ignore_unnamed_workspaces"
    )]
    ignore_unnamed_workspaces: bool,
    #[serde(
        default = "default_ignore_numeric_sessions",
        alias = "ignoreNumericSessions",
        alias = "ignore_numeric_sessions"
    )]
    ignore_numeric_sessions: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            project: Vec::new(),
            ignore: Vec::new(),
            codex_aliases: Vec::new(),
            ignore_unnamed_workspaces: default_ignore_unnamed_workspaces(),
            ignore_numeric_sessions: default_ignore_numeric_sessions(),
        }
    }
}

fn default_ignore_unnamed_workspaces() -> bool {
    true
}

fn default_ignore_numeric_sessions() -> bool {
    false
}

#[derive(Debug, Clone)]
struct WorkspaceColumn {
    workspace_name: String,
    workspace_ref: WorkspaceReferenceArg,
    workspace_key: char,
    column_index: u32,
    column_key: char,
    app_label: String,
    window_title: Option<String>,
    dir: Option<String>,
    static_workspace: bool,
    window_id: Option<u64>,
}

struct AppState {
    config: Config,
    codex_aliases: Vec<String>,
    entries: Vec<WorkspaceColumn>,
    pending_key: Option<char>,
    agent_sessions: HashMap<u64, AgentSession>,
    codex_sessions: HashMap<String, CodexSession>,
    last_config_error: Option<String>,
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("projects.toml")
}

fn load_config() -> Result<Config, String> {
    let path = config_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(err) => {
            return Err(format!("Failed to read {}: {}", path.display(), err));
        }
    };

    toml::from_str::<Config>(&content)
        .map_err(|err| format!("Failed to parse {}: {}", path.display(), err))
}

fn notify_config_error(message: &str) {
    log::warn!("{}", message);
    let _ = Command::new("notify-send")
        .args(["agent-switch: projects.toml error", message])
        .status();
}

fn load_agent_sessions() -> HashMap<u64, AgentSession> {
    let store = state::load();
    let mut sessions = HashMap::new();

    for (_, session) in store.sessions.iter() {
        let window_id = match session.window.niri_id.as_ref() {
            Some(id) => id.parse::<u64>().ok(),
            None => continue,
        };
        let Some(window_id) = window_id else { continue };

        sessions.insert(
            window_id,
            AgentSession {
                agent: session.agent.clone(),
                state: AgentState::from_str(&session.state),
                cwd: session.cwd.clone(),
            },
        );
    }
    sessions
}

fn normalized_codex_aliases(config_aliases: &[String]) -> Vec<String> {
    let mut aliases = vec!["codex".to_string()];
    for alias in config_aliases {
        let trimmed = alias.trim();
        if trimmed.is_empty() {
            continue;
        }
        if aliases
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(trimmed))
        {
            continue;
        }
        aliases.push(trimmed.to_string());
    }
    aliases
}

fn window_title_matches_codex_aliases(title: &str, aliases: &[String]) -> bool {
    title
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
        .filter(|token| !token.is_empty())
        .any(|token| {
            aliases
                .iter()
                .any(|alias| !alias.is_empty() && token.eq_ignore_ascii_case(alias))
        })
}

fn codex_state_for_entry(
    entry: &WorkspaceColumn,
    codex_by_cwd: &HashMap<String, CodexSession>,
    codex_aliases: &[String],
) -> Option<AgentState> {
    let title = entry.window_title.as_deref()?.trim();
    if !window_title_matches_codex_aliases(title, codex_aliases) {
        return None;
    }
    let dir = entry.dir.as_deref()?;
    let dir = shellexpand::tilde(dir).to_string();
    daemon::match_codex_by_dir(&dir, codex_by_cwd).map(|entry| entry.state)
}

fn niri_request(request: Request) -> Option<Response> {
    let mut socket = Socket::connect().ok()?;
    match socket.send(request) {
        Ok(Ok(response)) => Some(response),
        _ => None,
    }
}

fn niri_action(action: Action) {
    let _ = niri_request(Request::Action(action));
}

fn niri_workspaces() -> Vec<Workspace> {
    match niri_request(Request::Workspaces) {
        Some(Response::Workspaces(workspaces)) => workspaces,
        _ => Vec::new(),
    }
}

fn niri_windows() -> Vec<Window> {
    match niri_request(Request::Windows) {
        Some(Response::Windows(windows)) => windows,
        _ => Vec::new(),
    }
}

fn get_workspace_by_name(name: &str) -> Option<Workspace> {
    niri_workspaces()
        .into_iter()
        .find(|ws| ws.name.as_deref() == Some(name))
}

fn simplify_label(title: &str, app_id: &str) -> String {
    if app_id.contains("ghostty") || app_id.contains("terminal") || app_id.contains("alacritty") {
        let cleaned = title
            .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '~' && c != '/')
            .trim();
        if cleaned.starts_with('~') {
            let last = cleaned.split('/').next_back().unwrap_or(cleaned);
            format!("~/{}", last)
        } else if cleaned.starts_with('/') {
            cleaned
                .split('/')
                .next_back()
                .unwrap_or(cleaned)
                .to_string()
        } else {
            cleaned.to_string()
        }
    } else {
        app_id.split('.').next_back().unwrap_or(app_id).to_string()
    }
}

fn project_workspace_name(project: &Project) -> String {
    if let Some(name) = project.name.as_deref().map(str::trim)
        && !name.is_empty()
    {
        return name.to_string();
    }

    let expanded_dir = shellexpand::tilde(&project.dir).to_string();
    Path::new(&expanded_dir)
        .file_name()
        .and_then(|v| v.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| project.dir.clone())
}

fn is_numeric_name(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
}

fn should_skip_discovered_workspace(
    name_opt: Option<&str>,
    display_name: &str,
    config: &Config,
    seen_workspaces: &std::collections::HashSet<String>,
) -> bool {
    (name_opt.is_none() && config.ignore_unnamed_workspaces)
        || (config.ignore_numeric_sessions && is_numeric_name(display_name))
        || seen_workspaces.contains(display_name)
        || config.ignore.iter().any(|ignored| ignored == display_name)
}

fn configured_projects(config: &Config) -> Vec<&Project> {
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

fn get_workspace_columns(config: &Config) -> Vec<WorkspaceColumn> {
    use std::collections::{BTreeMap, HashSet};

    let workspaces = niri_workspaces();
    let windows = niri_windows();

    let mut entries = Vec::new();
    let mut seen_workspaces: HashSet<String> = HashSet::new();
    let mut key_idx = 0;

    let add_workspace_entries = |entries: &mut Vec<WorkspaceColumn>,
                                 ws_id: u64,
                                 ws_name: &str,
                                 workspace_ref: WorkspaceReferenceArg,
                                 workspace_key: char,
                                 dir: Option<String>,
                                 static_workspace: bool,
                                 skip_first_column: bool,
                                 windows_arr: &[&Window]| {
        let min_col: usize = if skip_first_column { 2 } else { 1 };
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

        let has_columns = columns.keys().any(|&idx| idx >= min_col);

        if has_columns {
            for (&col_idx, col_windows) in &columns {
                if col_idx < min_col {
                    continue;
                }
                let key_offset = col_idx - min_col;
                if key_offset >= KEYS.len() {
                    continue;
                }
                let column_key = KEYS[key_offset];

                let first_window = col_windows.first();
                let title = first_window.and_then(|w| w.title.as_deref()).unwrap_or("?");
                let app_id = first_window
                    .and_then(|w| w.app_id.as_deref())
                    .unwrap_or("?");
                let window_id = first_window.map(|w| w.id);
                let window_title = first_window.and_then(|w| w.title.clone());
                let app_label = simplify_label(title, app_id);

                entries.push(WorkspaceColumn {
                    workspace_name: ws_name.to_string(),
                    workspace_ref: workspace_ref.clone(),
                    workspace_key,
                    column_index: col_idx as u32,
                    column_key,
                    app_label,
                    window_title,
                    dir: dir.clone(),
                    static_workspace,
                    window_id,
                });
            }
        } else {
            entries.push(WorkspaceColumn {
                workspace_name: ws_name.to_string(),
                workspace_ref: workspace_ref.clone(),
                workspace_key,
                column_index: 2,
                column_key: KEYS[0],
                app_label: "(empty)".to_string(),
                window_title: None,
                dir: dir.clone(),
                static_workspace,
                window_id: None,
            });
        }
    };

    let windows_refs: Vec<&Window> = windows.iter().collect();

    for project in configured_projects(config) {
        if key_idx >= KEYS.len() {
            break;
        }

        let project_name = project_workspace_name(project);
        seen_workspaces.insert(project_name.clone());
        let workspace_key = KEYS[key_idx];

        let ws_id = workspaces
            .iter()
            .find(|ws| ws.name.as_deref() == Some(project_name.as_str()))
            .map(|ws| ws.id);

        if let Some(ws_id) = ws_id {
            add_workspace_entries(
                &mut entries,
                ws_id,
                &project_name,
                WorkspaceReferenceArg::Name(project_name.clone()),
                workspace_key,
                Some(project.dir.clone()),
                project.static_workspace,
                project.skip_first_column,
                &windows_refs,
            );
        } else {
            entries.push(WorkspaceColumn {
                workspace_name: project_name.clone(),
                workspace_ref: WorkspaceReferenceArg::Name(project_name),
                workspace_key,
                column_index: 2,
                column_key: KEYS[0],
                app_label: "(empty)".to_string(),
                window_title: None,
                dir: Some(project.dir.clone()),
                static_workspace: project.static_workspace,
                window_id: None,
            });
        }

        key_idx += 1;
    }

    let mut remaining: Vec<_> = workspaces
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

            let workspace_ref = match name_opt {
                Some(n) => WorkspaceReferenceArg::Name(n.to_string()),
                None => WorkspaceReferenceArg::Index(idx),
            };

            Some((idx, ws_id, display_name, workspace_ref))
        })
        .collect();

    remaining.sort_by_key(|(idx, _, _, _)| *idx);

    for (_, ws_id, display_name, workspace_ref) in remaining {
        if key_idx >= KEYS.len() {
            break;
        }

        let workspace_key = KEYS[key_idx];

        add_workspace_entries(
            &mut entries,
            ws_id,
            &display_name,
            workspace_ref,
            workspace_key,
            None,
            true,
            true,
            &windows_refs,
        );

        key_idx += 1;
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

fn focus_window(id: u64) -> bool {
    matches!(
        niri_request(Request::Action(Action::FocusWindow { id })),
        Some(Response::Handled)
    )
}

fn spawn_terminals(dir: &str) {
    let dir = shellexpand::tilde(dir).to_string();

    // Prefer compositor-side spawn to avoid inheriting daemon/zmx env vars.
    let spawned_via_niri = matches!(
        niri_request(Request::Action(Action::Spawn {
            command: vec![
                "ghostty".to_string(),
                format!("--working-directory={}", dir)
            ],
        })),
        Some(Response::Handled)
    );
    if spawned_via_niri {
        return;
    }

    // Fallback path for environments where niri IPC spawn is unavailable.
    let mut command = Command::new("ghostty");
    command
        .arg(format!("--working-directory={}", dir))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TMUX_TMPDIR");
    for (key, _) in std::env::vars() {
        if key.starts_with("ZMX_") {
            command.env_remove(key);
        }
    }
    let _ = command.spawn();
}

fn create_workspace(name: &str, dir: Option<&str>) {
    if get_workspace_by_name(name).is_some() {
        focus_workspace(WorkspaceReferenceArg::Name(name.to_string()));
    } else {
        let max_idx = niri_workspaces().iter().map(|ws| ws.idx).max().unwrap_or(0);
        let new_idx = max_idx.saturating_add(1);
        focus_workspace(WorkspaceReferenceArg::Index(new_idx));
        niri_action(Action::SetWorkspaceName {
            name: name.to_string(),
            workspace: None,
        });
    }

    if let Some(d) = dir {
        std::thread::sleep(std::time::Duration::from_millis(100));
        spawn_terminals(d);
    }
}

fn switch_to_entry(entry: &WorkspaceColumn) {
    if let Some(window_id) = entry.window_id
        && focus_window(window_id)
    {
        return;
    }

    if entry.static_workspace {
        focus_workspace(entry.workspace_ref.clone());
        if entry.app_label == "(empty)"
            && let Some(ref dir) = entry.dir
        {
            spawn_terminals(dir);
        }
    } else {
        if entry.app_label == "(empty)" {
            create_workspace(&entry.workspace_name, entry.dir.as_deref());
        }
        focus_workspace(entry.workspace_ref.clone());
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    focus_column(entry.column_index);
}

fn send_toggle() -> Result<(), Box<dyn std::error::Error>> {
    let path = daemon::socket_path();
    let mut stream = UnixStream::connect(&path)?;
    stream.write_all(b"toggle")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(())
}

fn start_config_watcher(tx: mpsc::Sender<NiriMessage>) {
    let config = config_path();
    let config_dir = config.parent().map(|p| p.to_path_buf());

    thread::spawn(move || {
        let tx_clone = tx.clone();
        let config_filename = config.file_name().map(|s| s.to_os_string());

        let mut watcher = match RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let dominated_by_config = event
                        .paths
                        .iter()
                        .any(|p| p.file_name() == config_filename.as_deref());
                    if dominated_by_config {
                        match event.kind {
                            notify::EventKind::Modify(_) | notify::EventKind::Create(_) => {
                                let _ = tx_clone.send(NiriMessage::ReloadConfig);
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

        if let Some(dir) = config_dir {
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                log::error!("Failed to watch config directory: {}", e);
                return;
            }
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
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
                    Event::WindowOpenedOrChanged { window } => {
                        if window.is_focused {
                            *focused_window.lock().unwrap() = Some(window.id);
                        }
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

fn overlay_size_caps_for_geometry(width: i32, height: i32) -> (i32, i32) {
    let available_width = (width - NIRI_OVERLAY_MARGIN).max(320);
    let available_height = (height - NIRI_OVERLAY_MARGIN).max(200);
    let max_width = clamp_i32(
        (width as f64 * NIRI_OVERLAY_WIDTH_RATIO).round() as i32,
        NIRI_OVERLAY_MIN_WIDTH.min(available_width),
        NIRI_OVERLAY_MAX_WIDTH.min(available_width),
    );
    let max_height = clamp_i32(
        (height as f64 * NIRI_OVERLAY_HEIGHT_RATIO).round() as i32,
        1,
        NIRI_OVERLAY_MAX_HEIGHT.min(available_height),
    );

    (max_width, max_height)
}

fn input_char_for_key(keyval: gtk4::gdk::Key) -> Option<char> {
    keyval.to_unicode().map(|ch| ch.to_ascii_lowercase())
}

fn selection_key_for_input(keyval: gtk4::gdk::Key) -> Option<char> {
    input_char_for_key(keyval).filter(|ch| KEYS.contains(ch))
}

fn update_overlay_size(window: &ApplicationWindow, scroller: &ScrolledWindow, outer_box: &GtkBox) {
    let (max_width, max_height) = overlay_monitor(window)
        .map(|monitor| {
            let geometry = monitor.geometry();
            overlay_size_caps_for_geometry(geometry.width(), geometry.height())
        })
        .unwrap_or((NIRI_OVERLAY_FALLBACK_WIDTH, NIRI_OVERLAY_FALLBACK_HEIGHT));

    scroller.set_max_content_width(max_width);
    scroller.set_max_content_height(max_height);

    let (_, natural) = outer_box.preferred_size();
    let width = clamp_i32(
        natural.width(),
        NIRI_OVERLAY_MIN_WIDTH.min(max_width),
        max_width,
    );
    let height = clamp_i32(natural.height().max(1), 1, max_height);

    window.set_default_size(width, height);
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
    let delta = adjustment.step_increment().max(NIRI_OVERLAY_STEP_SCROLL) * direction;
    scroll_overlay(scroller, delta);
}

fn scroll_overlay_by_page(scroller: &ScrolledWindow, direction: f64) {
    let adjustment = scroller.vadjustment();
    let delta = adjustment
        .page_increment()
        .max(adjustment.page_size() * 0.9)
        .max(NIRI_OVERLAY_PAGE_SCROLL)
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

fn build_ui(
    app: &Application,
    rx: mpsc::Receiver<NiriMessage>,
    focused_window: Arc<Mutex<Option<u64>>>,
    cache: Arc<Mutex<SessionCache>>,
) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(NIRI_OVERLAY_FALLBACK_WIDTH)
        .default_height(NIRI_OVERLAY_FALLBACK_HEIGHT)
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_anchor(Edge::Top, false);
    window.set_anchor(Edge::Bottom, false);
    window.set_anchor(Edge::Left, false);
    window.set_anchor(Edge::Right, false);

    let (config, last_config_error) = match load_config() {
        Ok(config) => (config, None),
        Err(err) => {
            notify_config_error(&err);
            (Config::default(), Some(err))
        }
    };
    let entries = get_workspace_columns(&config);
    let agent_sessions = load_agent_sessions();
    let codex_sessions = {
        let cache = cache.lock().unwrap();
        cache.codex_sessions.clone()
    };
    let codex_aliases = normalized_codex_aliases(&config.codex_aliases);

    let state = Rc::new(RefCell::new(AppState {
        config,
        codex_aliases,
        entries,
        pending_key: None,
        agent_sessions,
        codex_sessions,
        last_config_error,
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
    main_box.set_margin_start(20);
    main_box.set_margin_end(20);
    main_box.set_margin_bottom(20);
    scroller.set_child(Some(&main_box));

    {
        let state_ref = state.borrow();
        build_entry_list(
            &main_box,
            &state_ref.entries,
            state_ref.pending_key,
            &state_ref.agent_sessions,
            &state_ref.codex_sessions,
            &state_ref.codex_aliases,
        );
    }
    outer_box.append(&scroller);

    let css_provider = gtk4::CssProvider::new();
    css_provider.load_from_data(
        r#"
        window {
            background-color: transparent;
        }
        .outer {
            background-color: rgba(30, 30, 30, 0.95);
            border-radius: 10px;
            border: 2px solid #f92672;
        }
        .workspace-column {
            min-width: 0;
        }
        .workspace-group {
            min-width: 0;
        }
        separator.workspace-separator {
            margin-top: 4px;
            margin-bottom: 4px;
            color: rgba(249, 38, 114, 0.35);
        }
        label {
            color: #ffffff;
            font-size: 14px;
        }
        label.workspace-title {
            color: #b5bd68;
            font-size: 12px;
            font-family: monospace;
            font-weight: bold;
        }
        label.key {
            color: #f0c674;
            font-family: monospace;
            font-weight: bold;
        }
        label.project {
            color: #888888;
        }
        label.selected {
            color: #b5bd68;
        }
        "#,
    );

    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().unwrap(),
        &css_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    window.set_child(Some(&outer_box));

    let key_controller = gtk4::EventControllerKey::new();
    let state_clone = state.clone();
    let window_clone = window.clone();
    let main_box_clone = main_box.clone();
    let outer_box_clone = outer_box.clone();
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
            let mut state = state_clone.borrow_mut();
            if state.pending_key.is_some() {
                state.pending_key = None;
                let entries = state.entries.clone();
                let agent_sessions = state.agent_sessions.clone();
                let codex_sessions = state.codex_sessions.clone();
                let codex_aliases = state.codex_aliases.clone();
                drop(state);
                build_entry_list(
                    &main_box_clone,
                    &entries,
                    None,
                    &agent_sessions,
                    &codex_sessions,
                    &codex_aliases,
                );
                reset_overlay_scroll(&scroller_clone);
                update_overlay_size(&window_clone, &scroller_clone, &outer_box_clone);
            } else {
                drop(state);
                window_clone.set_visible(false);
            }
            return glib::Propagation::Stop;
        }

        if let Some(key_char) = selection_key {
            let mut state = state_clone.borrow_mut();

            if let Some(first_key) = state.pending_key {
                if let Some(entry) = state
                    .entries
                    .iter()
                    .find(|e| e.workspace_key == first_key && e.column_key == key_char)
                {
                    let entry = entry.clone();
                    state.pending_key = None;
                    drop(state);
                    window_clone.set_visible(false);
                    switch_to_entry(&entry);
                } else {
                    state.pending_key = None;
                    let entries = state.entries.clone();
                    let agent_sessions = state.agent_sessions.clone();
                    let codex_sessions = state.codex_sessions.clone();
                    let codex_aliases = state.codex_aliases.clone();
                    drop(state);
                    build_entry_list(
                        &main_box_clone,
                        &entries,
                        None,
                        &agent_sessions,
                        &codex_sessions,
                        &codex_aliases,
                    );
                    reset_overlay_scroll(&scroller_clone);
                    update_overlay_size(&window_clone, &scroller_clone, &outer_box_clone);
                }
            } else if state.entries.iter().any(|e| e.workspace_key == key_char) {
                state.pending_key = Some(key_char);
                let entries = state.entries.clone();
                let agent_sessions = state.agent_sessions.clone();
                let codex_sessions = state.codex_sessions.clone();
                let codex_aliases = state.codex_aliases.clone();
                drop(state);
                build_entry_list(
                    &main_box_clone,
                    &entries,
                    Some(key_char),
                    &agent_sessions,
                    &codex_sessions,
                    &codex_aliases,
                );
                reset_overlay_scroll(&scroller_clone);
                update_overlay_size(&window_clone, &scroller_clone, &outer_box_clone);
            }
        }

        glib::Propagation::Stop
    });

    window.add_controller(key_controller);
    window.set_visible(false);
    window.present();
    update_overlay_size(&window, &scroller, &outer_box);
    window.set_visible(false);

    let window_for_poll = window.clone();
    let state_for_poll = state.clone();
    let main_box_for_poll = main_box.clone();
    let outer_box_for_poll = outer_box.clone();
    let scroller_for_poll = scroller.clone();
    let focused_window_for_poll = focused_window.clone();
    let cache_for_poll = cache.clone();

    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                NiriMessage::Daemon(DaemonMessage::Toggle) => {
                    let is_visible = window_for_poll.is_visible();
                    if is_visible {
                        window_for_poll.set_visible(false);
                        let mut state = state_for_poll.borrow_mut();
                        state.pending_key = None;
                    } else {
                        // Cleanup stale sessions on toggle
                        let mut store = state::load();
                        state::cleanup_stale(&mut store);
                        state::save(&store);

                        let mut state = state_for_poll.borrow_mut();
                        state.entries = get_workspace_columns(&state.config);
                        state.agent_sessions = load_agent_sessions();
                        state.codex_sessions = {
                            let cache = cache_for_poll.lock().unwrap();
                            cache.codex_sessions.clone()
                        };
                        state.pending_key = None;
                        let entries = state.entries.clone();
                        let agent_sessions = state.agent_sessions.clone();
                        let codex_sessions = state.codex_sessions.clone();
                        let codex_aliases = state.codex_aliases.clone();
                        drop(state);
                        build_entry_list(
                            &main_box_for_poll,
                            &entries,
                            None,
                            &agent_sessions,
                            &codex_sessions,
                            &codex_aliases,
                        );
                        reset_overlay_scroll(&scroller_for_poll);
                        update_overlay_size(
                            &window_for_poll,
                            &scroller_for_poll,
                            &outer_box_for_poll,
                        );
                        window_for_poll.set_visible(true);
                        window_for_poll.present();
                        update_overlay_size(
                            &window_for_poll,
                            &scroller_for_poll,
                            &outer_box_for_poll,
                        );
                    }
                }
                NiriMessage::ReloadConfig => {
                    let mut state = state_for_poll.borrow_mut();
                    let reloaded = match load_config() {
                        Ok(config) => {
                            state.config = config;
                            state.entries = get_workspace_columns(&state.config);
                            state.codex_aliases =
                                normalized_codex_aliases(&state.config.codex_aliases);
                            state.last_config_error = None;
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
                        let entries = state.entries.clone();
                        let pending = state.pending_key;
                        let agent_sessions = state.agent_sessions.clone();
                        let codex_sessions = state.codex_sessions.clone();
                        let codex_aliases = state.codex_aliases.clone();
                        drop(state);
                        build_entry_list(
                            &main_box_for_poll,
                            &entries,
                            pending,
                            &agent_sessions,
                            &codex_sessions,
                            &codex_aliases,
                        );
                        reset_overlay_scroll(&scroller_for_poll);
                        update_overlay_size(
                            &window_for_poll,
                            &scroller_for_poll,
                            &outer_box_for_poll,
                        );
                    }
                }
                NiriMessage::Daemon(DaemonMessage::SessionsChanged) => {
                    let mut state = state_for_poll.borrow_mut();
                    state.agent_sessions = load_agent_sessions();
                    if window_for_poll.is_visible() {
                        let entries = state.entries.clone();
                        let pending = state.pending_key;
                        let agent_sessions = state.agent_sessions.clone();
                        let codex_sessions = state.codex_sessions.clone();
                        let codex_aliases = state.codex_aliases.clone();
                        drop(state);
                        build_entry_list(
                            &main_box_for_poll,
                            &entries,
                            pending,
                            &agent_sessions,
                            &codex_sessions,
                            &codex_aliases,
                        );
                        update_overlay_size(
                            &window_for_poll,
                            &scroller_for_poll,
                            &outer_box_for_poll,
                        );
                    }
                }
                NiriMessage::Daemon(DaemonMessage::CodexChanged) => {
                    let mut state = state_for_poll.borrow_mut();
                    state.codex_sessions = {
                        let cache = cache_for_poll.lock().unwrap();
                        cache.codex_sessions.clone()
                    };
                    if window_for_poll.is_visible() {
                        let entries = state.entries.clone();
                        let pending = state.pending_key;
                        let agent_sessions = state.agent_sessions.clone();
                        let codex_sessions = state.codex_sessions.clone();
                        let codex_aliases = state.codex_aliases.clone();
                        drop(state);
                        build_entry_list(
                            &main_box_for_poll,
                            &entries,
                            pending,
                            &agent_sessions,
                            &codex_sessions,
                            &codex_aliases,
                        );
                        update_overlay_size(
                            &window_for_poll,
                            &scroller_for_poll,
                            &outer_box_for_poll,
                        );
                    }
                }
                NiriMessage::Daemon(DaemonMessage::Track(event)) => {
                    let focused_id = *focused_window_for_poll.lock().unwrap();
                    handle_track_event(&event, focused_id);
                }
                NiriMessage::Daemon(DaemonMessage::List(_)) => {
                    // Handled by socket listener directly
                }
                NiriMessage::Daemon(DaemonMessage::Shutdown) => {
                    // Exit GTK app
                    std::process::exit(0);
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

fn handle_track_event(event: &TrackEvent, focused_window_id: Option<u64>) {
    let agent = event.agent.as_deref().unwrap_or("claude");
    let session_id = &event.session_id;
    let mut store = state::load();

    match event.event.as_str() {
        "session-start" => {
            let Some(window_id) = focused_window_id else {
                return;
            };
            let session = state::Session {
                agent: agent.to_string(),
                session_id: session_id.to_string(),
                cwd: event.cwd.clone(),
                state: "waiting".to_string(),
                state_updated: state::now(),
                window: state::WindowId {
                    niri_id: Some(window_id.to_string()),
                    tmux_id: None,
                },
            };
            store.sessions.insert(window_id.to_string(), session);
        }
        "session-end" => {
            let key = store
                .sessions
                .iter()
                .find(|(_, s)| s.agent == agent && s.session_id == *session_id)
                .map(|(k, _)| k.clone());
            if let Some(key) = key {
                store.sessions.remove(&key);
            }
        }
        "prompt-submit" => {
            if let Some(session) = state::find_by_session_id_mut(&mut store, agent, session_id) {
                session.state = "responding".to_string();
                session.state_updated = state::now();
            } else if let Some(window_id) = focused_window_id {
                let session = state::Session {
                    agent: agent.to_string(),
                    session_id: session_id.to_string(),
                    cwd: event.cwd.clone(),
                    state: "responding".to_string(),
                    state_updated: state::now(),
                    window: state::WindowId {
                        niri_id: Some(window_id.to_string()),
                        tmux_id: None,
                    },
                };
                store.sessions.insert(window_id.to_string(), session);
            }
        }
        "stop" => {
            if let Some(session) = state::find_by_session_id_mut(&mut store, agent, session_id) {
                let is_question = event
                    .transcript_path
                    .as_ref()
                    .map(|p| ends_with_question(p))
                    .unwrap_or(false);
                session.state = if is_question { "waiting" } else { "idle" }.to_string();
                session.state_updated = state::now();
            }
        }
        "notification" => {
            if event.notification_type.as_deref() == Some("permission_prompt")
                && let Some(session) = state::find_by_session_id_mut(&mut store, agent, session_id)
            {
                session.state = "waiting".to_string();
                session.state_updated = state::now();
            }
        }
        _ => {}
    }

    state::save(&store);
}

fn ends_with_question(transcript_path: &str) -> bool {
    use std::process::Command as StdCommand;

    let output = match StdCommand::new("tail")
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

fn group_entries_by_workspace<'a>(
    entries: &[&'a WorkspaceColumn],
) -> Vec<Vec<&'a WorkspaceColumn>> {
    let mut groups: Vec<Vec<&WorkspaceColumn>> = Vec::new();

    for entry in entries {
        let needs_new_group = groups
            .last()
            .and_then(|group| group.first())
            .map(|first| first.workspace_key != entry.workspace_key)
            .unwrap_or(true);

        if needs_new_group {
            groups.push(vec![*entry]);
        } else if let Some(group) = groups.last_mut() {
            group.push(*entry);
        }
    }

    groups
}

fn entry_markup(
    entry: &WorkspaceColumn,
    agent_sessions: &HashMap<u64, AgentSession>,
    codex_sessions: &HashMap<String, CodexSession>,
    codex_aliases: &[String],
) -> String {
    let app_label = glib::markup_escape_text(&entry.app_label);

    if let Some(window_id) = entry.window_id {
        if let Some(session) = agent_sessions.get(&window_id) {
            let agent = glib::markup_escape_text(&session.agent);
            return format!(
                "{} / {} <span color=\"{}\" weight=\"bold\">[{}]</span>",
                app_label,
                agent,
                session.state.color(),
                session.state.label()
            );
        }

        if let Some(state) = codex_state_for_entry(entry, codex_sessions, codex_aliases) {
            return format!(
                "codex <span color=\"{}\" weight=\"bold\">[{}]</span>",
                state.color(),
                state.label()
            );
        }
    }

    app_label.to_string()
}

fn build_entry_row(
    entry: &WorkspaceColumn,
    agent_sessions: &HashMap<u64, AgentSession>,
    codex_sessions: &HashMap<String, CodexSession>,
    codex_aliases: &[String],
) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 10);
    row.add_css_class("entry-row");

    let key_text = format!("[{}{}]", entry.workspace_key, entry.column_key);
    let key_label = Label::new(Some(&key_text));
    key_label.add_css_class("key");
    row.append(&key_label);

    let name_label = Label::new(None);
    name_label.set_markup(&entry_markup(
        entry,
        agent_sessions,
        codex_sessions,
        codex_aliases,
    ));
    name_label.add_css_class("project");
    name_label.set_xalign(0.0);
    name_label.set_hexpand(true);
    row.append(&name_label);

    row
}

fn build_workspace_group(
    entries: &[&WorkspaceColumn],
    agent_sessions: &HashMap<u64, AgentSession>,
    codex_sessions: &HashMap<String, CodexSession>,
    codex_aliases: &[String],
) -> GtkBox {
    let group = GtkBox::new(Orientation::Vertical, 6);
    group.add_css_class("workspace-group");

    if let Some(first) = entries.first() {
        let title = Label::new(Some(&first.workspace_name));
        title.add_css_class("workspace-title");
        title.set_xalign(0.0);
        group.append(&title);
    }

    for entry in entries {
        group.append(&build_entry_row(
            entry,
            agent_sessions,
            codex_sessions,
            codex_aliases,
        ));
    }

    group
}

fn build_entry_list(
    container: &GtkBox,
    entries: &[WorkspaceColumn],
    pending_key: Option<char>,
    agent_sessions: &HashMap<u64, AgentSession>,
    codex_sessions: &HashMap<String, CodexSession>,
    codex_aliases: &[String],
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let filtered: Vec<_> = if let Some(key) = pending_key {
        entries.iter().filter(|e| e.workspace_key == key).collect()
    } else {
        entries.iter().collect()
    };

    let groups = group_entries_by_workspace(&filtered);

    if pending_key.is_none() && groups.len() > 1 {
        let columns = GtkBox::new(Orientation::Horizontal, 24);
        columns.add_css_class("workspace-columns");

        let left_column = GtkBox::new(Orientation::Vertical, 12);
        left_column.add_css_class("workspace-column");
        left_column.set_hexpand(true);

        let right_column = GtkBox::new(Orientation::Vertical, 12);
        right_column.add_css_class("workspace-column");
        right_column.set_hexpand(true);

        for (index, group_entries) in groups.iter().enumerate() {
            let column = if index % 2 == 0 {
                &left_column
            } else {
                &right_column
            };

            if column.first_child().is_some() {
                let separator = Separator::new(Orientation::Horizontal);
                separator.add_css_class("workspace-separator");
                column.append(&separator);
            }

            column.append(&build_workspace_group(
                group_entries,
                agent_sessions,
                codex_sessions,
                codex_aliases,
            ));
        }

        columns.append(&left_column);
        if right_column.first_child().is_some() {
            columns.append(&right_column);
        }
        container.append(&columns);
    } else {
        for (index, group_entries) in groups.iter().enumerate() {
            if index > 0 {
                let separator = Separator::new(Orientation::Horizontal);
                separator.add_css_class("workspace-separator");
                container.append(&separator);
            }

            container.append(&build_workspace_group(
                group_entries,
                agent_sessions,
                codex_sessions,
                codex_aliases,
            ));
        }
    }
}

/// Run the niri daemon with GTK overlay (new `serve --niri` mode)
pub fn run_with_daemon() -> glib::ExitCode {
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonMessage>();
    let (niri_tx, niri_rx) = mpsc::channel::<NiriMessage>();
    let cache = Arc::new(Mutex::new(SessionCache::new()));
    let focused_window: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

    // Initial load
    {
        let mut cache = cache.lock().unwrap();
        cache.reload_agent_sessions();
        cache.reload_codex_sessions();
    }

    log::info!(
        "Starting niri daemon with overlay, listening on {:?}",
        daemon::socket_path()
    );

    // Start daemon threads (socket listener, file watchers)
    daemon::start_socket_listener(daemon_tx.clone(), cache.clone());
    daemon::start_sessions_watcher(daemon_tx.clone());
    daemon::start_codex_poller(daemon_tx.clone());

    // Start niri-specific threads
    start_config_watcher(niri_tx.clone());
    start_focus_tracker(focused_window.clone());

    // Bridge daemon messages to niri message channel
    let niri_tx_clone = niri_tx.clone();
    let cache_clone = cache.clone();
    thread::spawn(move || {
        loop {
            let msg = match daemon_rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            };

            // Handle cache updates for daemon messages
            match &msg {
                DaemonMessage::SessionsChanged => {
                    let mut cache = cache_clone.lock().unwrap();
                    cache.reload_agent_sessions();
                }
                DaemonMessage::CodexChanged => {
                    let mut cache = cache_clone.lock().unwrap();
                    cache.reload_codex_sessions();
                }
                _ => {}
            }

            // Forward to GTK thread
            if niri_tx_clone.send(NiriMessage::Daemon(msg)).is_err() {
                break;
            }
        }
    });

    let rx = Rc::new(RefCell::new(Some(niri_rx)));
    let focused_window_rc = Rc::new(RefCell::new(Some(focused_window)));
    let cache_rc = Rc::new(RefCell::new(Some(cache)));

    let app = Application::builder()
        .application_id(APP_ID)
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let rx_clone = rx.clone();
    let focused_clone = focused_window_rc.clone();
    let cache_clone = cache_rc.clone();
    app.connect_activate(move |app| {
        if let (Some(rx), Some(focused), Some(cache)) = (
            rx_clone.borrow_mut().take(),
            focused_clone.borrow_mut().take(),
            cache_clone.borrow_mut().take(),
        ) {
            build_ui(app, rx, focused, cache);
        }
    });

    app.run_with_args::<&str>(&[])
}

/// Legacy run function for backward compatibility (`niri --toggle` and standalone daemon)
pub fn run(toggle: bool) -> glib::ExitCode {
    if toggle {
        if let Err(e) = send_toggle() {
            log::error!("Failed to toggle: {} (is daemon running?)", e);
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    // Legacy mode: run standalone with its own socket listener
    run_with_daemon()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
    fn unnamed_workspaces_are_ignored_by_default() {
        let config: Config = toml::from_str("").expect("default config should parse");
        let seen = HashSet::new();

        assert!(should_skip_discovered_workspace(
            None, // unnamed workspace
            "3",  // fallback display name from index
            &config, &seen,
        ));
    }

    #[test]
    fn ignore_numeric_sessions_hides_numeric_named_workspaces() {
        let config: Config = toml::from_str(
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
        let config: Config = toml::from_str(
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
    fn codex_aliases_are_normalized_and_matched_case_insensitively() {
        let aliases = normalized_codex_aliases(&vec!["cx".to_string(), "CXY".to_string()]);
        assert_eq!(aliases, vec!["codex", "cx", "CXY"]);
        assert!(window_title_matches_codex_aliases("codex", &aliases));
        assert!(window_title_matches_codex_aliases("CX", &aliases));
        assert!(window_title_matches_codex_aliases("cxy", &aliases));
        assert!(window_title_matches_codex_aliases("cxy resume", &aliases));
        assert!(window_title_matches_codex_aliases(
            "/home/me/bin/cx",
            &aliases
        ));
        assert!(!window_title_matches_codex_aliases("claude", &aliases));
        assert!(!window_title_matches_codex_aliases("execute", &aliases));
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
        let (max_width, max_height) = overlay_size_caps_for_geometry(2560, 1440);
        let compact_width = clamp_i32(380, NIRI_OVERLAY_MIN_WIDTH.min(max_width), max_width);
        let compact_height = clamp_i32(170, 1, max_height);

        assert_eq!(compact_width, 380);
        assert_eq!(compact_height, 170);
        assert!(compact_width < max_width);
        assert!(compact_height < max_height);
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

        let names: Vec<_> = configured_projects(&config)
            .into_iter()
            .map(project_workspace_name)
            .collect();

        assert_eq!(names, vec!["agent-switch", "main", "wayvoice"]);
    }
}
