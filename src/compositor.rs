//! Compositor abstraction.
//!
//! agent-switch binds agent sessions to windows, garbage-collects sessions
//! whose window died, and drives a sidebar that focuses, parks and closes
//! those windows. Every one of those verbs is compositor-specific, and there
//! are now two compositors in play: niri and Hyprland.
//!
//! The trait below is deliberately verb-shaped rather than model-shaped —
//! exactly the operations the call sites in `track.rs`, `main.rs`, `state.rs`
//! and the live sidebar perform, nothing speculative. niri's scrolling layout
//! (columns, `MoveWindowToTiling`, the nirius scratchpad daemon) and
//! Hyprland's special workspaces both flatten to the same handful of verbs;
//! the column model and the GTK overlay in `niri.rs` stay niri-only because
//! nothing outside that overlay needs them.
//!
//! Window and workspace handles are opaque strings: niri numbers its windows,
//! Hyprland addresses them (`0x55d2…`). `state::WindowId::niri_id` has always
//! been a `String`, so both fit the on-disk shape without a migration.

use std::collections::HashSet;
use std::process::Command;
use std::sync::OnceLock;

use crate::niri;

/// Hyprland special workspace agent-switch parks windows into. niri parks via
/// the nirius scratchpad daemon instead, which has no workspace name.
const HYPRLAND_PARK_WORKSPACE: &str = "special:agentpark";

/// One live toplevel window, in compositor-agnostic terms.
#[derive(Debug, Clone)]
pub struct CompWindow {
    /// Stable handle for this window: niri's numeric id, Hyprland's address.
    pub id: String,
    pub title: Option<String>,
    /// Opaque workspace handle; matches some `CompWorkspace::id`.
    pub workspace_id: Option<String>,
    pub pid: Option<i32>,
    pub focused: bool,
}

/// One workspace ("area" in sidebar terms).
#[derive(Debug, Clone)]
pub struct CompWorkspace {
    pub id: String,
    pub name: Option<String>,
    pub focused: bool,
}

pub trait Compositor: Send + Sync {
    /// Backend name, for log lines that used to hardcode "niri".
    fn name(&self) -> &'static str;

    /// Handle of the focused window. `Err` carries a human-readable reason —
    /// probe failure and "nothing focused" are not distinguished, because no
    /// call site treats them differently.
    fn focused_window_id(&self) -> Result<String, String>;

    /// Best-effort window list: empty when the compositor cannot be reached.
    /// Used by the sidebar, which re-polls every second and tolerates a gap.
    fn windows(&self) -> Vec<CompWindow>;

    /// Window handles for stale-session GC. `Err` means "the probe failed" and
    /// must *not* be read as "no windows exist" — that would delete every
    /// bound session in the store.
    fn live_window_ids(&self) -> Result<HashSet<String>, String>;

    /// Best-effort workspace list, empty on failure.
    fn workspaces(&self) -> Vec<CompWorkspace>;

    /// Returns false when the compositor did not handle the focus request
    /// (typically: the window is gone).
    fn focus_window(&self, id: &str) -> bool;

    fn close_window(&self, id: &str);

    fn focus_workspace_by_name(&self, name: &str);

    fn focus_workspace_by_id(&self, id: &str);

    /// Handles of windows currently parked out of sight.
    fn parked_window_ids(&self) -> HashSet<String>;

    /// Bring a parked window onto the *currently focused* workspace and leave
    /// it visible there. Callers focus the destination workspace first —
    /// threads never move on their own.
    fn summon_parked_window(&self, id: &str) -> Result<(), String>;

    /// Park a window without moving the user: no focus change, no workspace
    /// switch. `Err` means the backend could not address the window from afar
    /// and the caller should fall back to [`Compositor::park_focused_window`].
    fn park_window_in_place(&self, window: &CompWindow) -> Result<(), String>;

    /// Park whatever is focused right now. The caller must have focused the
    /// target window first.
    fn park_focused_window(&self) -> Result<(), String>;

    /// Project directory registered for the focused workspace, if the
    /// compositor tracks one.
    fn project_dir_for_focused(&self) -> Option<String>;
}

