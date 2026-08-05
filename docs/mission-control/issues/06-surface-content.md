# Surface content: sidebar, waybar, jump bind

Type: grilling
Status: resolved

Reframed 2026-08-03 (surface pivot, see map): the picker is now a GTK layer-shell **sidebar** in agent-switch. The live prototype now defaults to an always-visible left dock with global-default content; Mod+S toggles keyboard command mode. The Answer below records the evolving feel-test calls. Ticket 03 resolved the vocabulary (lifecycle, attention tiers, verbs); this ticket designs what the sidebar shows and how it behaves.

## Question

- **Sidebar interaction model**: toggled-only (Mod+S summon with exclusive keyboard, Esc dismiss, space reclaimed) vs always-visible dashboard option (keyboard on-demand; refocus-by-bind is the weak spot since niri focus actions target windows, not layer surfaces)
- **Sidebar rows**: what a row carries; ordering — attention tiers as sort key (Needs input > Unread > Working > Idle from 03), stable within tiers; settled threads behind a reveal (successor to fzf `ctrl-s`); tombstones view for unarchive
- **Per-workspace illusion**: content follows focused workspace via niri event stream; behavior on non-area workspaces (hide vs global view)
- **Waybar**: what remains — aggregate counts (`2 waiting · 3 running`, FleetView-style), per-state glyphs, or nothing (sidebar subsumes)
- **Jump-to-next-needing-me**: which Mod-bind, and what it does when nothing waits (research: cheapest high-value unclaimed primitive)

Constraints from research: the primary/glanceable surface never reorders on activity — ranking lives only in the summoned sidebar; dim the running, brighten the finished-unread; no numeric badges per row; attention transitions may jump the queue, attention states never; the currently-open thread is never hidden by a filter. Colorblind constraint (user): blue/orange + structure, never red/green pairs.

Open from the pivot discussion: is "rename" also an area verb (workspace naming)? Thread-rename landed in 03.

## Answer (grilled 2026-08-04, confirmed)

The user's call mid-grilling: **copy t3code's sidebar v2** (`~/code/t3code`, `apps/web/src/components/SidebarV2.tsx` + `Sidebar.logic.ts` — a full design extraction was done during the session). v2's philosophy adopted wholesale: inbox-zero for agent threads; position is stable so muscle memory works; color and brightness are budgeted for "needs a human"; parking work — not the sidebar guessing — is what compacts the list.

### Interaction model

**Always-visible dock, revised by feel test 2026-08-05.** The live sidebar reserves 480 px on the left. It is normally passive; Mod+S toggles an exclusive keyboard command mode, and Esc/q releases that grab without hiding the dock. The prior toggled overlay remains available through `--popup` or `just demo-sidebar-live popup` for immediate A/B testing. Removing a left exclusive zone can leave a stale horizontal niri viewport offset, but the user accepted that trial trade-off because scrolling right clears it. The stable v2 ordering rule remains unchanged.

### List structure (v2 copy)

Flat per-area inbox, no grouping. Sections in order:

1. **Active cards** — every live thread is a full 3-line card (~78px). **Static creation order, newest first; activity NEVER reorders the list — the screen only moves at lifecycle transitions.** (This revised an initial tier-sort decision during grilling; the tier order survives only in the jump bind and waybar aggregation.)
2. **`Settled (n)` shelf** — expanded by default, slim dim one-line rows (36px), sorted by settle-recency, paged: 10 initially + `Show 25 more`. Collapsed state hides all settled rows except the currently open thread (never hide the open thread — carried constraint).
3. **`Archived (n)` shelf** — collapsed by default, ghost rows with unarchive and permanent sidebar-delete actions. Deviation from v2 (which hides archived entirely): our archive is a sidebar verb, so its undo stays discoverable in the same surface. `d` twice hides one tombstone permanently while retaining an internal suppression record; `D` twice deletes all archived rows across all areas.

**Snooze: deferred to v2 of the glue.** v2's snooze/wake/woke (hide until time T, hand-raise wakes early, `Woke` pill since static order won't surface it) is a good concept but adds a stored field, a daemon wake timer, and a fourth verb. Reserve a `snoozedUntil` schema seam (→ ticket 04) and the shelf slot in the layout; no verb in v1.

### Card anatomy (v2 mapped to our domain)

- **Line 1**: repo/project name (the area is already the scope; within a multi-repo area this is the repo) + trailing slot: **status label or compact relative time** (`now`/`5m`/`3h`/`2d`); on hover the label yields to action buttons (v2 pattern).
- **Line 2**: thread title; inline rename (double-click or `r`).
- **Line 3**: branch name + host icon when the thread lives on a non-local host (registry `host`, v2's server icon) + harness icon (pi/claude/codex, v2's provider icon).
- **PR badge `#123` and diff stats: empty layout seams in v1** — both need PR/diff tracking that belongs with the future merge-automation design.
- Rich hover tooltip (title, area/repo, host, branch, harness) copied from v2.

