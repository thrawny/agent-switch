# Sidebar prototype — later considerations

Idea dump for possible follow-up work. These are not active tasks; do not implement them unless explicitly requested.

- Cold resurrection must account for the user's sandbox wrapper and direnv environment, rather than blindly spawning `ghostty -e <harness resume>`.
- Support an agent "hand raise" / background-work state. A session waiting for background tasks is currently reported as Idle, but in practice it remains in flight and should be represented separately (or continue to count as Working).
