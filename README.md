# jira_at_home

`jira_at_home` is an intentionally tiny MCP for parking project-local ideas.

The domain is mercilessly small:

- `~/.local/state/jira_at_home/projects/.../issues/<slug>.md` stores the actual note body
- `issue.save` overwrites or creates one note
- `issue.list` enumerates the existing slugs
- `issue.read` returns the note body for one slug

The feature set stays primitive, but the transport posture is not:

- durable stdio host with a disposable worker
- explicit replay contracts
- porcelain-by-default tool output
- hot host reexec through `libmcp` session snapshots
- issue bodies and append-only JSONL telemetry outside the repo under the platform state dir

Use `cargo run -- mcp serve --project .` to launch it against the current repo.
