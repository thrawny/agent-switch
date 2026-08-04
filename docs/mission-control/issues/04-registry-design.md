# Registry design

Type: grilling
Status: resolved
Blocked by: 01, 03

## Question

The registry is the core build — durable truth for threads. Decide:

- **Schema**: thread id, host, dir, branch, harness, zmx session name(s), lifecycle state, read-bit (mark-read-on-open, server-side style), PR association
- **Storage**: sqlite vs jsonl vs dir-of-files
- **Write model**: single-writer daemon vs library + lock
- **Cross-host** (if [01](01-v1-scope-cut.md) puts remote in scope): how EC2 thread state reaches the laptop — push-on-event over ssh, periodic pull, syncthing, or a central store

Constraint from research: **derive, don't reconcile** — one authoritative producer per fact, no second projection to sync. Remote status over SSH is the gap no product ships; this is the part worth getting right.

Flags from [06](06-surface-content.md) (2026-08-04): reserve a `snoozedUntil` seam (snooze deferred to v2); decide area reference name-vs-stable-id (area rename deferred, registry stores workspace name for now); schema must feed the card lines (repo, branch, host, harness) and keep PR association as the future badge/merge-automation feed.

## Answer (grilled 2026-08-04, confirmed)

### Identity

**Registry-minted ULID**, immutable for the thread's whole life including tombstone. ULIDs sort by creation time — the sidebar's static order (06) falls out of the key. Everything that dies earlier than the thread hangs off it as a mutable fact: harness session id (changes on resume), window ids (die with windows), zmx names (pending 08). Rejected: harness session id as key (breaks on resume, format varies per harness), human slug (rename breaks identity or slug diverges from title).

### Schema (one row per thread)

- **identity**: `id` (ULID) · `title` (mutable, rename verb) · `area` (workspace *name*, per 06's deferral — revisit only if area rename becomes a verb) · `host` (day one, per 01)
- **resume manifest** (what summon/resurrect needs, per 03): `harness` (open enum) · `harness_session_id` (mutable, updated on resume) · `transcript_path` (activity watching now, tombstone pointer later) · `cwd` · `worktree_path` (Option; None = stable checkout; feeds archive's refcount/GC) · `runtime` (reserved slot, shape pending [08](08-thread-runtime-substrate.md))
- **lifecycle**: `created_at` · `settled_at` (Option) · `archived_at` (Option) — stored as timestamps, the enum is derived (both None = live)
- **attention support**: `last_read_at` (the single stored read marker, per 03)
- **seams, unused in v1**: `snoozed_until` (06) · `pr` association (badge + merge automation)
- **tombstone extra**: `branch_ref`, written once at archive time

**Derive rules**: repo name and current branch are *derived* while live (from cwd/worktree at read time — a stored branch goes stale on rebase); branch is only persisted at archive, when the worktree is about to be reclaimed. Never stored: visibility, liveness, attention, repo, live branch.

**Tombstone** = the same row with `archived_at` set; manifest slims to identity + `transcript_path` + `branch_ref`. No separate tombstone store.

### Storage

**Dir-of-files**: `~/.local/state/agent-switch/registry/<host>/<ulid>.json`, atomic temp+rename per thread (the `state.rs` pattern). Corruption is confined to one thread; tombstones accumulate without bloating a hot file; the store is greppable. The per-host directory makes ownership *structural* — v2 sync is "mirror other hosts' directories, read-only", no merge logic, which is what derive-don't-reconcile demands. Rejected: sqlite (dependency, opaque, file-sync-hostile for no needed queries), single JSON (whole-store write and conflict unit).

### Write model

**Single-writer daemon with a hook-event fallback.** The daemon (which also hosts the sidebar and owns auto-settle per 03) is the sole writer on its host; verbs require it. `track` keeps its existing degraded direct-write-under-flock path *for hook events only*, so a `session-start` fired during a daemon outage still registers the manifest instead of vanishing.

### Registry vs session state — two stores

The registry is durable truth (identity, manifest, lifecycle, read marker). `sessions.json` stays what it already is: a rebuildable window-keyed producer cache (hook-fed attention inputs, `state_updated`, window mapping) with no durability promise. Joined on `harness_session_id`. Hook-frequency writes never touch registry files (except session-id/transcript updates on start/resume). Rejected: one unified store — hot ephemeral writes on cold durable rows.

### Cross-host rule (designed now, built in v2)

**Verbs travel, facts stay home.** A thread's row is written only by the daemon on its owning host; a verb aimed at a remote thread is forwarded to that host's daemon; mirrored directories are read-only. Thread migration between hosts stays fog — create-on-new-host + archive-on-old until a real need appears.

### Flows onward

Ticket 08 fills the `runtime` manifest slot; the blueprint gets this schema verbatim; `CONTEXT.md` gains **Registry** and **Manifest** terms.
