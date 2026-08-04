# Surface content: sidebar, waybar, jump bind

Type: grilling
Status: resolved

Reframed 2026-08-03 (surface pivot, see map): the picker is now a GTK layer-shell **sidebar** in agent-switch — Mod+S summons it, exclusive zone on the left, per-area content. Ticket 03 resolved the vocabulary (lifecycle, attention tiers, verbs); this ticket designs what the sidebar shows and how it behaves.

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

**Toggled-only.** Mod+S summons with exclusive keyboard grab + exclusive zone (left, waybar-style); Esc or Mod+S dismisses and reclaims the space. Always-visible dashboard rejected (permanent space cost; layer-surface refocus-by-bind is the weak spot; the glanceable role belongs to waybar). Because the sidebar is summoned, it *could* rank — but v2's ordering rule was adopted instead (below), which supersedes the earlier "attention tiers as sort key" sketch in this ticket's Question.

### List structure (v2 copy)

Flat per-area inbox, no grouping. Sections in order:

1. **Active cards** — every live thread is a full 3-line card (~78px). **Static creation order, newest first; activity NEVER reorders the list — the screen only moves at lifecycle transitions.** (This revised an initial tier-sort decision during grilling; the tier order survives only in the jump bind and waybar aggregation.)
2. **`Settled (n)` shelf** — expanded by default, slim dim one-line rows (36px), sorted by settle-recency, paged: 10 initially + `Show 25 more`. Collapsed state hides all settled rows except the currently open thread (never hide the open thread — carried constraint).
3. **`Archived (n)` shelf** — collapsed by default, ghost rows with an unarchive action. Deviation from v2 (which hides archived entirely): our archive is a sidebar verb, so its undo stays discoverable in the same surface. This is the tombstones view.

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

`j/k` or arrows navigate; `1–9` jump to Nth visible row (v2's ⌘1–9, modifier-free); `Enter` = summon/go-to selected and dismiss; `s` = settle/un-settle; `p` = park; `a` = archive (confirm — the one one-way door); `r` = inline rename; `n` = new thread (creation flow itself is a separate unspecified ticket); `Tab` = cycle shelf expand/collapse; `Esc` dismisses. Mouse hover actions + right-click context menu copied from v2. No type-to-filter in v1 (per-area lists are short; a palette can land later). **Dropped from v2**: multi-select/bulk verbs (GTK cost, short lists), delete verb (archive/tombstone is our destruction path).

### Per-workspace illusion

Content follows the focused workspace via the niri event stream (layer surfaces are per-output; per-area sidebar is rendered content, not a compositor feature). On a workspace that isn't a named area, Mod+S shows the **global all-areas view** — all threads, cards gaining an area label (v2's "All projects" analog). `g` toggles area↔global from anywhere. Mod+S always answers "what's happening".

### Waybar

**Aggregate counts only** — `2 waiting · 3 running` (FleetView-style, global across areas): orange segment when anything is attention-worthy, dim when only working, module hidden when nothing live. Click toggles the sidebar. Satisfies "the glanceable surface never reorders" trivially. Per-row detail lives only in the sidebar.

### Jump-to-next-needing-me

**Mod+Shift+S.** (Mod+A was the user's first pick but their xremap macOS layer maps Super-a→Ctrl-a outside Ghostty; Super-Shift-S is unmapped so the jump works from any app. A Ghostty-only caveat was suspected for Mod+S via Super-s→Ctrl-s, but empirically — 08 prototype bind, 2026-08-04 — Mod+S reaches niri from any app.) Target selection: highest attention tier (Approval > Input > Failed > Unread), longest-waiting first within a tier (FIFO — nothing starves). Go-to if visible, summon if parked (marks read + un-settles per 03). When nothing needs you: **opens the sidebar** — the bind never feels dead.

### Area rename

Deferred. Sidebar verbs stay thread-scoped in v1; the registry stores the workspace name directly and renaming an area is a rare manual edit. The name-vs-stable-id question is flagged to [04](04-registry-design.md).

### Flows into 04 (registry)

`snoozedUntil` seam (deferred snooze); area referenced by workspace name (rename deferred — name-vs-stable-id flagged); per-thread repo/branch/host for card lines; PR association stays in schema as the badge seam's future feed.
