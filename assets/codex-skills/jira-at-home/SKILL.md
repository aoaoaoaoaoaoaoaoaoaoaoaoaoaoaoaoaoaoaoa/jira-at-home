---
name: jira-at-home
description: >-
  Use when you want brutally basic per-project issue parking without touching
  the worktree: bind a project, save a freeform note to the project's
  state-backed `issues/<category>/<slug>.md`, list existing issue slugs, read
  one issue back, or delete one outright. Keep the workflow primitive; the only
  mandatory schema beyond slug plus Markdown body is the closed category enum
  `feature | bug`.
---

Bind the target project with `project.bind` before issue work unless the MCP was started with `--project`; then use `issue.save` to create or overwrite `issues/<category>/<slug>.md` under the project’s external state root. The category is mandatory and must be exactly `feature` or `bug`; `issue.list` enumerates the currently open issue files across both categories, `issue.read` recovers one note body by category and slug, and `issue.delete` removes one parked note outright. Treat the store as a tiny parked-ideas notebook, not a tracker: every issue is just freeform Markdown outside the repo in the canonical `issues/` directory for that project, and the only operational tools worth touching are `system.health` and `system.telemetry` when the transport itself looks suspect.
