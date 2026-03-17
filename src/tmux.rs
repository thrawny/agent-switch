use crate::daemon::{self, CodexSession, ListResponse};
use crate::projects;
use crate::state;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const KEYS: [char; 12] = ['h', 'j', 'k', 'l', 'u', 'i', 'o', 'p', 'n', 'm', ',', '.'];

/// Query daemon for cached sessions (instant, includes Codex)
fn query_daemon_sessions() -> Option<ListResponse> {
    daemon::query_daemon_list().ok()
}

/// Get Codex session for a pane's current path
fn get_codex_for_pane<'a>(
    pane_path: &str,
    codex_sessions: &'a HashMap<String, CodexSession>,
) -> Option<&'a CodexSession> {
    daemon::match_codex_by_dir(pane_path, codex_sessions)
}

#[derive(Clone)]
struct TmuxWindow {
    session_name: String,
    session_index: String, // e.g. "main:1"
    window_id: String,     // e.g. "@5"
    window_name: String,
    pane_path: Option<String>,    // Current pane working directory
    pane_command: Option<String>, // Current pane command
}

#[derive(Clone, Copy)]
enum AgentState {
    Waiting,
    Responding,
    Idle,
    Unknown,
}

impl AgentState {
    fn from_session_state(state: state::SessionState) -> Self {
        match state {
            state::SessionState::Waiting => Self::Waiting,
            state::SessionState::Responding => Self::Responding,
            state::SessionState::Idle => Self::Idle,
            state::SessionState::Unknown => Self::Unknown,
        }
    }

    fn from_daemon_state(state: daemon::AgentState) -> Self {
        match state {
            daemon::AgentState::Waiting => Self::Waiting,
            daemon::AgentState::Responding => Self::Responding,
            daemon::AgentState::Idle => Self::Idle,
            daemon::AgentState::Unknown => Self::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Responding => "working",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Waiting => "\x1b[1;33m",  // bold yellow
            Self::Responding => "\x1b[34m", // blue
            Self::Idle => "\x1b[90m",       // gray
            Self::Unknown => "\x1b[31m",    // red
        }
    }
}

#[allow(dead_code)]
enum ScreenState {
    Sessions,
    Windows {
        target_session: String,
        session_windows: Vec<TmuxWindow>,
    },
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = stty_command(&["sane"]);
    }
}

fn enable_raw_mode() -> Option<RawModeGuard> {
    let status = stty_command(&["-echo", "-icanon", "min", "1", "time", "0"])?;
    if status.success() {
        Some(RawModeGuard)
    } else {
        None
    }
}

fn stty_command(args: &[&str]) -> Option<std::process::ExitStatus> {
    // Try GNU stty (-F) first, fall back to BSD stty (-f)
    let gnu = Command::new("stty")
        .args(["-F", "/dev/tty"])
        .args(args)
        .status()
        .ok();
    if let Some(status) = &gnu
        && status.success()
    {
        return gnu;
    }
    Command::new("stty")
        .args(["-f", "/dev/tty"])
        .args(args)
        .status()
        .ok()
}

struct Tty {
    file: fs::File,
}

impl Tty {
    fn open() -> Option<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        Some(Self { file })
    }

    fn read_key(&mut self) -> Option<char> {
        let mut buf = [0u8; 1];
        if self.file.read_exact(&mut buf).is_ok() {
            Some(buf[0] as char)
        } else {
            None
        }
    }

    fn write_all(&mut self, text: &str) {
        let _ = self.file.write_all(text.as_bytes());
    }

    fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

fn key_to_index(key: char) -> i32 {
    KEYS.iter()
        .position(|&k| k == key)
        .map(|v| v as i32)
        .unwrap_or(-1)
}

