---
name: jira-at-home
description: >-
  Use when you want drooling, braindead per-project issue parking without
  touching the worktree: bind a project, save a freeform note to the project's
  state-backed `issues/<feature|bug>/<slug>.md`, list existing issue slugs,
  read one issue back, or delete one outright.
---

Bind the target project with `project.bind` before issue work unless the MCP was started with `--project`. Use `issue.save` to overwrite `issues/<category>/<slug>.md` under the external state root; `category` is mandatory and must be exactly `feature` or `bug`. Use `issue.list` to enumerate live categorized notes, `issue.read` to recover one note by category and slug, and `issue.delete` to obliterate one note. Treat legacy `issues/*.md` root files as inert migration residue. Reach for `system.health` and `system.telemetry` only when the transport looks suspect.
