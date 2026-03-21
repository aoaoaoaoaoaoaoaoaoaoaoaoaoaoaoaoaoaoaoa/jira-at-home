---
name: jira-at-home
description: Use when you want brutally basic per-project issue parking in this repository: bind a project, save a freeform note to `issues/<slug>.md`, list existing issue slugs, or read one issue back. Keep the workflow primitive; there are no issue types, statuses, or schema beyond slug plus Markdown body.
---

Bind the target project with `project.bind` before issue work unless the MCP was started with `--project`; then use `issue.save` to create or overwrite `issues/<slug>.md`, `issue.list` to enumerate the currently open issue files, and `issue.read` to recover one note body by slug. Treat the store as a tiny parked-ideas notebook, not a tracker: every issue is just freeform Markdown in the canonical `issues/` directory, and the only operational tools worth touching are `system.health` and `system.telemetry` when the transport itself looks suspect.
