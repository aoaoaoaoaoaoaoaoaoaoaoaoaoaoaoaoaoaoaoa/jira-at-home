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

Calls act on the project containing your working directory; pass `project` to
reach another. A project root or any path inside it is accepted. Reads do not
create an empty notebook; an unknown issue still reports not found. Existing
linked-worktree notes migrate to the shared namespace on first issue use.
Responses identify a non-default project; `system.health` reports the default.

Tools:

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

A body may open with TOML front matter between `+++` lines:

```text
+++
depends_on = ["bug/parser-crash", "feature/schema-v2"]
effort = 4
+++
The note itself.
```

- `depends_on` names existing issues, as `category/slug`, that must be done
  first. Edges carry no label. Saving rejects unknown issues, self-dependencies,
  duplicates and cycles. Deleting an issue means it is done: it leaves every
  `depends_on` that listed it.
- `effort` sizes the remaining work in doubling units: 1, 2, 4, 8, 16 or 32.
- `issue.save` stores the body verbatim, front matter included. Front matter
  written outside the server is read leniently: if malformed, it stays body text
  until the next save rejects it.

Output defaults to compact porcelain. `render: "json"` changes encoding only,
including for tool errors. `detail: "full"` adds metadata in either encoding.

- `issue.list`: `view: "issues"` (default) returns selectors, epics, effort and
  dependency counts; full detail adds `depends_on`, timestamps and paths, never
  bodies. `view: "epics"` lists epic
  names instead. `epic` filters the issues view.
- Lists use `limit` (default 50, maximum 200) and `offset` (default 0). `count`
  is the filtered total; continue with `next_offset` and the same view/filter.
  Each page reads current state; concurrent edits can shift rows.
- `issue.read` returns the entire stored body and the issues that depend on it
  at both detail levels. `issue.delete` reports the dependents it released.
- `system.telemetry` pages methods with `limit` (default 6, maximum 200),
  `offset`, and optional exact `method`. Totals remain session-wide.
  Full detail adds timings and fault context without expanding the page.

Version 2 replaces unbounded lists and renderer previews with these selections.
Full detail never disables pagination; porcelain never clips selected content.

Run:

```bash
cargo run -- mcp serve
```

The checked-in `.mcp-depot.toml` optionally enables eager immutable releases
and readiness-gated live handoff. It does not alter the standalone path above.
