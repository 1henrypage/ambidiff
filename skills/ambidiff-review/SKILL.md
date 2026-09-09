---
name: ambidiff-review
description: Work an ambidiff code-review loop - list the reviewer's comments, address each one, record a response, and stop for re-review. Use when .ambidiff.json exists at the review root, when the user asks to address review comments, or when `ambidiff status` exits with code 10.
---

# Working an ambidiff review

The reviewer left comments in `.ambidiff.json` at the review root. Your job
is to address every actionable comment, record what you did, and stop.
Never pass verdicts: resolving and reopening belong to the human.

## The loop

1. `ambidiff status --json` - overview. Exit code 10 means comments await
   you; 0 means you are done.
2. `ambidiff comment list --todo --json` - your to-do list (status `open`
   or `reopened`).
3. For each comment, in any order:
   - Read it. `path`/`side`/`line` locate it; `snippet` is the code as it
     looked when the comment was written. If lines have moved since, find
     the code by its content, not the line number.
   - A body containing `??` is a QUESTION. Answer it in the response
     instead of changing code, unless the answer itself implies a fix.
   - Otherwise make the change it asks for.
   - `ambidiff comment addressed <id> -m "one line on what you did"`
4. When `ambidiff comment list --todo --json` is empty, stop and report
   back. The human re-reviews and either resolves or reopens with
   follow-ups; a reopened comment lands back on your to-do list with its
   history attached.

## Rules

- NEVER run `ambidiff comment resolve`, `ambidiff comment reopen`,
  `ambidiff comment edit`, or `ambidiff comment delete` on the reviewer's
  comments, and never change `revision`. Those are human moves.
- Prefer the CLI verbs over editing `.ambidiff.json` directly; the verbs
  lock, validate, and write atomically.
- You may add findings of your own while working:
  `ambidiff comment add -p <file> -l <line> -m "..." --author agent`
  (removed lines take `--side old`; added and unchanged lines default to
  the new side).
- If a comment is unclear, mark it addressed with a response asking for
  clarification rather than guessing at a large change.

## Reference

- Comment lifecycle: open -> addressed (you) -> resolved | reopened (human).
- `rev` on a comment is the review pass it was raised in; the file's
  `revision` is the current pass.
- All verbs accept `--json` and print stable schemas.
