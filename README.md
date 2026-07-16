# jira-at-home

drooling, braindead issue tracking MCP.

It parks Markdown outside the repo:

```text
~/.local/state/jira_at_home/projects/.../issues/<feature|bug>/<slug>.md
```

Git-linked worktrees share the same issue namespace via Git's common-dir
identity; the linked worktree path is only the attachment point.

Tools:

- `project.bind`
- `issue.save`
- `issue.read`
- `issue.list`
- `issue.delete`
- `system.health`
- `system.telemetry`

Rules:

- every issue is `feature` or `bug`
- category is mandatory
- delete means obliterate

Run:

```bash
cargo run -- mcp serve --project .
```