fn list_tmux_windows() -> Vec<TmuxWindow> {
    let output = Command::new("tmux")
        .args([
            "list-windows",
            "-a",
            "-F",
            "#{session_name}:#{window_index}\t#{window_id}\t#{window_name}\t#{pane_current_path}\t#{pane_current_command}",
        ])
        .output()
        .ok();

    let output = match output {
        Some(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let mut windows = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let session_name = parts[0].split(':').next().unwrap_or("").to_string();
        // Skip scratch sessions
        if session_name == "scratch" {
            continue;
        }
        let pane_path = parts
            .get(3)
            .map(|p| p.to_string())
            .filter(|p| !p.is_empty());
        let pane_command = parts
            .get(4)
            .map(|p| p.to_string())
            .filter(|p| !p.is_empty());
        windows.push(TmuxWindow {
            session_name,
            session_index: parts[0].to_string(),
            window_id: parts[1].to_string(),
            window_name: parts[2].to_string(),
            pane_path,
            pane_command,
        });
    }
    windows
}

fn load_projects_config() -> projects::Config {
    projects::load_config().unwrap_or_default()
}

fn load_session_order(config: &projects::Config) -> Vec<String> {
    projects::configured_project_names(config)
}

fn filter_windows_by_config(
    windows: Vec<TmuxWindow>,
    config: &projects::Config,
) -> Vec<TmuxWindow> {
    windows
        .into_iter()
        .filter(|window| !projects::should_ignore_name(&window.session_name, config))
        .collect()
}

fn sorted_sessions(windows: &[TmuxWindow], order: &[String]) -> Vec<String> {
    let mut sessions = Vec::new();
    let mut seen = HashSet::new();
    for window in windows {
        if seen.insert(window.session_name.clone()) {
            sessions.push(window.session_name.clone());
        }
    }

    let mut sorted = Vec::new();
    for preferred in order {
        if sessions.iter().any(|name| name == preferred) {
            sorted.push(preferred.clone());
        }
    }
    let mut rest: Vec<String> = sessions
        .into_iter()
        .filter(|name| !sorted.contains(name))
        .collect();
    rest.sort();
    sorted.extend(rest);
    sorted
}

fn format_status(state: AgentState, agent: &str) -> String {
    format!("{}{} [{}]\x1b[0m", state.color(), agent, state.label())
}

fn status_for_window(
    window: &TmuxWindow,
    status_by_tmux_id: &HashMap<String, &state::Session>,
    codex_by_tmux_id: &HashMap<String, CodexSession>,
    codex_sessions: &HashMap<String, CodexSession>,
    codex_aliases: &[String],
) -> Option<String> {
    // First check Claude sessions
    if let Some(session) = status_by_tmux_id.get(&window.window_id) {
        let state = AgentState::from_session_state(session.state);
        return Some(format_status(state, &session.agent));
    }
    if let Some(codex) = codex_by_tmux_id.get(&window.window_id) {
        let state = AgentState::from_daemon_state(codex.state);
        return Some(format_status(state, "codex"));
    }
    // Only check Codex for windows with codex name/alias in the title or command.
    let has_codex_name = projects::contains_alias_token(&window.window_name, codex_aliases);
    let has_codex_command = window
        .pane_command
        .as_ref()
        .map(|c| projects::contains_alias_token(c, codex_aliases))
        .unwrap_or(false);
    if !has_codex_name && !has_codex_command {
        return None;
    }
    if let Some(pane_path) = &window.pane_path
        && let Some(codex) = get_codex_for_pane(pane_path, codex_sessions)
    {
        let state = AgentState::from_daemon_state(codex.state);
        return Some(format_status(state, "codex"));
    }
    None
}

/// Query actual terminal dimensions via stty on /dev/tty.
/// Works correctly in tmux popups (unlike tmux display-message which returns the parent pane size).
fn terminal_size() -> (usize, usize) {
    // Try stty size on /dev/tty — works in tmux popups
    for flag in ["-f", "-F"] {
        if let Ok(output) = Command::new("stty")
            .args([flag, "/dev/tty", "size"])
            .output()
            && output.status.success()
            && let Ok(out) = String::from_utf8(output.stdout)
        {
            let parts: Vec<&str> = out.split_whitespace().collect();
            if let [rows, cols] = parts.as_slice() {
                let rows = rows.parse().unwrap_or(24);
                let cols = cols.parse().unwrap_or(80);
                return (rows, cols);
            }
        }
    }
    (24, 80)
}

fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            in_escape = true;
        } else {
            len += 1;
        }
    }
    len
}

