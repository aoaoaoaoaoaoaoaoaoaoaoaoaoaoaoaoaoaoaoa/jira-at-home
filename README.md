# jira-at-home

drooling, braindead issue tracking MCP.

No workflow. No status. No ceremony.

It parks Markdown outside the repo:

```text
~/.local/state/jira_at_home/projects/.../issues/<feature|bug>/<slug>.md
```

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
- legacy root files under `issues/*.md` are ignored

Run:

```bash
cargo run -- mcp serve --project .
```
