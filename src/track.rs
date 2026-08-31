use crate::compositor;
use crate::daemon;
use serde::Deserialize;
use std::io::{self, Read};
use std::str::FromStr;

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: Option<String>,
    session_name: Option<String>,
    agent: Option<String>,
    cwd: Option<String>,
    transcript_path: Option<String>,
    notification_type: Option<String>,
    /// Compositor window handle, if the hook already knows it. Named
    /// `niri_id` for wire compatibility with existing hook payloads; it
    /// carries a Hyprland address just as happily.
    niri_id: Option<String>,
}

/// Append the caller's PPID to the session ID so that forked Claude agents
/// (which inherit the same session_id) get distinct entries.  The PPID is
/// the PID of the Claude process that spawned this hook command, which
/// differs between a parent Claude and any agents it forks.
fn disambiguate_session_id(id: String) -> String {
    let ppid = std::os::unix::process::parent_id();
    format!("{id}-{ppid}")
}

fn normalize_session_name(session_name: Option<String>) -> Option<String> {
    session_name.and_then(|name| {
        let trimmed = name.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn should_ignore_hook_for_agent(agent: &str) -> bool {
    process_tree_has_noninteractive_agent(agent)
}

#[cfg(target_os = "linux")]
fn process_tree_has_noninteractive_agent(agent: &str) -> bool {
    let mut pid = std::process::id();

    while pid > 1 {
        if let Some(argv) = proc_cmdline(pid)
            && argv_is_noninteractive_agent(agent, &argv)
        {
            return true;
        }

        let Some(parent_pid) = proc_parent_pid(pid) else {
            break;
        };
        if parent_pid == pid {
            break;
        }
        pid = parent_pid;
    }

    false
}

#[cfg(not(target_os = "linux"))]
fn process_tree_has_noninteractive_agent(_agent: &str) -> bool {
    false
}

fn argv_is_noninteractive_agent(agent: &str, argv: &[String]) -> bool {
    match agent {
        "codex" => {
            argv_invokes_agent(argv, "codex")
                && argv
                    .iter()
                    .skip(1)
                    .any(|arg| arg == "exec" || arg == "app-server")
        }
        "claude" => {
            argv_invokes_agent(argv, "claude")
                && argv
                    .iter()
                    .skip(1)
                    .any(|arg| arg == "-p" || arg == "--print" || arg == "--no-session-persistence")
        }
        _ => false,
    }
}

fn argv_invokes_agent(argv: &[String], agent: &str) -> bool {
    argv.iter().take(3).any(|arg| {
        let basename = arg.rsplit('/').next().unwrap_or(arg);
        basename == agent
            || arg.contains(&format!("/{agent}/"))
            || arg.contains(&format!("{agent}-code"))
    })
}

#[cfg(target_os = "linux")]
fn proc_cmdline(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).to_string())
        .collect::<Vec<_>>();
    (!args.is_empty()).then_some(args)
}

#[cfg(target_os = "linux")]
fn proc_parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// Returns true on success, false on failure
pub fn handle_event(
    event: &str,
    agent_override: Option<&str>,
    session_name_override: Option<&str>,
) -> bool {
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

    let agent = match agent_override.map(str::to_string).or(hook.agent) {
        Some(agent) => agent,
        None => {
            eprintln!("Missing agent; pass --agent or include agent in hook payload");
            return false;
        }
    };

    if should_ignore_hook_for_agent(&agent) {
        return true;
    }

    let session_id = match hook.session_id {
        Some(id) => disambiguate_session_id(id),
        None => {
            eprintln!("Missing session_id");
            return false;
        }
    };

    let msg = daemon::TrackEvent {
        event,
        session_id,
        session_name: normalize_session_name(
            session_name_override
                .map(str::to_string)
                .or(hook.session_name),
        ),
        agent: Some(agent),
        cwd: hook.cwd,
        transcript_path: hook.transcript_path,
        notification_type: hook.notification_type,
        niri_id: hook
            .niri_id
            .or_else(|| compositor::get().focused_window_id().ok()),
    };

    match daemon::send_track_request(&msg) {
        Ok(()) => true,
        Err(err) => {
            eprintln!("Failed to send track event to daemon: {}", err);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn detects_codex_exec_as_noninteractive() {
        assert!(argv_is_noninteractive_agent(
            "codex",
            &argv(&["/home/me/.cargo/bin/codex", "exec", "review"]),
        ));
        assert!(argv_is_noninteractive_agent(
            "codex",
            &argv(&["codex", "exec", "--ephemeral", "review"]),
        ));
        assert!(argv_is_noninteractive_agent(
            "codex",
            &argv(&["node", "/opt/codex/bin/codex.js", "exec", "review"]),
        ));
        assert!(!argv_is_noninteractive_agent(
            "codex",
            &argv(&["/home/me/.cargo/bin/codex"]),
        ));
    }

    #[test]
    fn detects_codex_app_server_as_noninteractive() {
        assert!(argv_is_noninteractive_agent(
            "codex",
            &argv(&["/etc/profiles/per-user/me/bin/codex", "app-server"]),
        ));
        assert!(!argv_is_noninteractive_agent(
            "codex",
            &argv(&["codex", "--dangerously-bypass-approvals-and-sandbox"]),
        ));
    }

    #[test]
    fn detects_claude_print_as_noninteractive() {
        assert!(argv_is_noninteractive_agent(
            "claude",
            &argv(&["claude", "-p", "review"]),
        ));
        assert!(argv_is_noninteractive_agent(
            "claude",
            &argv(&["claude", "--print", "review"]),
        ));
        assert!(argv_is_noninteractive_agent(
            "claude",
            &argv(&["claude", "-p", "--no-session-persistence", "review"]),
        ));
        assert!(argv_is_noninteractive_agent(
            "claude",
            &argv(&["node", "/opt/claude-code/cli.js", "-p", "review"]),
        ));
        assert!(!argv_is_noninteractive_agent("claude", &argv(&["claude"]),));
    }
}