/// Truncate a string with ANSI escapes to `width` visible characters, appending a reset code.
fn truncate_visible(s: &str, width: usize) -> String {
    let mut result = String::new();
    let mut vis = 0;
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            result.push(ch);
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            in_escape = true;
            result.push(ch);
        } else {
            if vis >= width {
                break;
            }
            result.push(ch);
            vis += 1;
        }
    }
    result.push_str("\x1b[0m");
    result
}

fn pad_visible(s: &str, width: usize) -> String {
    let vis = visible_len(s);
    if vis >= width {
        truncate_visible(s, width)
    } else {
        format!("{}{}", s, " ".repeat(width - vis))
    }
}

fn tty_print(tty: &mut Option<Tty>, text: &str) {
    if let Some(tty) = tty.as_mut() {
        tty.write_all(text);
    } else {
        print!("{text}");
    }
}

fn tty_flush(tty: &mut Option<Tty>) {
    if let Some(tty) = tty.as_mut() {
        tty.flush();
    } else {
        let _ = io::stdout().flush();
    }
}

fn print_clear(tty: &mut Option<Tty>) {
    if let Some(tty) = tty.as_mut() {
        tty.write_all("\x1b[2J\x1b[H");
        tty.flush();
        return;
    }
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
}

/// Compute the row count of an interleaved two-column layout.
/// Even groups go left, odd groups go right, with one separator between groups per column.
/// `cap` limits how many lines each group contributes.
fn interleaved_height(group_sizes: &[usize], cap: usize) -> usize {
    let mut left = 0usize;
    let mut right = 0usize;
    for (gi, &size) in group_sizes.iter().enumerate() {
        let size = size.min(cap);
        let col = if gi % 2 == 0 { &mut left } else { &mut right };
        if *col > 0 {
            *col += 1; // separator line
        }
        *col += size;
    }
    left.max(right)
}

fn render_sessions_screen(
    tty: &mut Option<Tty>,
    sessions: &[String],
    windows: &[TmuxWindow],
    status_by_tmux_id: &HashMap<String, &state::Session>,
    codex_by_tmux_id: &HashMap<String, CodexSession>,
    codex_sessions: &HashMap<String, CodexSession>,
    codex_aliases: &[String],
) {
    print_clear(tty);
    let header = "h/j/k/l = select session | / = search | q/Esc = cancel\n\n";
    tty_print(tty, header);

    let (term_rows, term_cols) = terminal_size();
    let max_lines = term_rows.saturating_sub(3);

    // Collect lines grouped by session so we never split a session across columns.
    let mut groups: Vec<Vec<String>> = Vec::new();
    for (sidx, session) in sessions.iter().enumerate() {
        let skey = if sidx < KEYS.len() { KEYS[sidx] } else { '?' };
        let mut group = Vec::new();
        for (widx, window) in windows
            .iter()
            .filter(|w| &w.session_name == session)
            .enumerate()
        {
            let wkey = if widx < KEYS.len() { KEYS[widx] } else { '?' };
            let status = status_for_window(
                window,
                status_by_tmux_id,
                codex_by_tmux_id,
                codex_sessions,
                codex_aliases,
            );
            let content = if let Some(status) = status {
                format!("{} {} {}", window.session_index, status, window.window_name)
            } else {
                format!("{} {}", window.session_index, window.window_name)
            };
            group.push(format!("\x1b[33m[{skey}{wkey}]\x1b[0m {content}"));
        }
        groups.push(group);
    }

    let total_lines: usize = groups.iter().map(|g| g.len()).sum();

    if total_lines > max_lines / 2 {
        // Interleaved two-column layout: even sessions left, odd sessions right.
        // Both columns pack tightly with a blank line between sessions.
        let col_width = term_cols / 2;

        // If two columns still won't fit, cap each session at 3 windows.
        let group_sizes: Vec<usize> = groups.iter().map(|g| g.len()).collect();
        let cap = if interleaved_height(&group_sizes, usize::MAX) > max_lines {
            3
        } else {
            usize::MAX
        };

        let mut left_lines: Vec<String> = Vec::new();
        let mut right_lines: Vec<String> = Vec::new();
        for (gi, group) in groups.iter().enumerate() {
            let col = if gi % 2 == 0 {
                &mut left_lines
            } else {
                &mut right_lines
            };
            if !col.is_empty() {
                col.push(String::new());
            }
            col.extend(group.iter().take(cap).cloned());
        }
        let rows = left_lines.len().max(right_lines.len());
        let pad = " ".repeat(col_width);
        for i in 0..rows.min(max_lines) {
            let left = left_lines
                .get(i)
                .filter(|s| !s.is_empty())
                .map(|s| pad_visible(s, col_width))
                .unwrap_or_else(|| pad.clone());
            let right = right_lines
                .get(i)
                .filter(|s| !s.is_empty())
                .map(|s| truncate_visible(s, col_width))
                .unwrap_or_default();
            tty_print(tty, &format!("{left}{right}\n"));
        }
    } else {
        for group in &groups {
            for line in group {
                tty_print(tty, &format!("{line}\n"));
            }
        }
    }

    tty_flush(tty);
}

