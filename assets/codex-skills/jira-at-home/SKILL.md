---
name: jira-at-home
description: >-
  Per-project notebook of bug and feature notes, stored outside the worktree.
  Use to record a problem or idea for later, find or read existing notes, or
  delete notes whose work is done. Not a ticket system: notes have no status,
  assignee, or workflow.
---

Bind the project with `project.bind` before other calls, unless the server was
started with `--project`. Notes live under the server's state root, not in the
worktree, as `issues/<category>/<slug>.md`, where `category` is `bug` or
`feature`; an optional epic adds a subdirectory.

- `issue.save` creates or overwrites a note; changing its epic moves it.
- `issue.list` returns 50 selectors per page by default; continue with
  `next_offset` and the same filter. `view:"epics"` lists epic names instead.
  Full detail adds metadata, not rows.
- `issue.read` returns the complete body; `issue.delete` removes a note.
- Use `system.health` and `system.telemetry` only when the server seems faulty.

Porcelain and JSON carry the same facts; read porcelain. Ignore root-level
`issues/*.md` files, which are residue of an older layout.

## Writing notes

- List existing slugs first, and update a matching note instead of adding another.
- Say how the problem is known: reproduced, with the command or input, or
  inferred from reading code. An unreproduced diagnosis may be wrong.
- Refer to code by file and symbol; line numbers drift.
- If the owner deferred the item, say so in the first line.
- When the work is done, delete the note; when part remains, rewrite the note to
  that part.

## Dependencies and effort

A body may open with TOML front matter between `+++` lines:

```text
+++
depends_on = ["bug/parser-crash"]
effort = 4
+++
```

- `depends_on` lists issues, as `category/slug`, that must be done first.
  Edges carry no label. Deleting a done issue removes it from every list.
- `issue.save` replaces the whole body: keep the front matter when rewriting.
- `effort` sizes the remaining work as 1, 2, 4, 8, 16 or 32. Compare scope with
  these rungs; never convert from duration, because human schedules do not
  describe agent throughput.
  - 1: one known change in one place, checked by one targeted command.
  - 2: a contained change in one module, with a few tests or snapshots.
  - 4: a slice across a few modules or two crates, after reading and one design
    decision.
  - 8: a cross-cutting change landing as several commits.
  - 16: a feature that adds protocol or UI surface and still has open questions.
  - 32: a campaign: split it, or run it as a Vauban siege.
- Unknowns are size: between two rungs, take the larger. Re-size when the
  remaining work changes.
