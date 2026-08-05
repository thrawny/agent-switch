// PROTOTYPE — throwaway code, not production. Delete freely.
//
// Live backend for the ticket-06 sidebar prototype, built for ticket 08's
// window-hosted leg: does the sidebar + registry-shaped threads + niri verbs
// compose spatially? Threads are minted in-memory per ticket 04's manifest
// shape (registry identity, harness + harness_session_id + cwd + transcript,
// runtime = niri window) by joining real agent-switch sessions with live niri
// windows. Verbs act on the real desktop:
//   summon  — always a go-to, threads never move: parked → focus its area +
//             nirius scratchpad-show + tile there; visible → focus; cold
//             (window gone) → resurrect in its area: ghostty + harness
//             resume (03's cold path).
//   park    — nirius scratchpad-toggle --pid (ticket 02's mechanism).
//   new     — ghostty + pi in the focused workspace's nirius directory.
// Lifecycle (settle/archive), titles and read markers persist in a sidecar —
// `sidebar-proto-registry.json` next to sessions.json (PROTOTYPE — wipe me) —
// so archived tombstones and renames survive sidebar restarts. It is a crude
// stand-in for ticket 04's registry, not its design.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::time::Instant;

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::state::{self, SessionState, WaitingReason};
use crate::{daemon, niri};

pub struct LiveThread {
    pub seq: u64,
    /// User-owned sort key (Shift+J/K). Starts as seq (creation order,
    /// newest on top); manual reorder swaps it. seq stays pure identity.
    pub order: u64,
    pub harness: String,
    pub harness_session_id: String,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    pub area: String,
    pub title: String,
    pub repo: String,
    pub branch: String,
    pub window_id: Option<u64>,
    pub pid: Option<i32>,
    pub parked: bool,
    pub state: SessionState,
    pub waiting_reason: Option<WaitingReason>,
    pub state_updated: f64,
    pub last_read_at: f64,
    pub settled_at: Option<f64>,
    pub archived_at: Option<f64>,
    /// True once the user renamed the thread (r): the title is theirs and
    /// harness-provided session names stop updating it.
    pub renamed: bool,
}

impl LiveThread {
    pub fn cold(&self) -> bool {
        self.window_id.is_none()
    }
}

pub struct LiveWorld {
    threads: Vec<LiveThread>,
    next_seq: u64,
    branch_cache: HashMap<String, (Instant, String)>,
}

/// Persisted slice of a thread — what the sidecar registry keeps so identity,
/// lifecycle, title and read markers outlive both the window and the sidebar
/// process. Everything else is re-derived on refresh.
#[derive(Serialize, Deserialize)]
struct ProtoRecord {
    seq: u64,
    #[serde(default)]
    order: u64,
    harness: String,
    harness_session_id: String,
    cwd: Option<String>,
    transcript_path: Option<String>,
    area: String,
    title: String,
    state_updated: f64,
    last_read_at: f64,
    settled_at: Option<f64>,
    archived_at: Option<f64>,
    #[serde(default)]
    renamed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WaybarAttention {
    Done,
    Working,
    Approval,
    Input,
    Idle,
}

impl WaybarAttention {
    fn label(self) -> &'static str {
        match self {
            Self::Done => "Done",
            Self::Working => "Working",
            Self::Approval => "Approval",
            Self::Input => "Input",
            Self::Idle => "Idle",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Working => "⚙",
            Self::Approval => "!",
            Self::Input => "?",
            Self::Idle => "○",
        }
    }
}

#[derive(Serialize)]
struct WaybarOutput {
    text: String,
    tooltip: String,
    class: &'static str,
    updated_at: f64,
}

fn registry_path() -> std::path::PathBuf {
    state::state_file().with_file_name("sidebar-proto-registry.json")
}

fn waybar_status_path() -> std::path::PathBuf {
    state::state_file().with_file_name("sidebar-proto-waybar.json")
}

/// Go to the named area workspace. False when the area is unnamed ("other")
/// or its workspace no longer exists — callers decide what a homeless thread
/// gets (resurrect spawns here, unpark noops: threads never move).
fn focus_area(area: &str) -> bool {
    if area == "other" {
        return false;
    }
    let exists = niri::niri_workspaces()
        .into_iter()
        .any(|ws| ws.name.as_deref() == Some(area));
    if exists {
        info!("focusing area '{area}'");
        niri::niri_action(niri_ipc::Action::FocusWorkspace {
            reference: niri_ipc::WorkspaceReferenceArg::Name(area.to_string()),
        });
    }
    exists
}

