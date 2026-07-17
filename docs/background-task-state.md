# Deferred design note: background-task session state

- **Status:** deferred pending an empirical PID-tree spike
- **Date:** 2026-07-17

## Problem

`agent-switch` currently presents these session states:

- `responding`: the foreground agent is running
- `waiting`: the user needs to answer a question or permission request
- `idle`: the foreground agent has settled

`idle` is ambiguous when an agent has deliberately settled while work continues in a
background process. A common example is Claude Code launching an `acpx` command with
`Bash` and `run_in_background: true`, reporting that it is waiting for Codex, and then
resuming automatically when the command completes.

That interval is useful to distinguish from true inactivity, but it must not be represented
as `waiting`: in the existing model, `waiting` means that human attention is required.

## Why transcript parsing is not the plan

An earlier version of the project derived more state from agent transcripts. We do not want
to restore that approach for background jobs:

- transcript formats are implementation details and can change between agent versions
- large JSONL files are expensive to inspect repeatedly
- incremental cursors, truncation, rotation, and malformed records complicate correctness
- each agent needs a different parser
- completion matching becomes a persistent bookkeeping problem

Claude hooks expose enough information to observe a background Bash launch, but there is
no documented hook for the later completion of an arbitrary background Bash command.
Claude's `agent_completed` notification concerns native background agents/sessions, not a
Bash process such as `acpx`. Codex has a similar gap. A start-only hook would leave stale
state, so hooks alone do not justify this feature.

## Candidate: derive the state from the process tree

The interactive agent process is the natural lifetime root for commands launched through
its shell tool. Hook commands already know their parent PID; today `track` appends that PID
to the provider session ID to distinguish a parent Claude session from forked agents.
A future implementation should store it explicitly as `agent_pid` rather than parse the
session-ID suffix.

While investigating a live Claude/acpx workflow, the relevant processes looked like:

```text
claude 2248182
└─ zsh 3114582
   └─ acpx 3114585
```

The long-lived acpx queue owner had detached to the user systemd process, but the acpx
client waiting for that queued prompt remained below Claude. When the prompt completes,
the waiting client and shell branch exit. This is exactly the lifetime the proposed state
needs to represent; the detached queue owner's TTL should not keep the Claude session
marked as busy.

This suggests a cheap derived rule after the top-level agent emits `Stop`:

```text
settled + relevant live descendants = background
settled + no relevant descendants   = idle
```

Only settled/background sessions need inspection. On Linux the daemon can walk
`/proc/<agent-pid>/task/*/children` recursively, avoiding transcript I/O and avoiding a
full process-table scan. The niri daemon already refreshes dynamic state periodically, so
background completion can be noticed without another agent hook.

## Proposed semantics

If the spike validates the process relationship, add an effective `background` state with
this meaning:

> The top-level agent has settled, no human response is required, and at least one relevant
> descendant process is still running.

Suggested attention precedence:

1. `waiting` — human input is required
2. `responding` — foreground agent activity
3. `background` — delegated/background work continues without human input
4. `idle` — no foreground or observed background work

`background` should initially be a runtime-derived state, not persisted background-task
IDs. It should naturally recover after a daemon restart by inspecting the live process
tree, and it should disappear when the processes disappear.

The transition immediately after a background process exits needs validation. Claude
normally injects a completion notification and starts another model turn, but that
synthetic wake-up does not fire `UserPromptSubmit`. Options include:

- move directly to `idle` until another existing hook reports activity; or
- use a short-lived `responding` completion grace, cleared by the next `Stop` and bounded
  by a timeout if no turn follows.

The spike should determine which behavior is least misleading rather than guessing now.

## Required safeguards

A process-tree implementation must account for:

- **The hook itself.** During `Stop`, the `agent-switch track stop` process is temporarily
  a descendant. Delay inspection briefly or explicitly exclude the hook branch.
- **PID reuse.** Record the process start time or another identity value and verify it with
  the PID before trusting a stored session.
- **Agent death.** If the root PID disappears, do not infer background work; normal stale
  session cleanup applies.
- **Persistent helpers.** Verify that MCP servers, shell helpers, or other agent-owned
  processes do not remain as ordinary descendants and create permanent false positives.
- **Reparenting.** A double-forked, disowned, containerized, or systemd-launched job may
  leave the tree and therefore be missed. The feature should be conservative rather than
  attempt command-line or transcript heuristics to recover these cases.
- **Multiple jobs.** The state is true while any relevant descendant branch remains; no
  task-ID bookkeeping should be necessary.
- **Platform scope.** `/proc` makes this Linux-specific. Other platforms should retain the
  existing hook-derived states unless they gain a reliable equivalent.
- **Sandbox visibility.** Confirm that the daemon and agent hooks observe the same PID
  namespace. The current bwrap setup does, but that cannot be assumed for every launcher.

## Spike checklist

Before implementing the state model, test the following without reading transcripts:

1. Claude launches a plain background `sleep`; the descendant branch appears after
   `Stop` and disappears on completion.
2. Claude launches an acpx persistent-session prompt; the waiting client remains beneath
   Claude while the detached queue owner does not affect completion.
3. Two concurrent background commands keep the state active until both finish.
4. A failed or cancelled background command clears the state.
5. Normal Claude turns leave no persistent descendants after `Stop`.
6. Codex shell execution preserves an equivalent descendant relationship for background
   work, including its newer unified execution path.
7. Stop hooks do not cause a visible false `background` flash.
8. Agent termination, PID reuse, and daemon restart behave conservatively.
9. Polling several settled sessions has negligible CPU and I/O cost.

If Claude and Codex do not provide stable process ancestry, or normal sessions produce
persistent false-positive descendants, leave the feature deferred. Do not fall back to
transcript parsing.

## Future explicit integrations

Pi can report background lifecycle directly from its extension. Native Claude and Codex
subagents also have start/stop hooks. Those explicit signals may eventually complement the
PID-derived state, but they should use the same semantics: `background` is non-attention
work, while `waiting` remains reserved for human action.

An explicit acpx lifecycle callback could be another reliable source in the future, but it
would need a robust way to identify the parent interactive session. Parsing arbitrary acpx
shell commands or polling detached queue owners is intentionally out of scope.
