# Mission control for agent threads — map

Label: wayfinder:map

## Destination

Reach a build-ready `docs/mission-control/blueprint.md` for glue v1: registry schema, status producers, runtime/attach backend, surfaces (sidebar / Waybar / jump bind), and lifecycle verbs. The blueprint is not written yet; tickets 08 and 09 plus the implementation checkpoint still need folding into it.

## Notes

- Charted premises (decided during charting, 2026-08-03):
  - **Threads are not spatial.** A thread's identity is its registry entry, independent of runtime substrate; niri stays stock and displays attachments. Workspace-per-thread rejected (a 20-minute maintenance thread must not cost a workspace). Workspaces remain meaningful as *areas of work* that threads visit while engaged; a parked thread has no presented window in its work area, though a backend may preserve a hidden warm window.
  - **Attach model:** later resolved to park + summon/go-to behind a backend interface. The charted "pin to workspace" idea did not survive feel-testing; threads keep an area and are visited there rather than moved.
  - **Build the glue.** Custom Wayland compositor is the fallback, not the plan. t3code rejected as platform (no pi support; its chat UI replaces the terminal workflow) — may be played with for ideas.
- Read first: memory note `project_thread_environment_design.md` (design pillars + full sidebar/landscape research: stable-list/ranked-palette, read-unread primitive, derive-don't-reconcile, archive-on-merge rules).
- Constraint for the blueprint (user, 2026-08-03): the `bin/thread-*` prototypes hardcode niri/nirius/fzf — fine while prototyping, but the real verbs (park/summon/go-to/settle) must land behind an interface where the compositor glue is one backend, not baked into the verb logic.
- Surface pivot (user, 2026-08-03): the picker becomes a **GTK layer-shell sidebar** living in agent-switch (`~/code/agent-switch`, Rust, gtk4 + layer-shell + niri IPC + daemon). Mod+S toggles it. The live prototype overlays on the left with exclusive keyboard grab and exclusive zone `0` after a niri viewport-offset issue invalidated dynamic space reservation. **Workspace = named area**; thread→area lives in durable state because parked/cold threads cannot derive it. The sidebar carries create / summon / park / settle / archive / rename.
- Ubiquitous language for the whole effort lives in `CONTEXT.md` at the repo root (started during ticket 03).
- Repo split (moved here 2026-08-03): these artifacts live in agent-switch — the build home for sidebar, daemon, registry, and verb interface. The ticket-02 throwaway prototypes (`bin/thread-*` + niri binds in dotfiles) were deleted the same day after the sidebar pivot (dotfiles commit `ed387c4`); their mechanism findings are preserved in ticket 02's Answer.
- Skills: `/grilling` + `/domain-modeling` for grilling tickets, `/prototype` for 02 and 06, `/research` for research tickets, `/blueprint` for the final artifact.
- Preferences: subagents default to Opus 5, never Fable. pi is the primary harness; terminal-focused workflow (ghostty, nvim/hunk, zmx). Employer work code stays inside employer infra.

## Decisions so far

<!-- one line per closed ticket: gist + link -->

- [v1 scope cut](issues/01-v1-scope-cut.md) — local implementation with remote-shaped schema (host field day one, sync deferred to v2); harnesses pi + claude + codex (codex conditional met by research).
- [Codex status surface](issues/05-codex-status-surface.md) — build the codex producer on its hooks engine (11 events, headless-capable, 3 already wired in this repo); no status file exists; liveness via `$PPID` from SessionStart + `kill -0`; watch for content-hashed hook-trust invalidation.
- [Attach mechanism](issues/02-attach-mechanism.md) — verbs are park + summon/go-to on nirius scratchpad + niri IPC, window-id keyed via agent-switch; threads never move: visible threads are focused, parked threads are shown/tiled in their recorded area, cold threads resurrect there; the fzf prototype was superseded by the live GTK sidebar; missing "settle" state fed into 03.
- [Registry design](issues/04-registry-design.md) — registry-minted ULID identity (harness session ids/windows/zmx names are mutable facts under it); schema = identity + resume manifest (runtime slot pending 08) + lifecycle timestamps + last_read_at + snooze/PR seams; branch/repo derived while live, branch_ref written only at archive (tombstone = same row, archived_at set); storage dir-of-files `registry/<host>/<ulid>.json` with structural per-host ownership; single-writer daemon with hook-event flock fallback; registry (durable) and sessions.json (producer cache) stay two stores joined on harness_session_id; cross-host = verbs travel, mirrors read-only, migration stays fog.
- [Surface content](issues/06-surface-content.md) — copy t3code sidebar v2: toggled-only Mod+S overlay, static creation order (activity never reorders; in-flight fades, brightness = needs-a-human), 3-line cards + slim Settled / Archived shelves; snooze deferred; attention refined to Approval > Input > Working > Failed > Unread > Idle with a colorblind-safe Monokai treatment; keyboard-first; global view by default with `g` narrowing to the focused area; Waybar shows highest-only literal Done > Working > open Idle; jump-to-next remains designed for Mod+Shift+S; area rename deferred.
- [Lifecycle verbs](issues/03-lifecycle-verbs.md) — three axes: visibility derived (summoned/parked — park is spatial, not lifecycle), lifecycle stored (live→settled→archived), attention derived+fused with one stored `last_read_at`; settle promises recoverability not warmth; archive = reclaim verb; resurrection is lazy and go-to rather than relocation; auto-settle is designed but unbuilt; background-work hand-raise semantics remain a known gap; runtime substrate stays in [08](issues/08-thread-runtime-substrate.md).
- [Fate of existing pieces](issues/07-existing-pieces.md) — evolve agent-switch in place: keep its producers and hot session cache during the transition, build registry/sidebar/verbs in the same daemon, leave nirius and project tooling in their spatial roles, and evolve the Waybar module to consume the sidebar projection.

## Current implementation checkpoint

See [current-state.md](current-state.md) for what is actually running, where the live prototype differs from the target model, and which pieces remain scaffolding.

## Not yet specified

- [Runtime substrate](issues/08-thread-runtime-substrate.md) — the local window-hosted prototype works, but the value and scope of zmx-backed runtimes remain unresolved.
- [Harness-derived thread content](issues/09-harness-derived-content.md) — session names and manual-rename precedence are implemented; recap/latest-ask/live-activity placement is still open.
- Thread creation flow (picker → project / harness / host / worktree-vs-stable-checkout; tenancy via path convention) — sharpens after scope cut + registry design.
- Worktree GC / archive automation details — after lifecycle verbs.
- Remote hand-raise delivery + ntfy phone approval — remote-phase. Local background-work/hand-raise semantics are a nearer status-model gap.
- Transcript sync / cross-machine resume — after scope cut + registry design.
- Custom-compositor fallback criteria — what failure of the glue would trigger reconsidering it.
- t3code playtime notes — optional, off-route; anything stolen lands in Decisions via the ticket that uses it.

## Out of scope

- Adopting t3code as the platform — ruled out during charting (pi unsupported; chat UI replaces terminal workflow).
- Port-collision handling for concurrent dev stacks — deferred in the original design discussion, stays deferred.
- Building any web/desktop app surface — v1 surfaces are waybar, picker, and terminals only.