fn render_windows_screen(
    tty: &mut Option<Tty>,
    target_session: &str,
    windows: &[TmuxWindow],
    status_by_tmux_id: &HashMap<String, &state::Session>,
    codex_by_tmux_id: &HashMap<String, CodexSession>,
    codex_sessions: &HashMap<String, CodexSession>,
    codex_aliases: &[String],
) -> Vec<TmuxWindow> {
    print_clear(tty);
    tty_print(tty, "h/j/k/l = select window | q/Esc = cancel\n\n");
    tty_print(
        tty,
        &format!("\x1b[36mSession: {target_session}\x1b[0m\n\n"),
    );

    let mut session_windows = Vec::new();
    for (widx, window) in windows
        .iter()
        .filter(|w| w.session_name == target_session)
        .enumerate()
    {
        session_windows.push(window.clone());
        let wkey = if widx < KEYS.len() { KEYS[widx] } else { '?' };
        let status = status_for_window(
            window,
            status_by_tmux_id,
            codex_by_tmux_id,
            codex_sessions,
            codex_aliases,
        );
        let content = if let Some(status) = status {
            format!("{} {} {}", window.session_index, status, window.window_name)
        } else {
            format!("{} {}", window.session_index, window.window_name)
        };
        tty_print(tty, &format!("\x1b[33m[{wkey}]\x1b[0m {content}\n"));
    }
    tty_flush(tty);

    session_windows
}

fn run_fzf_search(windows: &[TmuxWindow]) {
    let mut input = String::new();
    for window in windows {
        input.push_str(&format!(
            "{}\t{} {}\n",
            window.session_index, window.session_index, window.window_name
        ));
    }

    let mut fzf = match Command::new("fzf")
        .args([
            "--ansi",
            "--no-border",
            "--height=100%",
            "--with-nth=2..",
            "--header=Type to search, Enter to select",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };

    if let Some(mut stdin) = fzf.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }

    let output = match fzf.wait_with_output() {
        Ok(output) => output,
        Err(_) => return,
    };

    if !output.status.success() {
        return;
    }

    let selected = String::from_utf8_lossy(&output.stdout);
    let target = selected.split('\t').next().unwrap_or("").trim();
    if target.is_empty() {
        return;
    }

    let _ = Command::new("tmux")
        .args(["switch-client", "-t", target])
        .status();
}

