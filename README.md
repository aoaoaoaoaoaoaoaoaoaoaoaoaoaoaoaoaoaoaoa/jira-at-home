# jira-at-home

drooling, braindead issue tracking MCP.

It parks Markdown outside the repo:

```text
~/.local/state/jira_at_home/projects/.../issues/<feature|bug>/<slug>.md
~/.local/state/jira_at_home/projects/.../issues/<feature|bug>/<epic>/<slug>.md
```

An epic is only a subdirectory; it holds no metadata beyond its name. A slug
is unique within its category across every epic — saving an existing slug
under a different epic (or none) moves the file there and prunes the old
epic directory once it is empty.

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

The checked-in `.mcp-depot.toml` optionally enables eager immutable releases
and readiness-gated live handoff. It does not alter the standalone path above.
