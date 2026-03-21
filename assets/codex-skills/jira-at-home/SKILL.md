---
name: jira-at-home
description: Use when you want brutally basic per-project issue parking without touching the worktree: bind a project, save a freeform note to the project’s state-backed `issues/<slug>.md`, list existing issue slugs, or read one issue back. Keep the workflow primitive; there are no issue types, statuses, or schema beyond slug plus Markdown body.
---

Bind the target project with `project.bind` before issue work unless the MCP was started with `--project`; then use `issue.save` to create or overwrite `issues/<slug>.md` under the project’s external state root, `issue.list` to enumerate the currently open issue files, and `issue.read` to recover one note body by slug. Treat the store as a tiny parked-ideas notebook, not a tracker: every issue is just freeform Markdown outside the repo in the canonical `issues/` directory for that project, and the only operational tools worth touching are `system.health` and `system.telemetry` when the transport itself looks suspect.