pub fn run() {
    if env::var("TMUX").is_err() {
        eprintln!("agent-switch tmux must run inside tmux");
        return;
    }

    let store = match state::with_locked_store(|store| {
        state::cleanup_stale(store);
        Ok(store.clone())
    }) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("Failed to load state: {}", err);
            return;
        }
    };

    let config = load_projects_config();
    let windows = filter_windows_by_config(list_tmux_windows(), &config);
    if windows.is_empty() {
        eprintln!("No tmux windows found");
        return;
    }

    let session_order = load_session_order(&config);
    let sessions = sorted_sessions(&windows, &session_order);
    let codex_aliases = projects::normalized_codex_aliases(&config.codex_aliases);

    // Build lookup by tmux_id for agent status
    let status_by_tmux_id: HashMap<String, &state::Session> = store
        .sessions
        .values()
        .filter_map(|s| s.window.tmux_id.as_ref().map(|id| (id.clone(), s)))
        .collect();

    // Query daemon for Codex sessions (instant if running, empty otherwise)
    let (codex_by_tmux_id, codex_sessions): (
        HashMap<String, CodexSession>,
        HashMap<String, CodexSession>,
    ) = query_daemon_sessions()
        .map(|resp| {
            let mut by_tmux_id = HashMap::new();
            let mut by_session_id = HashMap::new();

            for entry in resp.codex {
                let session = CodexSession::new(
                    entry.session_id,
                    entry.cwd,
                    entry.state,
                    entry.state_updated,
                );

                if let Some(tmux_id) = entry.tmux_id {
                    by_tmux_id.insert(tmux_id, session.clone());
                }
                by_session_id.insert(session.session_id.clone(), session);
            }

            (by_tmux_id, by_session_id)
        })
        .unwrap_or_default();

    let mut tty_out = Tty::open();

    let _raw = match enable_raw_mode() {
        Some(guard) => guard,
        None => {
            eprintln!("Failed to enable raw mode");
            return;
        }
    };

    let (key_tx, key_rx) = mpsc::channel::<char>();
    thread::spawn(move || {
        let mut tty_in = Tty::open();
        loop {
            let key = if let Some(tty) = tty_in.as_mut() {
                tty.read_key()
            } else {
                let mut buf = [0u8; 1];
                if io::stdin().read_exact(&mut buf).is_ok() {
                    Some(buf[0] as char)
                } else {
                    None
                }
            };
            match key {
                Some(k) => {
                    if key_tx.send(k).is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    });

    let mut screen = ScreenState::Sessions;
    render_sessions_screen(
        &mut tty_out,
        &sessions,
        &windows,
        &status_by_tmux_id,
        &codex_by_tmux_id,
        &codex_sessions,
        &codex_aliases,
    );

    loop {
        let key = match key_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(key) => key,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        match &mut screen {
            ScreenState::Sessions => {
                if key == '/' {
                    drop(_raw);
                    if let Ok(exe) = env::current_exe() {
                        let err = Command::new(exe).arg("tmux").arg("--fzf").exec();
                        eprintln!("Failed to exec fzf mode: {err}");
                    }
                    run_fzf_search(&windows);
                    return;
                }

                if key == 'q' || key == 27 as char {
                    return;
                }

                let session_idx = key_to_index(key);
                if session_idx < 0 || session_idx as usize >= sessions.len() {
                    return;
                }

                let target_session = sessions[session_idx as usize].clone();
                let session_windows = render_windows_screen(
                    &mut tty_out,
                    &target_session,
                    &windows,
                    &status_by_tmux_id,
                    &codex_by_tmux_id,
                    &codex_sessions,
                    &codex_aliases,
                );
                screen = ScreenState::Windows {
                    target_session,
                    session_windows,
                };
            }
            ScreenState::Windows {
                session_windows, ..
            } => {
                if key == 'q' || key == 27 as char {
                    return;
                }

                let window_idx = key_to_index(key);
                if window_idx < 0 {
                    return;
                }

                let window_idx = window_idx as usize;
                let target = session_windows
                    .get(window_idx)
                    .map(|w| w.session_index.clone())
                    .unwrap_or_default();
                if target.is_empty() {
                    return;
                }

                let _ = Command::new("tmux")
                    .args(["switch-client", "-t", &target])
                    .status();
                return;
            }
        }
    }
}

pub fn run_fzf_only() {
    if env::var("TMUX").is_err() {
        eprintln!("agent-switch tmux must run inside tmux");
        return;
    }

    let config = load_projects_config();
    let windows = filter_windows_by_config(list_tmux_windows(), &config);
    if windows.is_empty() {
        eprintln!("No tmux windows found");
        return;
    }

    run_fzf_search(&windows);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(session_name: &str, window_id: &str) -> TmuxWindow {
        TmuxWindow {
            session_name: session_name.to_string(),
            session_index: format!("{}:1", session_name),
            window_id: window_id.to_string(),
            window_name: "shell".to_string(),
            pane_path: None,
            pane_command: None,
        }
    }

    #[test]
    fn projects_toml_drives_tmux_order_and_filtering() {
        let config: projects::Config = toml::from_str(
            r#"
ignore = ["web"]
ignoreNumericSessions = true

[[project]]
dir = "~/code/agent-switch"

[[project]]
name = "company"
dir = "~/code/the-company-private"
"#,
        )
        .expect("projects.toml should parse");

        let windows = vec![
            window("company", "@1"),
            window("1", "@2"),
            window("misc", "@3"),
            window("agent-switch", "@4"),
            window("web", "@5"),
            window("company", "@6"),
        ];

        let filtered = filter_windows_by_config(windows, &config);
        let order = load_session_order(&config);
        let sessions = sorted_sessions(&filtered, &order);

        assert_eq!(order, vec!["agent-switch", "company"]);
        assert_eq!(sessions, vec!["agent-switch", "company", "misc"]);
    }

    #[test]
    fn numeric_tmux_sessions_are_kept_when_not_ignored() {
        let config: projects::Config = toml::from_str(
            r#"
ignoreNumericSessions = false
"#,
        )
        .expect("projects.toml should parse");

        let sessions = sorted_sessions(
            &filter_windows_by_config(vec![window("1", "@1"), window("dev", "@2")], &config),
            &load_session_order(&config),
        );

        assert_eq!(sessions, vec!["1", "dev"]);
    }

    #[test]
    fn ignore_list_wins_even_for_prioritized_projects() {
        let config: projects::Config = toml::from_str(
            r#"
ignore = ["company"]

[[project]]
name = "company"
dir = "~/code/the-company-private"

[[project]]
name = "agent-switch"
dir = "~/code/agent-switch"
"#,
        )
        .expect("projects.toml should parse");

        let sessions = sorted_sessions(
            &filter_windows_by_config(
                vec![
                    window("company", "@1"),
                    window("misc", "@2"),
                    window("agent-switch", "@3"),
                ],
                &config,
            ),
            &load_session_order(&config),
        );

        assert_eq!(sessions, vec!["agent-switch", "misc"]);
    }

    #[test]
    fn duplicate_project_names_are_deduplicated_in_order() {
        let config: projects::Config = toml::from_str(
            r#"
[[project]]
dir = "~/code/agent-switch"

[[project]]
name = "agent-switch"
dir = "~/work/agent-switch"
"#,
        )
        .expect("projects.toml should parse");

        let order = load_session_order(&config);
        let sessions = sorted_sessions(
            &filter_windows_by_config(
                vec![window("misc", "@1"), window("agent-switch", "@2")],
                &config,
            ),
            &order,
        );

        assert_eq!(order, vec!["agent-switch"]);
        assert_eq!(sessions, vec!["agent-switch", "misc"]);
    }

    #[test]
    fn non_project_sessions_are_sorted_alphabetically() {
        let config: projects::Config = toml::from_str(
            r#"
[[project]]
name = "company"
dir = "~/code/the-company-private"
"#,
        )
        .expect("projects.toml should parse");

        let sessions = sorted_sessions(
            &filter_windows_by_config(
                vec![
                    window("misc", "@1"),
                    window("zeta", "@2"),
                    window("company", "@3"),
                    window("alpha", "@4"),
                ],
                &config,
            ),
            &load_session_order(&config),
        );

        assert_eq!(sessions, vec!["company", "alpha", "misc", "zeta"]);
    }

    #[test]
    fn project_with_name_only_defaults_dir_to_home() {
        let config: projects::Config = toml::from_str(
            r#"
[[project]]
name = "scratch"
"#,
        )
        .expect("projects.toml should parse");

        assert_eq!(config.project.len(), 1);
        assert_eq!(config.project[0].name.as_deref(), Some("scratch"));
        assert_eq!(config.project[0].dir, "~/");
        assert_eq!(
            projects::project_workspace_name(&config.project[0]),
            "scratch"
        );
    }

    #[test]
    fn codex_aliases_parse_and_include_codex_default() {
        let config: projects::Config = toml::from_str(
            r#"
codexAliases = ["cx", "cxy"]
"#,
        )
        .expect("projects.toml should parse");

        assert_eq!(
            projects::normalized_codex_aliases(&config.codex_aliases),
            vec!["codex", "cx", "cxy"]
        );
    }

    #[test]
    fn alias_token_match_is_exact_by_token() {
        let aliases = vec!["codex".to_string(), "cx".to_string(), "cxy".to_string()];
        assert!(projects::contains_alias_token("cxy", &aliases));
        assert!(projects::contains_alias_token("run cx now", &aliases));
        assert!(projects::contains_alias_token("/home/me/bin/cx", &aliases));
        assert!(!projects::contains_alias_token("execute", &aliases));
    }

    #[test]
    fn invalid_projects_toml_falls_back_to_default_behavior() {
        let config = projects::parse_config_or_default("ignoreNumericSessions = not-a-bool");

        let sessions = sorted_sessions(
            &filter_windows_by_config(vec![window("1", "@1"), window("web", "@2")], &config),
            &load_session_order(&config),
        );

        // Default config: no explicit ordering, no ignore list, numeric sessions allowed.
        assert_eq!(sessions, vec!["1", "web"]);
    }

    #[test]
    fn visible_len_strips_ansi_escapes() {
        assert_eq!(visible_len("hello"), 5);
        assert_eq!(visible_len("\x1b[33m[hh]\x1b[0m main:1 shell"), 17);
        assert_eq!(visible_len("\x1b[1;33mwait\x1b[0m"), 4);
        assert_eq!(visible_len(""), 0);
    }

    #[test]
    fn pad_visible_pads_based_on_visible_width() {
        let s = "\x1b[33mhi\x1b[0m";
        let padded = pad_visible(s, 10);
        assert_eq!(visible_len(&padded), 10);
        assert!(padded.starts_with(s));
    }

    #[test]
    fn interleaved_height_basic() {
        // 2 groups of 3: left gets group 0 (3), right gets group 1 (3)
        assert_eq!(interleaved_height(&[3, 3], usize::MAX), 3);
    }

    #[test]
    fn interleaved_height_uneven() {
        // left: group 0 (7), right: group 1 (3) → max(7, 3) = 7
        assert_eq!(interleaved_height(&[7, 3], usize::MAX), 7);
    }

    #[test]
    fn interleaved_height_many_groups_with_separators() {
        // Simulates: dotfiles(7), data-scripts(3), kanel-backend(5), kf1-go(7),
        //            main(5), kanel-gitops(3), tf-infra(3), proj-mgmt(2), site(5), split-binary(2)
        // Left (even):  7 +1+ 5 +1+ 5 +1+ 3 +1+ 5 = 29
        // Right (odd):  3 +1+ 7 +1+ 3 +1+ 2 +1+ 2 = 21
        assert_eq!(
            interleaved_height(&[7, 3, 5, 7, 5, 3, 3, 2, 5, 2], usize::MAX),
            29
        );
    }

    #[test]
    fn interleaved_height_with_cap() {
        // Same groups capped at 3 windows each
        // Left (even):  3 +1+ 3 +1+ 3 +1+ 3 +1+ 3 = 19
        // Right (odd):  3 +1+ 3 +1+ 3 +1+ 2 +1+ 2 = 17
        assert_eq!(interleaved_height(&[7, 3, 5, 7, 5, 3, 3, 2, 5, 2], 3), 19);
    }

    #[test]
    fn cap_triggers_when_height_exceeds_max_lines() {
        let sizes = vec![7, 3, 5, 7, 5, 3, 3, 2, 5, 2];
        // Full height is 29, so any max_lines < 29 should trigger cap
        assert!(interleaved_height(&sizes, usize::MAX) > 20);
        assert!(interleaved_height(&sizes, 3) <= 20);
    }
}