/// Pick a backend from the environment. niri wins when both markers are
/// present, and niri is also the fallback so behavior outside any compositor
/// (tests, headless daemon) is unchanged.
pub fn detect() -> Box<dyn Compositor> {
    if std::env::var_os("NIRI_SOCKET").is_some() {
        return Box::new(NiriCompositor);
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Box::new(HyprlandCompositor);
    }
    Box::new(NiriCompositor)
}

/// Process-wide backend, detected once.
pub fn get() -> &'static dyn Compositor {
    static COMPOSITOR: OnceLock<Box<dyn Compositor>> = OnceLock::new();
    COMPOSITOR.get_or_init(detect).as_ref()
}

/// Run a command to completion, folding a non-zero exit into an `Err` carrying
/// the command line + stderr (so verb failures surface in the sidebar footer
/// AND the log).
pub(crate) fn run_cmd(program: &str, args: &[&str]) -> Result<(), String> {
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

/// Capture a command's stdout, or `None` when it fails to run or exits non-zero.
fn cmd_stdout(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(program).args(args).output().ok()?;
    output.status.success().then_some(output.stdout)
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

/// Shared failure text for the window probes that back stale-session GC. The
/// wording predates the abstraction and is kept so the existing warn line
/// reads the same under niri.
fn probe_error(backend: &'static str, detail: String) -> String {
    format!("{backend} probe failed: {detail}")
}

fn probe_command_failed(backend: &'static str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        format!("command exited with status {}", output.status)
    } else {
        format!("command exited with status {}: {}", output.status, stderr)
    };
    probe_error(backend, detail)
}

// ---------------------------------------------------------------------------
// niri
// ---------------------------------------------------------------------------

/// niri backend. Queries and actions go through the existing `niri.rs` IPC
/// layer (the `niri-ipc` crate) except where the pre-abstraction code shelled
/// out to `niri msg` — those keep shelling out, because the socket and the
/// subprocess differ in how they fail and the stale-GC path depends on that
/// difference. Parking is the nirius scratchpad daemon, as before.
pub struct NiriCompositor;

impl NiriCompositor {
    fn window_id(id: &str) -> Option<u64> {
        id.parse().ok()
    }
}

impl Compositor for NiriCompositor {
    fn name(&self) -> &'static str {
        "niri"
    }

    fn focused_window_id(&self) -> Result<String, String> {
        let output = Command::new("niri")
            .args(["msg", "--json", "focused-window"])
            .output()
            .map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|err| err.to_string())?;
        value
            .get("id")
            .and_then(|id| id.as_u64())
            .map(|id| id.to_string())
            .ok_or_else(|| "focused window JSON did not contain numeric id".to_string())
    }

    fn windows(&self) -> Vec<CompWindow> {
        niri::niri_windows()
            .into_iter()
            .map(|w| CompWindow {
                id: w.id.to_string(),
                title: w.title,
                workspace_id: w.workspace_id.map(|id| id.to_string()),
                pid: w.pid,
                focused: w.is_focused,
            })
            .collect()
    }

    fn live_window_ids(&self) -> Result<HashSet<String>, String> {
        let output = Command::new("niri")
            .args(["msg", "-j", "windows"])
            .output()
            .map_err(|err| probe_error("niri", err.to_string()))?;
        if !output.status.success() {
            return Err(probe_command_failed("niri", &output));
        }
        let windows =
            serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout).map_err(|err| {
                probe_error("niri", format!("failed to parse backend output: {}", err))
            })?;
        Ok(windows
            .into_iter()
            .filter_map(|w| w.get("id").and_then(|v| v.as_u64()))
            .map(|id| id.to_string())
            .collect())
    }

    fn workspaces(&self) -> Vec<CompWorkspace> {
        niri::niri_workspaces()
            .into_iter()
            .map(|ws| CompWorkspace {
                id: ws.id.to_string(),
                name: ws.name,
                focused: ws.is_focused,
            })
            .collect()
    }

    fn focus_window(&self, id: &str) -> bool {
        Self::window_id(id).is_some_and(niri::focus_window)
    }

    fn close_window(&self, id: &str) {
        if let Some(id) = Self::window_id(id) {
            niri::niri_action(niri_ipc::Action::CloseWindow { id: Some(id) });
        }
    }

    fn focus_workspace_by_name(&self, name: &str) {
        niri::niri_action(niri_ipc::Action::FocusWorkspace {
            reference: niri_ipc::WorkspaceReferenceArg::Name(name.to_string()),
        });
    }

    fn focus_workspace_by_id(&self, id: &str) {
        if let Some(id) = Self::window_id(id) {
            niri::niri_action(niri_ipc::Action::FocusWorkspace {
                reference: niri_ipc::WorkspaceReferenceArg::Id(id),
            });
        }
    }

    fn parked_window_ids(&self) -> HashSet<String> {
        let Some(stdout) = cmd_stdout("nirius", &["list-scratchpad"]) else {
            return HashSet::new();
        };
        String::from_utf8_lossy(&stdout)
            .lines()
            .filter_map(|line| {
                Some(
                    line.strip_prefix("id: ")?
                        .split(',')
                        .next()?
                        .trim()
                        .to_string(),
                )
            })
            .filter(|id| id.parse::<u64>().is_ok())
            .collect()
    }

    fn summon_parked_window(&self, id: &str) -> Result<(), String> {
        run_cmd("nirius", &["scratchpad-show", "--id", id])?;
        // Tiling also evicts nirius scratchpad membership, so the window stops
        // being parked as a side effect of landing in the layout.
        if let Some(id) = Self::window_id(id) {
            niri::niri_action(niri_ipc::Action::MoveWindowToTiling { id: Some(id) });
        }
        Ok(())
    }

    fn park_window_in_place(&self, window: &CompWindow) -> Result<(), String> {
        // scratchpad-toggle has no --id, but its matchers (--workspace-id +
        // exact-title regex) select the window from anywhere — no focus, no
        // grab release. Fails when the title changed between poll and toggle
        // (working threads animate their titles) or the match is ambiguous.
        let (Some(ws), Some(title)) = (window.workspace_id.as_deref(), window.title.as_deref())
        else {
            return Err("window has no workspace/title to match on".into());
        };
        let pattern = format!("^{}$", regex_escape(title));
        run_cmd(
            "nirius",
            &[
                "scratchpad-toggle",
                "--workspace-id",
                ws,
                "--title",
                &pattern,
            ],
        )
    }

    fn park_focused_window(&self) -> Result<(), String> {
        run_cmd("nirius", &["scratchpad-toggle"])
    }

    fn project_dir_for_focused(&self) -> Option<String> {
        cmd_stdout("nirius", &["get-workspace-directory"])
            .map(|stdout| String::from_utf8_lossy(&stdout).trim().to_string())
            .filter(|dir| !dir.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Hyprland
// ---------------------------------------------------------------------------

/// Hyprland backend, driven entirely through `hyprctl` subprocesses. That
/// matches the style of the nirius calls it replaces and avoids carrying a
/// hand-rolled socket protocol; nothing here needs an event stream, since the
/// only event-stream consumer (`niri::start_focus_tracker`) feeds the
/// niri-only GTK overlay.
pub struct HyprlandCompositor;

/// Hyprland window addresses arrive with a `0x` prefix from `hyprctl -j` and
/// without one from the socket2 event stream. Normalize to the prefixed form
/// so a stored session id matches a live window whichever way it was read.
fn normalize_address(address: &str) -> String {
    let address = address.trim();
    let bare = address.strip_prefix("0x").unwrap_or(address);
    format!("0x{bare}")
}

impl HyprlandCompositor {
    fn dispatch(&self, args: &[&str]) -> Result<(), String> {
        let mut argv = vec!["dispatch"];
        argv.extend_from_slice(args);
        run_cmd("hyprctl", &argv)
    }

    fn json(&self, kind: &str) -> Option<serde_json::Value> {
        let stdout = cmd_stdout("hyprctl", &["-j", kind])?;
        serde_json::from_slice(&stdout).ok()
    }

    /// `hyprctl -j clients`, with the probe failure preserved for stale GC.
    fn clients(&self) -> Result<Vec<serde_json::Value>, String> {
        let output = Command::new("hyprctl")
            .args(["-j", "clients"])
            .output()
            .map_err(|err| probe_error("hyprland", err.to_string()))?;
        if !output.status.success() {
            return Err(probe_command_failed("hyprland", &output));
        }
        serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout).map_err(|err| {
            probe_error(
                "hyprland",
                format!("failed to parse backend output: {}", err),
            )
        })
    }

    fn active_window_address(&self) -> Option<String> {
        self.json("activewindow")?
            .get("address")?
            .as_str()
            .filter(|address| !address.is_empty())
            .map(normalize_address)
    }

    fn active_workspace_id(&self) -> Option<String> {
        self.json("activeworkspace")?
            .get("id")?
            .as_i64()
            .map(|id| id.to_string())
    }

    fn client_window(value: &serde_json::Value, focused: Option<&str>) -> Option<CompWindow> {
        let id = normalize_address(value.get("address")?.as_str()?);
        let workspace = value.get("workspace");
        Some(CompWindow {
            focused: focused.is_some_and(|address| address == id),
            id,
            title: value
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            workspace_id: workspace
                .and_then(|ws| ws.get("id"))
                .and_then(|v| v.as_i64())
                .map(|id| id.to_string()),
            pid: value
                .get("pid")
                .and_then(|v| v.as_i64())
                .and_then(|pid| i32::try_from(pid).ok())
                .filter(|pid| *pid > 0),
        })
    }

    fn client_is_parked(value: &serde_json::Value) -> bool {
        value
            .get("workspace")
            .and_then(|ws| ws.get("name"))
            .and_then(|v| v.as_str())
            == Some(HYPRLAND_PARK_WORKSPACE)
    }

    fn address_arg(id: &str) -> String {
        format!("address:{}", normalize_address(id))
    }

    fn window_exists(&self, id: &str) -> bool {
        self.live_window_ids()
            .is_ok_and(|ids| ids.contains(&normalize_address(id)))
    }
}

impl Compositor for HyprlandCompositor {
    fn name(&self) -> &'static str {
        "hyprland"
    }

    fn focused_window_id(&self) -> Result<String, String> {
        self.active_window_address()
            .ok_or_else(|| "hyprctl -j activewindow reported no focused window".to_string())
    }

    fn windows(&self) -> Vec<CompWindow> {
        let Ok(clients) = self.clients() else {
            return Vec::new();
        };
        let focused = self.active_window_address();
        clients
            .iter()
            .filter_map(|value| Self::client_window(value, focused.as_deref()))
            .collect()
    }

    fn live_window_ids(&self) -> Result<HashSet<String>, String> {
        Ok(self
            .clients()?
            .iter()
            .filter_map(|value| value.get("address")?.as_str())
            .map(normalize_address)
            .collect())
    }

    fn workspaces(&self) -> Vec<CompWorkspace> {
        let Some(serde_json::Value::Array(workspaces)) = self.json("workspaces") else {
            return Vec::new();
        };
        let active = self.active_workspace_id();
        workspaces
            .iter()
            .filter_map(|ws| {
                let id = ws.get("id")?.as_i64()?.to_string();
                Some(CompWorkspace {
                    focused: active.as_deref() == Some(id.as_str()),
                    name: ws
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|name| !name.is_empty())
                        .map(str::to_string),
                    id,
                })
            })
            .collect()
    }

    fn focus_window(&self, id: &str) -> bool {
        // Hyprland dispatchers are fire-and-forget: `focuswindow` on a dead
        // address still answers "ok". The sidebar reads a false here as "the
        // window is gone" (niri's semantics), so check before dispatching
        // rather than reporting a focus that never happened.
        if !self.window_exists(id) {
            return false;
        }
        self.dispatch(&["focuswindow", &Self::address_arg(id)])
            .is_ok()
    }

    fn close_window(&self, id: &str) {
        if let Err(err) = self.dispatch(&["closewindow", &Self::address_arg(id)]) {
            log::warn!("hyprland close_window: {err}");
        }
    }

    fn focus_workspace_by_name(&self, name: &str) {
        // Prefer the id: `workspace name:3` on a default-named workspace can
        // create a *new* named workspace instead of switching to the existing
        // numeric one. Resolving through the live list avoids that, and keeps
        // the name path only for workspaces that really are named.
        let by_id = self
            .workspaces()
            .into_iter()
            .find(|ws| ws.name.as_deref() == Some(name))
            .map(|ws| ws.id)
            .filter(|id| id.parse::<u32>().is_ok());
        let selector = by_id.unwrap_or_else(|| format!("name:{name}"));
        if let Err(err) = self.dispatch(&["workspace", &selector]) {
            log::warn!("hyprland focus_workspace_by_name: {err}");
        }
    }

    fn focus_workspace_by_id(&self, id: &str) {
        if let Err(err) = self.dispatch(&["workspace", id]) {
            log::warn!("hyprland focus_workspace_by_id: {err}");
        }
    }

    fn parked_window_ids(&self) -> HashSet<String> {
        let Ok(clients) = self.clients() else {
            return HashSet::new();
        };
        clients
            .iter()
            .filter(|value| Self::client_is_parked(value))
            .filter_map(|value| value.get("address")?.as_str())
            .map(normalize_address)
            .collect()
    }

    fn summon_parked_window(&self, id: &str) -> Result<(), String> {
        // The caller already focused the destination workspace, so "here" is
        // where the thread belongs. movetoworkspace (not …silent) would focus
        // it too, but be explicit so a same-workspace summon still raises.
        if !self.window_exists(id) {
            return Err(format!("window {id} is gone"));
        }
        let workspace = self
            .active_workspace_id()
            .ok_or_else(|| "hyprctl -j activeworkspace reported no workspace".to_string())?;
        let address = Self::address_arg(id);
        self.dispatch(&["movetoworkspace", &format!("{workspace},{address}")])?;
        self.dispatch(&["focuswindow", &address])
    }

    fn park_window_in_place(&self, window: &CompWindow) -> Result<(), String> {
        // Addressed by handle, so unlike nirius' title matcher this never
        // misses and the caller's focus-dance fallback stays unused.
        let address = Self::address_arg(&window.id);
        self.dispatch(&[
            "movetoworkspacesilent",
            &format!("{HYPRLAND_PARK_WORKSPACE},{address}"),
        ])
    }

    fn park_focused_window(&self) -> Result<(), String> {
        self.dispatch(&["movetoworkspacesilent", HYPRLAND_PARK_WORKSPACE])
    }

    fn project_dir_for_focused(&self) -> Option<String> {
        // `project_dir()` is a Lua global the user's Hyprland config defines;
        // it prints nothing (or a nil-ish word) when the focused workspace has
        // no registered project, so accept only an absolute path.
        let stdout = cmd_stdout("hyprctl", &["repl", "return project_dir()"])?;
        let dir = String::from_utf8_lossy(&stdout).trim().to_string();
        dir.starts_with('/').then_some(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_address_accepts_both_event_and_json_forms() {
        assert_eq!(normalize_address("0x55d2abc"), "0x55d2abc");
        assert_eq!(normalize_address("55d2abc"), "0x55d2abc");
        assert_eq!(normalize_address("  0x55d2abc\n"), "0x55d2abc");
    }

    #[test]
    fn hyprland_client_maps_to_comp_window() {
        let client = serde_json::json!({
            "address": "0x55d2abc",
            "title": "nvim",
            "pid": 4242,
            "workspace": { "id": 3, "name": "code" },
        });
        let window = HyprlandCompositor::client_window(&client, Some("0x55d2abc"))
            .expect("client with an address should map");
        assert_eq!(window.id, "0x55d2abc");
        assert_eq!(window.title.as_deref(), Some("nvim"));
        assert_eq!(window.workspace_id.as_deref(), Some("3"));
        assert_eq!(window.pid, Some(4242));
        assert!(window.focused);
    }

    #[test]
    fn hyprland_park_membership_reads_the_special_workspace_name() {
        let parked = serde_json::json!({
            "address": "0x1",
            "workspace": { "id": -99, "name": "special:agentpark" },
        });
        let tiled = serde_json::json!({
            "address": "0x2",
            "workspace": { "id": 3, "name": "code" },
        });
        assert!(HyprlandCompositor::client_is_parked(&parked));
        assert!(!HyprlandCompositor::client_is_parked(&tiled));
    }

    #[test]
    fn regex_escape_makes_a_title_an_exact_pattern() {
        assert_eq!(regex_escape("agent-switch (2)"), "agent-switch \\(2\\)");
    }
}
