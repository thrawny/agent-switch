# Mission control — current implementation state

Updated 2026-08-05. This is the implementation checkpoint; the issue documents record the intended domain and resolved design decisions. Where the prototype intentionally differs, this file describes what is actually running.

## What is live

A persistent GTK4 layer-shell sidebar runs from `target/debug/agent-switch demo-sidebar --live` under the `just watch` loop. Mod+S toggles it. It joins the existing hook-fed `sessions.json` cache with niri windows/workspaces, nirius scratchpad membership, transcript-derived state, and a prototype registry sidecar.

Implemented feel-test behavior:

- Stable thread rows with prototype `seq` identity and a separate user-owned `order` sort key.
- Global all-areas view by default; `g` narrows to the focused named area.
- Static active-row ordering, Settled and Archived shelves, keyboard navigation and verbs.
- Attention labels: Approval, Input, Working, Done/Unread, and Idle. Failed remains a display seam; no producer currently emits it reliably.
- Park, summon/go-to, settle, archive, mark-read, rename, reorder, and minimal new-Pi-thread creation.
- Thread succession: a new conversation taking over the same live window adopts the existing row instead of archive-plus-mint.
- Cold resurrection through harness resume (`pi --session`, `claude --resume`, `codex resume`).
- Manual titles persist and propagate to Pi (`setSessionName` through a drop-file watcher) and Claude (`custom-title` transcript append), including re-assertion after session succession.
- A Waybar snapshot refreshed while the sidebar is visible or hidden. Its headline shows the highest nonzero count only: literal unread Done, then Working, then open Idle. Settled and archived threads are excluded.

## Prototype storage and identity

The durable registry from ticket 04 is **not implemented**. The prototype uses:

- `~/.local/state/agent-switch/sessions.json` — existing hot producer cache.
- `~/.local/state/agent-switch/sidebar-proto-registry.json` — single-file sidecar preserving `seq`, order, title, area, lifecycle timestamps, and read markers.
- `~/.local/state/agent-switch/sidebar-proto-waybar.json` — atomically replaced Waybar projection.

This is scaffolding, not the planned `registry/<host>/<ulid>.json` store. Prototype `seq` is identity; production ULID identity, per-host ownership, host-aware manifests, worktree metadata, and remote mirrors remain unbuilt.

## Runtime and spatial behavior

Local threads are window-hosted: the harness process lives in Ghostty. Zmx currently supervises the development loop, not thread runtimes.

The latest spatial rule is **threads never move**:

- Visible thread → focus its existing window.
- Parked thread → visit its recorded area, show it from nirius scratchpad, and tile it there.
- Cold thread → visit its area and spawn a harness resume there.
- A named area with no workspace does not silently move the thread elsewhere.

Parking hides a still-live window; it does not achieve the aspirational zero-window-at-rest model. The sidebar itself currently overlays with exclusive zone `0`, rather than changing niri's working area, because removing a left exclusive zone left stale horizontal viewport offsets during testing.

Archive currently closes the runtime window and leaves a prototype tombstone. It does **not** reclaim a worktree. Settle parks the window and shelves the row. The designed 36-hour auto-settle and settled-runtime reaper are not implemented. Closing a thread outside the sidebar is treated as an explicit abandonment signal and auto-tombstones it once both window and producer session are gone.

## Producers today

- **Claude:** lifecycle/status still comes from SessionStart, UserPromptSubmit, Stop, permission Notification, and SessionEnd hooks. Claude's `~/.claude/sessions/<pid>.json` files are researched but are not consumed by agent-switch.
- **Pi:** the `agent-switch.ts` extension maps Pi lifecycle events into `agent-switch track` and carries transcript path/session name.
- **Codex:** SessionStart, UserPromptSubmit, and Stop hooks are wired. The ticket-05 recommendation to add PermissionRequest and SessionEnd is still outstanding.

The daemon also inspects transcript progress to clear answered permission/question states. Producer state remains a cache, not durable thread identity.

## Open gaps

- Decide the runtime substrate (ticket 08): keep local window-hosted runtimes, adopt zmx, or use a per-host mix.
- Build the ticket-04 durable registry and migrate prototype sidecar behavior into it.
- Define and build the real creation flow (project, harness, host, checkout/worktree).
- Implement honest worktree reclaim/unarchive and future PR-assisted confirmation removal.
- Finish harness-derived content policy (ticket 09): recap, latest ask, and live activity beyond session names.
- Implement the Mod+Shift+S jump-to-next action.
- Resolve background-work/agent-hand-raise semantics so waiting on managed tasks is not mislabeled Idle.
- Make cold resurrection honor the sandbox wrapper and direnv environment.
- Remote registry mirroring, transcript sync, phone notification/approval, snooze, and migration remain later work.

## Document status

Resolved design tickets: 01–06. Ticket 07 is resolved by the evolve-in-place decision recorded there. Tickets 08 and 09 remain open. The final `docs/mission-control/blueprint.md` has not been written; it should follow runtime resolution and convert the validated prototype behavior plus registry design into build slices.