/// Escape regex metacharacters so a live window title becomes an exact-match
/// pattern for nirius matchers.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if "\\.^$*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn bare_session_id(id: &str) -> &str {
    // track.rs appends "-<ppid>" for disambiguation; strip it for resume and
    // for rebinding a resumed session to its cold thread.
    match id.rfind('-') {
        Some(pos) if id[pos + 1..].chars().all(|c| c.is_ascii_digit()) => &id[..pos],
        _ => id,
    }
}

/// Push a sidebar rename down to the harness so its own session picker shows
/// the same name. Pi: drop the name in the renames dir — the agent-switch pi
/// extension watches it and applies `pi.setSessionName`; a cold session picks
/// it up on its next session-start (resume). Claude: append a `custom-title`
/// entry to the transcript JSONL — the same append the SDK's renameSession()
/// and /rename make (verified 2026-08-05); the resume picker reads the last
/// one. Codex has no session-name concept. Returns the harness name on
/// successful hand-off, None when there is no rename path.
fn propagate_rename(t: &LiveThread) -> Option<&'static str> {
    match t.harness.as_str() {
        "pi" => {
            let dir = state::state_file().with_file_name("renames");
            if let Err(err) = std::fs::create_dir_all(&dir) {
                warn!("rename propagate: create {}: {err}", dir.display());
                return None;
            }
            let path = dir.join(bare_session_id(&t.harness_session_id));
            match std::fs::write(&path, &t.title) {
                Ok(()) => Some("pi"),
                Err(err) => {
                    warn!("rename propagate: write {}: {err}", path.display());
                    None
                }
            }
        }
        "claude" => {
            use std::io::Write;
            let path = t.transcript_path.as_ref()?;
            let entry = serde_json::json!({
                "type": "custom-title",
                "customTitle": t.title,
                "sessionId": bare_session_id(&t.harness_session_id),
            });
            // O_APPEND keeps the single-line write atomic alongside the
            // harness's own appends.
            let result = std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .and_then(|mut f| writeln!(f, "{entry}"));
            match result {
                Ok(()) => Some("claude"),
                Err(err) => {
                    warn!("rename propagate: append {path}: {err}");
                    None
                }
            }
        }
        _ => None,
    }
}

fn scratchpad_window_ids() -> HashSet<u64> {
    let output = Command::new("nirius").arg("list-scratchpad").output();
    let Ok(output) = output else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            line.strip_prefix("id: ")?
                .split(',')
                .next()?
                .trim()
                .parse()
                .ok()
        })
        .collect()
}

fn git_branch(cwd: &str) -> String {
    let output = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

fn repo_name(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

/// Codex prefixes its live terminal title with an animated Braille spinner.
/// That activity glyph is ephemeral display state, not part of a stable
/// thread title (e.g. `⠇ agent-switch` should mint as `agent-switch`).
fn stable_window_title(title: &str) -> String {
    let title = title.trim();
    let Some(first) = title.chars().next() else {
        return String::new();
    };
    if ('\u{2800}'..='\u{28ff}').contains(&first) {
        title[first.len_utf8()..].trim_start().to_string()
    } else {
        title.to_string()
    }
}

fn spawn_detached(program: &str, args: &[String]) -> Result<(), String> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("{program}: {err}"))
}

