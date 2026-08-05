# Mission control — ubiquitous language

Glossary for the thread/mission-control domain. Terms are added as they are resolved (wayfinder tickets, grilling sessions). Vocabulary only — no implementation.

## Terms

**Thread** — a unit of agent work: registry entry + runtime (+ optionally a worktree). Not spatial: its identity and lifecycle do not come from a window or workspace. The runtime may be window-hosted or zmx-backed; that substrate is not part of thread identity.

**Area** — a niri workspace understood as a domain of work (e.g. "work", "dotfiles"). Threads belong to an area; threads visit the area's workspace while engaged. (Name provisional — user: "project / area / domain, name tbd".)

**Visibility** *(derived axis — never stored)* — whether a thread is presented in its work area. Values: **summoned** (presented for engagement) / **parked** (hidden from the work area). Derived from runtime/compositor facts; a backend may preserve a hidden warm window. Park/summon are spatial verbs and never define lifecycle. (Decided in ticket 03, 2026-08-03: "park" as a lifecycle state was an artifact of the fzf prototype.)

**Lifecycle** *(stored axis — registry-owned)* — the thread's position in its life: **live** → **settled** → **archived**. Settle/archive/unsettle are registry verbs, orthogonal to visibility.

**park** — spatial verb: hide the thread's windows ("I don't want to see you now"). The thread stays live.

**summon** — engagement verb and go-to: materialize the thread in its own area without relocating it. Visible runtime → focus it; parked runtime → visit its area and show it; cold runtime → resurrect it in that area from the registry manifest + harness resume. Also clears settled and marks read.

**Resurrection** — recreating a thread's runtime from its registry manifest (spawn terminal, cd to manifest cwd, harness resume). Always lazy — happens through summon, never in bulk at boot. Resumed ≠ identical: conversation restored, scrollback and in-flight state gone.

**settle** — lifecycle verb: mark work as done-for-now; thread leaves the default list but remains fully recoverable. Settle promises **recoverability, not warmth**: worktree, registry row, and conversation are kept; the runtime may go cold. The destruction gradient: park destroys nothing → settle may let warmth lapse, keeps all state → archive reclaims state, leaves a tombstone.

**Reaper** — internal action (not a user verb) that may terminate the runtime of a long-settled thread, relying on resurrection for the way back. Invisible: summon behaves identically on warm and reaped threads.

**Auto-settle** — daemon-written transition into settled when all hold: runtime idle or dead, no unread activity, no needs-input, quiet for 36h. Attention always blocks auto-settle. Silent, reversible from the settled view.

**Un-settle** — leaving settled. Exactly two triggers: summon (engagement) and agent hand-raise (any attention-worthy event). Symmetry rule: attention-worthy events un-settle; attention-free time settles. Viewing the settled list is neither.

**Attention** *(derived axis — fused, Codex-style)* — one value per thread, priority-ordered: **Approval** (waiting on a permission decision) > **Input** (agent asked a question and waits on the answer; best-effort — harnesses that can't signal it never emit it) > **Working** (running; never unread by definition) > **Failed** (runtime reported an error; best-effort) > **Unread** (activity since `last_read_at`, rendered as "Done") > **Idle** (read and quiet, unlabeled). "Attention-worthy" means anything except Working and Idle — these block auto-settle and trigger un-settle. (Refined in ticket 06, 2026-08-04, from 03's Needs input > Unread > Working > Idle: Needs-input split into Approval/Input, Failed added, vocabulary copied from t3code sidebar v2.)

**Read marker** — the single stored `last_read_at` per thread. Advances on summon or opening the thread's detail; viewing a list never advances it. A working thread cannot be unread; only finished work demands reading.

**Sidebar** — the summoned surface: toggled into view on demand, never persistently visible. It shows all areas by default and can narrow to the focused area; it carries the thread verbs. Rows sit in static creation order (newest first); activity never reorders the list — attention is expressed by brightness and a trailing status label, and the screen only moves at lifecycle transitions. (Decided in ticket 06, 2026-08-04 and refined by live feel test.)

**Registry** — the durable record of every thread: identity, manifest, lifecycle, read marker. The single source of durable truth — every other thread fact is derived, never reconciled. Each thread's record is owned by exactly one host; other hosts only read it, and a verb aimed at a remote thread travels to the owning host. (Decided in ticket 04, 2026-08-04.)

**Manifest** — the stored part of a thread that resurrection needs: which harness and conversation to resume, where (directory, host), and under what name (title, area). If the manifest survives, the thread survives — everything else about a thread can be regrown.

**Runtime** — the living processes of a thread (agent + shell), wherever they are hosted. It may be window-hosted or zmx-backed without changing thread identity or verb contracts. Liveness is derived, never stored, and is **relative to the runtime's host**: a laptop reboot kills local runtimes only; a remote thread's ssh+zmx runtime may survive it.

**archive** — lifecycle verb, defined by contract independent of runtime substrate: terminate the runtime, reclaim the worktree (refcount + confirm), tombstone the registry row. Transcript pointer and branch ref survive.

**Tombstone** — the registry remnant of an archived thread: identity, transcript pointer, branch ref. Enough to unarchive.

**unarchive** — restore a tombstoned thread to live: registry entry back, worktree re-creatable from the branch; honest that the old processes don't return.
