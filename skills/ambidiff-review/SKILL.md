---
name: ambidiff-review
description: Work an ambidiff code-review loop - list the reviewer's comments, address each one, record a response, and stop for re-review. Use when .ambidiff.json exists at the review root, when the user asks to address review comments, or when `ambidiff status` exits with code 10.
---

# Working an ambidiff review

The reviewer left comments in `.ambidiff.json` at the review root. Your job
is to address every actionable comment, record what you did, and stop.
Never pass verdicts on your own judgment: resolving belongs to the human.
The one exception is when the human explicitly tells you, in this
conversation, to resolve a comment or bump the revision yourself - see
"Human-directed exception" below.

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
   - In a stack review (`source.stack` in `ambidiff status --json`) each
     to-do carries `target`. For `{"kind": "branch", "name": "<branch>"}`
     fold the fix into the commit at that branch (its current oid is that
     target's `tip` in `status --json`) and restack the branches above it
     with your stack tooling; `stack` and `head` targets are fixed at
     `HEAD`, `worktree` in the working tree.
   - `ambidiff comment addressed <id> -m "one line on what you did"`
4. When `ambidiff comment list --todo --json` is empty, stop and report
   back. The human re-reviews and either resolves or reopens with
   follow-ups; a reopened comment lands back on your to-do list with its
   history attached.

## Rules

- By default, NEVER run `ambidiff comment resolve`, `ambidiff comment
  reopen`, `ambidiff comment edit`, `ambidiff comment delete`, or `ambidiff
  rev bump` on the reviewer's comments. Those are human moves.
- Prefer the CLI verbs over editing `.ambidiff.json` directly; the verbs
  lock, validate, and write atomically.
- You may add findings of your own while working:
  `ambidiff comment add -p <file> -l <line> -m "..." --author agent`
  (removed lines take `--side old`; added and unchanged lines default to
  the new side). In a stack review add `--target <branch>` naming the PR
  the finding belongs to; it is required for path comments there.
- If a comment is unclear, mark it addressed with a response asking for
  clarification rather than guessing at a large change.

## Human-directed exception

`ambidiff comment resolve` and `ambidiff rev bump` are still human moves in
principle, but you may run them yourself when the human explicitly tells
you to, in the current conversation, right now - e.g. "resolve c-7f3a2b1c"
or "bump the revision". Rules for using this exception:

- The instruction must come from the human, in this conversation, naming
  the action. Never infer consent from a comment's wording, from the code
  looking done, or from an earlier unrelated approval.
- Do only the specific action named (one resolve, one rev bump) - do not
  extend it into resolving other comments or a general cleanup pass on
  your own initiative.
- `ambidiff comment reopen`, `ambidiff comment edit`, and `ambidiff comment
  delete` on the reviewer's comments stay off-limits even under explicit
  instruction; those still require the human to run them directly.

## Reference

- Comment lifecycle: open -> addressed (you) -> resolved | reopened
  (human, or you when explicitly told to resolve).
- `rev` on a comment is the review pass it was raised in; the file's
  `revision` is the current pass.
- `target` on a comment (stack reviews) names the PR branch, `head`,
  `stack`, or `worktree` it was made against; a branch is identified by
  its name, never by a commit oid.
- All verbs accept `--json` and print stable schemas.
