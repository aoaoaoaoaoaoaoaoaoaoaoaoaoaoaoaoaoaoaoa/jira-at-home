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

Output defaults to compact porcelain. `render: "json"` changes encoding only,
including for tool errors. `detail: "full"` adds metadata in either encoding.

- `issue.list`: `view: "issues"` (default) returns selectors and epics; full
  detail adds timestamps and paths, never bodies. `view: "epics"` lists epic
  names instead. `epic` filters the issues view.
- Lists use `limit` (default 50, maximum 200) and `offset` (default 0). `count`
  is the filtered total; continue with `next_offset` and the same view/filter.
  Each page reads current state; concurrent edits can shift rows.
- `issue.read` returns the entire stored body at both detail levels.
- `system.telemetry` pages methods with `limit` (default 6, maximum 200),
  `offset`, and optional exact `method`. Totals remain session-wide.
  Full detail adds timings and fault context without expanding the page.
- Binding receipts show the canonical project and issue count; full detail
  adds storage and requested-path metadata.

Version 2 replaces unbounded lists and renderer previews with these selections.
Full detail never disables pagination; porcelain never clips selected content.

Run:

```bash
cargo run -- mcp serve --project .
```

The checked-in `.mcp-depot.toml` optionally enables eager immutable releases
and readiness-gated live handoff. It does not alter the standalone path above.
