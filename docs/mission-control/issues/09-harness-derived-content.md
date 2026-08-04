# Harness-derived thread content (recap, titles)

Type: design
Status: open

## Question

The sidebar's rows currently carry only structural facts (repo, branch, state). Harnesses write much richer per-thread content that is readable from the outside — decide what the card surfaces and from where.

Discovered 2026-08-04 (ticket 08 prototype session), all in the Claude Code transcript JSONL (`transcript_path`, already tracked):

- **Recap** — `{"type":"system","subtype":"away_summary","content":"..."}`: rolling "what happened + what's next" summary, regenerated as the session progresses. The best answer to what a `Done`-brightened row should tell you: *what am I coming back to?*
- **Session title** — `{"type":"ai-title","aiTitle":"..."}`: AI-generated title, mostly reflects how the session started (take the last occurrence).
- **Latest ask** — `{"type":"last-prompt","lastPrompt":"..."}`: the user's most recent prompt.
- **Live activity** — the terminal window title, continuously rewritten by Claude Code (`✳ <current activity>`), already visible via `niri msg windows`. Display-only (02's rule: identity never keys on titles).

## To decide

- Card mapping: recap as line-2 title vs stable title + recap in the hover tooltip (06 reserved a rich tooltip); where `ai-title` and `last-prompt` land, if anywhere.
- Freshness: transcript is re-read anyway for activity watching — does the daemon parse these entries in the same pass, or lazily on sidebar open?
- **Per-harness enrichment, not manifest fields**: Claude-only today. Pi has a session display name (`--name`, session file header); codex unknown. The registry `title` (04, rename verb) stays the user-owned field — harness-derived content must not overwrite a manual rename. Decide the precedence: manual rename > recap/ai-title > window title > repo name?
- Whether the recap also feeds the jump-bind / waybar tooltip surfaces.

## Constraints

- 04: registry stores identity + manifest; derived content is never persisted (re-derivable from transcript at read time).
- 02: titles are display-only; identity stays window/session-keyed.
