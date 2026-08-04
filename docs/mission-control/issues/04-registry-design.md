# Registry design

Type: grilling
Status: open
Blocked by: 01, 03

## Question

The registry is the core build — durable truth for threads. Decide:

- **Schema**: thread id, host, dir, branch, harness, zmx session name(s), lifecycle state, read-bit (mark-read-on-open, server-side style), PR association
- **Storage**: sqlite vs jsonl vs dir-of-files
- **Write model**: single-writer daemon vs library + lock
- **Cross-host** (if [01](01-v1-scope-cut.md) puts remote in scope): how EC2 thread state reaches the laptop — push-on-event over ssh, periodic pull, syncthing, or a central store

Constraint from research: **derive, don't reconcile** — one authoritative producer per fact, no second projection to sync. Remote status over SSH is the gap no product ships; this is the part worth getting right.

Flags from [06](06-surface-content.md) (2026-08-04): reserve a `snoozedUntil` seam (snooze deferred to v2); decide area reference name-vs-stable-id (area rename deferred, registry stores workspace name for now); schema must feed the card lines (repo, branch, host, harness) and keep PR association as the future badge/merge-automation feed.
