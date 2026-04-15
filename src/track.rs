use crate::daemon;
use serde::Deserialize;
use std::io::{self, Read};
use std::process::Command;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: Option<String>,
    agent: Option<String>,
    cwd: Option<String>,
    transcript_path: Option<String>,
    notification_type: Option<String>,
    niri_id: Option<String>,
}

fn get_tmux_window_id() -> Option<String> {
    if std::env::var("TMUX").is_err() {
        return None;
    }
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{window_id}"])
        .output()
        .ok()?;
    if output.status.success() {
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

fn get_niri_window_id() -> Option<String> {
    let output = Command::new("niri")
        .args(["msg", "-j", "windows"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let windows = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout).ok()?;
    windows
        .into_iter()
        .find(|window| {
            window
                .get("is_focused")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        })
        .and_then(|window| window.get("id").and_then(|value| value.as_u64()))
        .map(|id| id.to_string())
}

/// Append the caller's PPID to the session ID so that forked Claude agents
/// (which inherit the same session_id) get distinct entries.  The PPID is
/// the PID of the Claude process that spawned this hook command, which
/// differs between a parent Claude and any agents it forks.
fn disambiguate_session_id(id: String) -> String {
    let ppid = std::os::unix::process::parent_id();
    format!("{id}-{ppid}")
}

/// Returns true on success, false on failure
pub fn handle_event(event: &str, agent_override: Option<&str>) -> bool {
    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("Failed to read stdin");
        return false;
    }

    let hook: HookInput = match serde_json::from_str(&input) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Failed to parse hook input: {}", e);
            return false;
        }
    };

    let event = match daemon::TrackEventKind::from_str(event) {
        Ok(event) => event,
        Err(err) => {
            eprintln!("Invalid event: {}", err);
            return false;
        }
    };

    let session_id = match hook.session_id {
        Some(id) => disambiguate_session_id(id),
        None => {
            eprintln!("Missing session_id");
            return false;
        }
    };

    let agent = match agent_override.map(str::to_string).or(hook.agent) {
        Some(agent) => agent,
        None => {
            eprintln!("Missing agent; pass --agent or include agent in hook payload");
            return false;
        }
    };

    let msg = daemon::TrackEvent {
        event,
        session_id,
        agent: Some(agent),
        cwd: hook.cwd,
        transcript_path: hook.transcript_path,
        notification_type: hook.notification_type,
        tmux_id: get_tmux_window_id(),
        niri_id: hook.niri_id.or_else(get_niri_window_id),
    };

    match daemon::send_track_request(&msg) {
        Ok(()) => true,
        Err(err) => {
            eprintln!("Failed to send track event to daemon: {}", err);
            false
        }
    }
}
