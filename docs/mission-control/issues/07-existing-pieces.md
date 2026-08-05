# Fate of existing pieces

Type: grilling
Status: resolved
Blocked by: 01

## Question

What happens to the machinery that existed before mission control:

- **agent-switch** (daemon + hook tracking + Waybar module) — retire in favor of registry + producers, hard-cut, or evolve in place?
- **Pi extension** `agent-switch.ts` — rewire or dual-emit?
- **niriusd / niri-project / niri-project-picker** — what changes under the non-spatial thread model?
- **Waybar agent-status module** — replace or evolve?

## Answer (resolved by implementation, 2026-08-05)

**Evolve agent-switch in place; no parallel replacement and no hard cut.** It is now the build home for the live GTK sidebar, thread/window join, verbs, future durable registry, and compositor backend. The existing daemon and `sessions.json` remain the hot producer cache during the transition; the ticket-04 registry will be added alongside that cache rather than replacing producer state with lifecycle state.

- **Producer integrations stay.** Claude hooks, the Pi extension, and Codex hooks continue to feed `agent-switch track`. They should evolve event coverage and eventually register/update durable manifests, but they do not dual-emit into a second service.
- **Pi extension stays the Pi adapter.** It now also applies sidebar rename hand-offs through `pi.setSessionName`; future registry integration belongs behind the same adapter.
- **nirius stays the spatial backend.** It owns scratchpad hide/show mechanics, not thread identity or lifecycle. niri-project and niri-project-picker keep their project/workspace roles; mission control does not absorb application/workspace switching.
- **Waybar evolves in place.** The existing custom module now reads an atomic sidebar projection rather than aggregating the legacy session list. It remains the glanceable surface and toggles the sidebar.
- **Transition rule:** keep the current hook-fed cache and live prototype working while durable registry behavior moves behind agent-switch interfaces. Remove the prototype sidecar only after per-thread registry files cover identity, manifests, lifecycle, and read markers.

This resolves the ownership and migration direction, not the runtime substrate: ticket 08 still decides window-hosted versus zmx-backed runtimes.
