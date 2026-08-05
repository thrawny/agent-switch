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
}

fn registry_path() -> std::path::PathBuf {
    state::state_file().with_file_name("sidebar-proto-registry.json")
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

fn bare_session_id(id: &str) -> &str {
    // track.rs appends "-<ppid>" for disambiguation; strip it for resume and
    // for rebinding a resumed session to its cold thread.
    match id.rfind('-') {
        Some(pos) if id[pos + 1..].chars().all(|c| c.is_ascii_digit()) => &id[..pos],
        _ => id,
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
            })
            .collect();
        if let Ok(json) = serde_json::to_string_pretty(&records)
            && let Err(err) = std::fs::write(registry_path(), json)
        {
            warn!("proto registry save failed: {err}");
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
                });

            let branch = session
                .cwd
                .as_deref()
                .map(|cwd| self.cached_branch(cwd))
                .unwrap_or_default();

            match idx {
                Some(idx) => {
                    let t = &mut self.threads[idx];
                    // The registry's live binding outlives producer-cache
                    // churn: Claude re-fires session-start on compaction, and
                    // the track hook re-keys sessions.json to whatever window
                    // was focused at that moment (2026-08-04 bug). Only adopt
                    // a different window once the bound one is actually gone.
                    let effective = match t.window_id {
                        Some(old) if windows.contains_key(&old) => Some(old),
                        _ => window_id,
                    };
                    let (window, parked, area) = derive(effective);
                    t.harness_session_id = session.session_id;
                    t.window_id = effective;
                    t.pid = window.and_then(|w| w.pid);
                    t.parked = parked;
                    t.state = session.state;
                    t.waiting_reason = session.waiting_reason;
                    t.state_updated = session.state_updated;
                    t.transcript_path = session.transcript_path;
                    t.branch = branch;
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
                    let title = window
                        .and_then(|w| w.title.clone())
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
                    });
                }
            }
        }

        // Only an actually-closed niri window makes a thread cold — a session
        // vanishing from sessions.json (compaction re-key, daemon cleanup)
        // must not: the registry keeps the thread, the binding stays live.
        for t in &mut self.threads {
            if t.window_id.is_some_and(|id| !windows.contains_key(&id)) {
                t.window_id = None;
                t.pid = None;
                t.parked = false;
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

    /// Toggle the *currently focused* window into the nirius scratchpad. The
    /// caller must have focused the thread's window first: scratchpad-toggle
    /// has no --id, pid-matching is useless against single-instance ghostty
    /// (one pid owns every window), and while the sidebar holds its exclusive
    /// keyboard grab niri reports no focused window at all — so parking goes
    /// through the sidebar's release-grab → focus → toggle → re-grab dance.
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
            "unarchived — restored to live".to_string()
        } else {
            t.archived_at = Some(now);
            t.settled_at = Some(now);
            "archived — tombstone on the z shelf (runtime/worktree untouched in proto)".to_string()
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
        let msg = format!("renamed to '{}'", t.title);
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
