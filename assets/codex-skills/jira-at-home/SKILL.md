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