/// Run a command to completion, folding a non-zero exit into an Err carrying
/// the command line + stderr (so verb failures surface in the footer AND the
/// zmx log).
fn run_cmd(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| format!("{program}: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

impl LiveWorld {
    pub fn new() -> Self {
        let mut world = Self {
            threads: Vec::new(),
            next_seq: 0,
            branch_cache: HashMap::new(),
        };
        world.load_registry();
        world.refresh();
        world
    }

    /// Mint threads from the sidecar registry (all cold until refresh rebinds
    /// them to live sessions/windows) — archives, renames and read markers
    /// survive sidebar restarts.
    fn load_registry(&mut self) {
        let Ok(data) = std::fs::read_to_string(registry_path()) else {
            return;
        };
        let Ok(mut records) = serde_json::from_str::<Vec<ProtoRecord>>(&data) else {
            warn!("proto registry unreadable — starting empty");
            return;
        };
        records.sort_by_key(|r| r.seq);
        for r in records {
            self.next_seq = self.next_seq.max(r.seq);
            let repo = r.cwd.as_deref().map(repo_name).unwrap_or_default();
            self.threads.push(LiveThread {
                seq: r.seq,
                // Sidecars written before `order` existed default it to 0.
                order: if r.order == 0 { r.seq } else { r.order },
                harness: r.harness,
                harness_session_id: r.harness_session_id,
                cwd: r.cwd,
                transcript_path: r.transcript_path,
                area: r.area,
                title: r.title,
                repo,
                branch: String::new(),
                window_id: None,
                pid: None,
                parked: false,
                state: SessionState::Idle,
                waiting_reason: None,
                state_updated: r.state_updated,
                last_read_at: r.last_read_at,
                settled_at: r.settled_at,
                archived_at: r.archived_at,
                renamed: r.renamed,
            });
        }
    }

    fn save(&self) {
        let records: Vec<ProtoRecord> = self
            .threads
            .iter()
            .map(|t| ProtoRecord {
                seq: t.seq,
                order: t.order,
                harness: t.harness.clone(),
                harness_session_id: t.harness_session_id.clone(),
                cwd: t.cwd.clone(),
                transcript_path: t.transcript_path.clone(),
                area: t.area.clone(),
                title: t.title.clone(),
                state_updated: t.state_updated,
                last_read_at: t.last_read_at,
                settled_at: t.settled_at,
                archived_at: t.archived_at,
                renamed: t.renamed,
            })
            .collect();
        if let Ok(json) = serde_json::to_string_pretty(&records)
            && let Err(err) = std::fs::write(registry_path(), json)
        {
            warn!("proto registry save failed: {err}");
        }
        self.save_waybar_status();
    }

    fn waybar_attention(t: &LiveThread) -> Option<WaybarAttention> {
        if t.archived_at.is_some() || t.settled_at.is_some() {
            return None;
        }
        Some(match t.state {
            SessionState::Responding => WaybarAttention::Working,
            SessionState::Waiting => match t.waiting_reason {
                Some(WaitingReason::PermissionPrompt) => WaybarAttention::Approval,
                None if t.state_updated > t.last_read_at => WaybarAttention::Input,
                None => WaybarAttention::Idle,
            },
            _ if t.state_updated > t.last_read_at => WaybarAttention::Done,
            _ => WaybarAttention::Idle,
        })
    }

    fn waybar_output(&self) -> WaybarOutput {
        let mut rows: Vec<_> = self
            .threads
            .iter()
            .filter_map(|thread| {
                Self::waybar_attention(thread).map(|attention| (attention, thread))
            })
            .collect();
        rows.sort_by_key(|(attention, thread)| (*attention, std::cmp::Reverse(thread.order)));

        let count = |wanted| {
            rows.iter()
                .filter(|(attention, _)| *attention == wanted)
                .count()
        };
        let done = count(WaybarAttention::Done);
        let working = count(WaybarAttention::Working);
        let idle = count(WaybarAttention::Idle);
        let (text, class) = if done > 0 {
            (format!("✓ {done}"), "done")
        } else if working > 0 {
            (format!("⚙ {working}"), "working")
        } else if idle > 0 {
            (format!("○ {idle}"), "idle")
        } else {
            (String::new(), "idle")
        };
        let tooltip = rows
            .iter()
            .map(|(attention, thread)| {
                let repo = if thread.repo.is_empty() {
                    "?"
                } else {
                    &thread.repo
                };
                format!(
                    "{} {} · {} ({})",
                    attention.glyph(),
                    thread.title,
                    repo,
                    attention.label()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        WaybarOutput {
            text,
            tooltip,
            class,
            updated_at: state::now(),
        }
    }

    fn save_waybar_status(&self) {
        let path = waybar_status_path();
        let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        let result = serde_json::to_vec(&self.waybar_output())
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(&temp, json))
            .and_then(|()| std::fs::rename(&temp, &path));
        if let Err(err) = result {
            let _ = std::fs::remove_file(temp);
            warn!("proto waybar status save failed: {err}");
        }
    }

    pub fn threads(&self) -> &[LiveThread] {
        &self.threads
    }

    fn get(&self, seq: u64) -> Option<&LiveThread> {
        self.threads.iter().find(|t| t.seq == seq)
    }

    fn get_mut(&mut self, seq: u64) -> Option<&mut LiveThread> {
        self.threads.iter_mut().find(|t| t.seq == seq)
    }

    fn cached_branch(&mut self, cwd: &str) -> String {
        if let Some((at, branch)) = self.branch_cache.get(cwd)
            && at.elapsed().as_secs() < 30
        {
            return branch.clone();
        }
        let branch = git_branch(cwd);
        self.branch_cache
            .insert(cwd.to_string(), (Instant::now(), branch.clone()));
        branch
    }

    /// Re-join sessions.json + niri windows/workspaces + nirius scratchpad
    /// into the in-memory registry. Identity (seq) is stable across refreshes;
    /// a session that reappears under a new ppid suffix (resume) rebinds to
    /// its cold thread instead of minting a new one.
    pub fn refresh(&mut self) {
        let mut store = match state::load_from_path(&state::state_file()) {
            Ok(store) => store,
            Err(_) => return,
        };
        daemon::refresh_transcript_derived_states(&mut store);

        let windows: HashMap<u64, niri_ipc::Window> = niri::niri_windows()
            .into_iter()
            .map(|w| (w.id, w))
            .collect();
        let workspace_names: HashMap<u64, Option<String>> = niri::niri_workspaces()
            .into_iter()
            .map(|ws| (ws.id, ws.name))
            .collect();
        let scratch = scratchpad_window_ids();

        // Oldest activity first so the initial mint approximates creation order
        // (sessions.json has no created_at — a real registry would).
        let mut sessions: Vec<_> = store.sessions.into_values().collect();
        sessions.sort_by(|a, b| a.state_updated.total_cmp(&b.state_updated));

        let now = state::now();
        // Which live window each store session claims, and which sessions
        // still exist at all — a binding held against a window another
        // session claims is stale, and a thread with neither window nor
        // session was closed outside the sidebar.
        let claimed: HashMap<u64, String> = sessions
            .iter()
            .filter_map(|s| {
                let id: u64 = s.window.niri_id.as_deref()?.parse().ok()?;
                windows
                    .contains_key(&id)
                    .then(|| (id, s.session_id.clone()))
            })
            .collect();
        let store_sids: HashSet<String> = sessions.iter().map(|s| s.session_id.clone()).collect();
        // Window/parked/area facts for a candidate binding.
        let derive = |id: Option<u64>| {
            let window = id.and_then(|i| windows.get(&i));
            let parked = id.map(|i| scratch.contains(&i)).unwrap_or(false);
            let area = window
                .filter(|_| !parked)
                .and_then(|w| w.workspace_id)
                .and_then(|ws| workspace_names.get(&ws).cloned().flatten());
            (window, parked, area)
        };
        for session in sessions {
            let window_id: Option<u64> = session
                .window
                .niri_id
                .as_deref()
                .and_then(|id| id.parse().ok())
                .filter(|id| windows.contains_key(id));

            let idx = self
                .threads
                .iter()
                .position(|t| t.harness_session_id == session.session_id)
                .or_else(|| {
                    // Resume rebind: a cold thread with the same bare id is
                    // the same conversation coming back in a new process.
                    self.threads.iter().position(|t| {
                        t.cold()
                            && t.harness == session.agent
                            && bare_session_id(&t.harness_session_id)
                                == bare_session_id(&session.session_id)
                    })
                })
                .or_else(|| {
                    // Window succession (2026-08-05): a new session claiming
                    // the window of a thread whose own session is gone is the
                    // same seat continuing (claude /clear + handoff, pi /new)
                    // — reuse the thread instead of releasing it into an
                    // auto-archive and minting a duplicate.
                    let id = window_id?;
                    if claimed.get(&id) != Some(&session.session_id) {
                        return None;
                    }
                    self.threads.iter().position(|t| {
                        t.window_id == Some(id) && !store_sids.contains(&t.harness_session_id)
                    })
                });

            let branch = session
                .cwd
                .as_deref()
                .map(|cwd| self.cached_branch(cwd))
                .unwrap_or_default();

            match idx {
                Some(idx) => {
                    let t = &mut self.threads[idx];
                    // Archive just closed this thread's window; give the
                    // session-end hook (and ghostty's close-confirm) a grace
                    // window before believing the lingering session again.
                    // Past the grace, a still-alive session means the reclaim
                    // failed — the rebind below un-archives honestly.
                    let reclaiming = t.archived_at.is_some_and(|at| now - at < 10.0);
                    // The registry's live binding outlives producer-cache
                    // churn: Claude re-fires session-start on compaction, and
                    // the track hook re-keys sessions.json to whatever window
                    // was focused at that moment (2026-08-04 bug). Only adopt
                    // a different window once the bound one is gone — or
                    // claimed by a different session (pi /new reuses the
                    // window; the old thread must not squat on it).
                    let effective = match t.window_id {
                        _ if reclaiming => None,
                        Some(old)
                            if windows.contains_key(&old)
                                && claimed
                                    .get(&old)
                                    .is_none_or(|sid| *sid == session.session_id) =>
                        {
                            Some(old)
                        }
                        _ => window_id,
                    };
                    let (window, parked, area) = derive(effective);
                    if t.window_id.is_none() && effective.is_some() {
                        // Runtime came back (resurrection rebind) — a revived
                        // thread is no tombstone.
                        t.archived_at = None;
                    }
                    // Succession can hand the seat to a different harness
                    // (quit claude, start pi in the same terminal) — the
                    // thread follows the seat, so its harness and cwd do too.
                    let succeeded = bare_session_id(&t.harness_session_id)
                        != bare_session_id(&session.session_id);
                    t.harness = session.agent;
                    t.harness_session_id = session.session_id;
                    if session.cwd.is_some() {
                        t.cwd = session.cwd;
                    }
                    t.window_id = effective;
                    t.pid = window.and_then(|w| w.pid);
                    t.parked = parked;
                    t.state = session.state;
                    t.waiting_reason = session.waiting_reason;
                    t.state_updated = session.state_updated;
                    t.transcript_path = session.transcript_path;
                    // Reconcile a manual rename across succession: the new
                    // conversation's transcript has never seen the title, so
                    // re-assert it. Resume-rebinds (same bare id, same
                    // transcript) already carry it.
                    if succeeded && t.renamed {
                        propagate_rename(t);
                    }
                    t.branch = branch;
                    // Harness session names (pi --name, claude) are the
                    // default title; a manual rename owns it forever. Heal
                    // older Codex rows minted while its Braille spinner was
                    // present in the terminal title.
                    if !t.renamed {
                        if let Some(name) = &session.session_name {
                            t.title = name.clone();
                        } else {
                            let stable = stable_window_title(&t.title);
                            if !stable.is_empty() {
                                t.title = stable;
                            }
                        }
                    }
                    if let Some(area) = area {
                        t.area = area;
                    }
                    // Hand-raise un-settles (03): attention-worthy events wake
                    // a settled thread; attention-free time never does.
                    if let Some(settled_at) = t.settled_at
                        && t.state_updated > settled_at
                    {
                        t.settled_at = None;
                    }
                    // Being in the window IS reading it — "jumped to it"
                    // counts however you got there (sidebar, alt-tab, mouse).
                    if window.is_some_and(|w| w.is_focused) {
                        t.last_read_at = now;
                    }
                }
                None => {
                    let (window, parked, area) = derive(window_id);
                    self.next_seq += 1;
                    let repo = session.cwd.as_deref().map(repo_name).unwrap_or_default();
                    // Default title precedence: harness session name (pi
                    // --name, claude) > window title > repo name.
                    let title = session
                        .session_name
                        .clone()
                        .or_else(|| {
                            window
                                .and_then(|w| w.title.as_deref())
                                .map(stable_window_title)
                                .filter(|title| !title.is_empty())
                        })
                        .unwrap_or_else(|| repo.clone());
                    self.threads.push(LiveThread {
                        seq: self.next_seq,
                        order: self.next_seq,
                        harness: session.agent,
                        harness_session_id: session.session_id,
                        cwd: session.cwd,
                        transcript_path: session.transcript_path,
                        area: area.unwrap_or_else(|| "other".to_string()),
                        title,
                        repo,
                        branch,
                        window_id,
                        pid: window.and_then(|w| w.pid),
                        parked,
                        state: session.state,
                        waiting_reason: session.waiting_reason,
                        state_updated: session.state_updated,
                        // Never-visited counts as read (06); only activity
                        // after discovery turns the row unread.
                        last_read_at: session.state_updated,
                        settled_at: None,
                        archived_at: None,
                        renamed: false,
                    });
                }
            }
        }

        // A thread goes cold when its window actually closes OR a session
        // that is still alive elsewhere holds a stale claim on the window
        // (compaction re-key). Dead-session takeovers never reach here —
        // window succession above rebinds the thread to the new session. A
        // session merely vanishing from sessions.json (compaction re-key)
        // still doesn't release a live binding.
        for t in &mut self.threads {
            let gone = t.window_id.is_some_and(|id| !windows.contains_key(&id));
            let taken = t.window_id.is_some_and(|id| {
                claimed
                    .get(&id)
                    .is_some_and(|sid| *sid != t.harness_session_id)
            });
            if gone || taken {
                t.window_id = None;
                t.pid = None;
                t.parked = false;
            }
            // Closed outside the sidebar = archive (user call, 2026-08-05):
            // no window and no session left means the user closed it or the
            // harness moved on — tombstone it; Enter on the shelf revives.
            // The read-marker grace keeps a just-summoned resurrection from
            // being re-tombstoned before its session-start lands.
            if t.window_id.is_none()
                && t.archived_at.is_none()
                && !store_sids.contains(&t.harness_session_id)
                && now - t.last_read_at > 15.0
            {
                info!(
                    "auto-archive: '{}' (#{}) lost window+session outside the sidebar",
                    t.title, t.seq
                );
                t.archived_at = Some(now);
                t.settled_at.get_or_insert(now);
                t.last_read_at = now;
            }
        }
        self.save();
    }

    pub fn summon(&mut self, seq: u64) -> String {
        let now = state::now();
        let Some(t) = self.get_mut(seq) else {
            return "no such thread".into();
        };
        t.last_read_at = now;
        t.settled_at = None;
        // Summoning a tombstone revives it (03's unarchive-first, folded in).
        t.archived_at = None;

        let msg = match (t.window_id, t.parked) {
            (Some(id), true) => {
                // Threads never move (user call, 2026-08-04): unpark = go to
                // the thread's area, then show + tile there (02's recipe —
                // tiling also evicts nirius scratchpad membership). "other"
                // threads have no home yet, so they unpark wherever you are;
                // a named area whose workspace is gone noops instead.
                let went = focus_area(&t.area);
                if !went && t.area != "other" {
                    return format!("area '{}' has no workspace — not unparking", t.area);
                }
                info!(
                    "summon: unparking window {id} ('{}') in '{}'",
                    t.title, t.area
                );
                if let Err(err) = run_cmd("nirius", &["scratchpad-show", "--id", &id.to_string()]) {
                    warn!("summon: {err}");
                    return err;
                }
                niri::niri_action(niri_ipc::Action::MoveWindowToTiling { id: Some(id) });
                t.parked = false;
                if went {
                    format!("summoned '{}' in area '{}'", t.title, t.area)
                } else {
                    format!("summoned '{}' here (no home area)", t.title)
                }
            }
            (Some(id), false) => {
                info!("summon: focusing window {id} ('{}')", t.title);
                if niri::focus_window(id) {
                    format!("went to '{}'", t.title)
                } else {
                    warn!("summon: focus-window {id} not handled");
                    format!("focus-window {id} failed")
                }
            }
            (None, _) => self.resurrect(seq),
        };
        self.save();
        msg
    }

    /// Cold path from ticket 03: recreate the window from the manifest via
    /// harness resume. The session-start hook in the new window rebinds it.
    fn resurrect(&mut self, seq: u64) -> String {
        let Some(t) = self.get(seq) else {
            return "no such thread".into();
        };
        let Some(cwd) = t.cwd.clone() else {
            return "cold thread has no cwd — cannot resurrect".into();
        };
        // Reopen in the thread's own area, not wherever the user happens to
        // be: cold summon is a go-to (the window belongs to its area), so
        // focus that workspace first and let the spawn land there.
        focus_area(&t.area);
        let resume: Vec<String> = match t.harness.as_str() {
            "pi" => match &t.transcript_path {
                Some(path) => vec!["pi".into(), "--session".into(), path.clone()],
                None => return "pi thread has no transcript_path".into(),
            },
            "claude" => vec![
                "claude".into(),
                "--resume".into(),
                bare_session_id(&t.harness_session_id).to_string(),
            ],
            "codex" => vec![
                "codex".into(),
                "resume".into(),
                bare_session_id(&t.harness_session_id).to_string(),
            ],
            other => return format!("no resume recipe for harness '{other}'"),
        };
        let mut args = vec![format!("--working-directory={cwd}"), "-e".to_string()];
        args.extend(resume.iter().cloned());
        info!("resurrect: ghostty {}", args.join(" "));
        match spawn_detached("ghostty", &args) {
            Ok(()) => format!("resurrecting: ghostty -e {}", resume.join(" ")),
            Err(err) => {
                warn!("resurrect: {err}");
                format!("resurrect failed: {err}")
            }
        }
    }

    /// Park without moving the user: scratchpad-toggle has no --id, but its
    /// matchers (--workspace-id + exact-title regex) select the window from
    /// anywhere — no focus, no grab release, you stay in your area. Fails
    /// (→ caller falls back to the focus dance) when the title changed
    /// between poll and toggle (working threads animate their titles) or the
    /// match is ambiguous.
    pub fn park_in_place(&mut self, seq: u64, verb: &str) -> Result<String, String> {
        let Some(t) = self.get_mut(seq) else {
            return Err("no such thread".into());
        };
        let Some(id) = t.window_id else {
            return Err("no window — nothing to park (cold)".into());
        };
        let window = niri::niri_windows().into_iter().find(|w| w.id == id);
        let Some(window) = window else {
            return Err("window vanished".into());
        };
        let (Some(ws), Some(title)) = (window.workspace_id, window.title) else {
            return Err("window has no workspace/title to match on".into());
        };
        let pattern = format!("^{}$", regex_escape(&title));
        run_cmd(
            "nirius",
            &[
                "scratchpad-toggle",
                "--workspace-id",
                &ws.to_string(),
                "--title",
                &pattern,
            ],
        )
        .map_err(|err| {
            warn!("{verb}: in-place park missed ({err}) — falling back to focus dance");
            err
        })?;
        info!(
            "{verb}: in-place scratchpad-toggle on window {id} ('{}')",
            t.title
        );
        t.parked = true;
        let msg = format!("{verb} '{}' (in place)", t.title);
        self.save();
        Ok(msg)
    }

    /// Fallback when the matcher park misses: toggle the *currently focused*
    /// window into the nirius scratchpad. The caller must have focused the
    /// thread's window first — pid-matching is useless against
    /// single-instance ghostty (one pid owns every window), and while the
    /// sidebar holds its exclusive keyboard grab niri reports no focused
    /// window at all — so this goes through the sidebar's release-grab →
    /// focus → toggle → return home → re-grab dance.
    pub fn park_focused(&mut self, seq: u64, verb: &str) -> String {
        let Some(t) = self.get_mut(seq) else {
            return "no such thread".into();
        };
        info!(
            "{verb}: scratchpad-toggle on focused window ('{}')",
            t.title
        );
        match run_cmd("nirius", &["scratchpad-toggle"]) {
            Ok(()) => {
                t.parked = true;
                format!("{verb} '{}' (nirius scratchpad)", t.title)
            }
            Err(err) => {
                warn!("{verb}: {err}");
                err
            }
        }
    }

    pub fn toggle_settle(&mut self, seq: u64) -> String {
        let now = state::now();
        let Some(t) = self.get_mut(seq) else {
            return "no such thread".into();
        };
        if t.archived_at.is_some() {
            return "archived threads: a to unarchive".into();
        }
        let msg = if t.settled_at.is_some() {
            // Bit-only: un-settle never un-parks (03) — summon is what shows.
            t.settled_at = None;
            "un-settled — back in the active list (window stays hidden)".to_string()
        } else {
            t.settled_at = Some(now);
            t.last_read_at = now;
            "settled — row moved to the shelf".to_string()
        };
        self.save();
        msg
    }

    pub fn toggle_archive(&mut self, seq: u64) -> String {
        let now = state::now();
        let Some(t) = self.get_mut(seq) else {
            return "no such thread".into();
        };
        let msg = if t.archived_at.is_some() {
            t.archived_at = None;
            "unarchived — restored to live (cold; ⏎ resurrects)".to_string()
        } else {
            t.archived_at = Some(now);
            t.settled_at = Some(now);
            t.last_read_at = now;
            // Archive is the reclaim verb (03): close the runtime too. The
            // transcript keeps the conversation; ⏎ on the tombstone revives.
            // CloseWindow by id is grab-safe (no focus involved); the harness
            // exits with its terminal, session-end fires, and the sweep sees
            // the window gone. Worktree reclaim stays out of proto scope.
            match t.window_id.take() {
                Some(id) => {
                    info!("archive: closing window {id} ('{}')", t.title);
                    t.pid = None;
                    t.parked = false;
                    niri::niri_action(niri_ipc::Action::CloseWindow { id: Some(id) });
                    "archived — window closed, tombstone on the z shelf".to_string()
                }
                None => "archived — tombstone on the z shelf".to_string(),
            }
        };
        self.save();
        msg
    }

    pub fn toggle_read(&mut self, seq: u64) -> String {
        let now = state::now();
        let Some(t) = self.get_mut(seq) else {
            return "no such thread".into();
        };
        let msg = if t.state_updated > t.last_read_at {
            t.last_read_at = now;
            "marked read".to_string()
        } else {
            t.last_read_at = 0.0;
            "marked unread".to_string()
        };
        self.save();
        msg
    }

    /// Manual rename (04's user-owned `title`): sticks across refreshes and,
    /// via the sidecar, across restarts. Harness-derived titles never
    /// overwrite it — refresh only sets title at mint.
    pub fn rename(&mut self, seq: u64, title: String) -> String {
        let Some(t) = self.get_mut(seq) else {
            return "no such thread".into();
        };
        t.title = title;
        t.renamed = true;
        let msg = match propagate_rename(t) {
            Some(harness) => format!("renamed to '{}' (→ {harness})", t.title),
            None => format!("renamed to '{}'", t.title),
        };
        self.save();
        msg
    }

    /// Manual reorder (Shift+J/K): swap the user-owned sort keys of two
    /// threads. The UI picks the display-order neighbor.
    pub fn swap_order(&mut self, a: u64, b: u64) -> String {
        let (Some(ia), Some(ib)) = (
            self.threads.iter().position(|t| t.seq == a),
            self.threads.iter().position(|t| t.seq == b),
        ) else {
            return "no such thread".into();
        };
        let order_a = self.threads[ia].order;
        self.threads[ia].order = self.threads[ib].order;
        self.threads[ib].order = order_a;
        self.save();
        "reordered".into()
    }

    /// Minimal creation scaffolding (creation flow design is its own ticket):
    /// spawn ghostty + pi in the focused workspace's nirius directory.
    pub fn new_thread(&self) -> String {
        let dir = Command::new("nirius")
            .arg("get-workspace-directory")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().display().to_string());
        let args = vec![
            format!("--working-directory={dir}"),
            "-e".to_string(),
            "pi".to_string(),
        ];
        info!("new thread: ghostty {}", args.join(" "));
        match spawn_detached("ghostty", &args) {
            Ok(()) => format!("spawning pi in {dir}"),
            Err(err) => {
                warn!("new thread: {err}");
                format!("spawn failed: {err}")
            }
        }
    }
}

/// Name of the focused workspace, if it has one — the sidebar's area scope.
pub fn focused_area() -> Option<String> {
    niri::niri_workspaces()
        .into_iter()
        .find(|ws| ws.is_focused)
        .and_then(|ws| ws.name)
}

#[cfg(test)]
mod tests {
    use super::stable_window_title;

    #[test]
    fn stable_window_title_strips_codex_braille_spinner() {
        assert_eq!(stable_window_title("⠇ agent-switch"), "agent-switch");
        assert_eq!(stable_window_title("agent-switch"), "agent-switch");
    }
}