### Status rendering (v2 copy + 03 refinement)

Attention axis extended (CONTEXT.md updated): **Approval > Input > Working > Failed > Unread > Idle** — 03's Needs-input split into Approval (permission decision; cleanly detectable via `permission_prompt`) and Input (agent asked a question; best-effort), Failed added (best-effort). "Attention-worthy" = anything except Working and Idle.

Trailing labels: `Approval` / `Input` / `Working` + live ticking duration (`12s`/`7m`/`2h 5m`) / `Failed` / `Done` (✓, the unread marker) / no label = relative time. **In-flight rows fade as a whole; prominence is reserved for rows that need a human** (done-unread, failed). Row background encodes interaction state only (hover/selected/open), never status.

**Colorblind-safe remap** (user is red-green colorblind; v2's emerald/red dropped): orange = Approval (the one "act now" hue); indigo = Input; dim sky = Working; magenta + alert icon = Failed; **brightness + ✓ = Done** (no green); future PR states blue/violet/gray; future diff stats blue/orange. Structure (labels + icons + brightness) always carries the meaning; hue is redundant.

Unread semantics from v2 confirmed: never-visited counts as read; visiting/summoning marks read; **mark-unread kept** (rewinds `last_read_at`).

### Keyboard scheme (exclusive grab makes the sidebar keyboard-first)

`j/k` or arrows navigate; `1–9` jump to Nth visible row (v2's ⌘1–9, modifier-free); `Enter` = summon/go-to selected and release command mode; `s` = settle/un-settle; `p` = park; `a` = archive/unarchive (archive confirms); `d` twice = permanently hide an archived tombstone; `D` twice = hide all archived tombstones globally; `r` = inline rename; `n` = new thread (creation flow itself is a separate unspecified ticket); `Tab` = cycle shelf expand/collapse; `Esc` dismisses. Mouse hover actions + right-click context menu copied from v2. No type-to-filter in v1 (per-area lists are short; a palette can land later). **Dropped from v2**: multi-select/bulk verbs (GTK cost, short lists). Delete was later restored for archived rows only; it hides the row and its resurrection affordance but does not delete harness files.

### Per-workspace illusion

The latest feel-test call is **global all-areas view by default** — all threads, with cards gaining an area label when it differs from the repo. `g` narrows to the currently focused named area; an unnamed workspace leaves the scope global. This replaced the earlier area-first default and makes Mod+S consistently answer "what's happening everywhere?"

### Waybar

**Highest nonzero count only**, global across areas: literal unread Done (`✓ n`) first, then Working (`⚙ n`), then open Idle (`○ n`). Settled and archived threads are excluded. Approval/Input remain visible in the tooltip but do not get folded into the literal Done count. The persistent sidebar writes an atomic projection every two seconds even while hidden; the Waybar module reads that projection rather than recomputing from legacy `agent-switch list`. Click toggles the dock's keyboard command mode (or popup visibility in popup mode). Per-row detail remains in the sidebar/tooltip.

### Jump-to-next-needing-me

**Mod+Shift+S.** (Mod+A was the user's first pick but their xremap macOS layer maps Super-a→Ctrl-a outside Ghostty; Super-Shift-S is unmapped so the jump works from any app. A Ghostty-only caveat was suspected for Mod+S via Super-s→Ctrl-s, but empirically — 08 prototype bind, 2026-08-04 — Mod+S reaches niri from any app.) Target selection: highest attention tier (Approval > Input > Failed > Unread), longest-waiting first within a tier (FIFO — nothing starves). Go-to if visible, summon if parked (marks read + un-settles per 03). When nothing needs you: **opens the sidebar** — the bind never feels dead.

### Area rename

Deferred. Sidebar verbs stay thread-scoped in v1; the registry stores the workspace name directly and renaming an area is a rare manual edit. The name-vs-stable-id question is flagged to [04](04-registry-design.md).

### Flows into 04 (registry)

`snoozedUntil` seam (deferred snooze); area referenced by workspace name (rename deferred — name-vs-stable-id flagged); per-thread repo/branch/host for card lines; PR association stays in schema as the badge seam's future feed.

## Implementation checkpoint (2026-08-05)

The live GTK prototype implements the persistent dock plus optional popup mode, static rows, active/settled/archived sections, attention rendering, Monokai Spectrum palette, global/area scope toggle, keyboard verbs, inline rename, and Waybar projection. Shelf pagination, hover action buttons, right-click menus, PR/diff seams, host support, and Mod+Shift+S jump-to-next are not implemented. Diagnostic row content and the 480px width are temporary feel-test instrumentation.
