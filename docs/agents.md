# The agent contract

Everything an agent (or any tool) needs to interoperate with an ambidiff
review. The file is the contract; the CLI verbs are the recommended way to
honour it.

## Discovery

`ambidiff agent-setup` installs a standing instruction block into
`AGENTS.md` (created if absent) and `CLAUDE.md` (updated only when
present), delimited by `<!-- BEGIN ambidiff (vN) -->` / `<!-- END ambidiff -->`.
The block is idempotent: rerunning replaces a stale version in place.
`--check` verifies without writing and exits 1 when setup is needed.
`--claude-skill` also installs `.claude/skills/ambidiff-review/SKILL.md`
for Claude Code.

Context files are baked at session load, so discovery relies on this
standing instruction, not on an agent noticing `.ambidiff.json` by itself.

## The file

`.ambidiff.json` at the review root. Its sidecars are `.ambidiff.json.guard`
(a persistent advisory-lock file, never deleted), `.ambidiff.json.lock`
(the ownership record of the writer currently holding the guard), and
`.ambidiff.json.tmp.*` (in-flight atomic writes). Schema major is the
`ambidiff` field; this build writes 1.

Top level: `review` (name), `revision` (current pass, starts at 1),
`source`, `createdAt`, `updatedAt`, `comments`, and `quarantined` (records
that failed validation on a read, preserved verbatim). Unknown fields
round-trip untouched.

`source` selects the comparison. Exactly one selector is expected:

| Shape | Meaning |
| --- | --- |
| `{kind: "git"}` | index against the working tree |
| `{kind: "git", base: "<ref>"}` | the current branch from `merge-base(<ref>, HEAD)` against the working tree: its own commits plus staged, unstaged and untracked changes, never what landed on `<ref>` since the fork; `staged: true` swaps the working tree for the index |
| `{kind: "git", base: "A..B"}` / `"A...B"` | two commits, with git's meanings: `A..B` is A against B, `A...B` is B against its merge base with A; neither involves the working tree |
| `{kind: "git", commit: "<spec>"}` | one commit against its first parent (`ambidiff init --commit HEAD`) |
| `{kind: "git", stack: {upstream?: "<ref>"}}` | a stack of PR branches above trunk (`ambidiff init --stack`); `upstream` overrides trunk auto-detection (`origin/HEAD`, then `origin/main`, `origin/master`, `main`, `master`) |

When several selectors are present `stack` wins over `commit`, which wins
over `base`, and every frontend shows a warning. A wrong-typed selector
makes `source` unreadable and the file read-only rather than silently
rewritten into another mode.

Comment fields:

| Field | Meaning |
| --- | --- |
| `id` | stable token, e.g. `c-7f3a2b1c`; lifecycle is keyed on it |
| `rev` | pass the comment was raised in (provenance; never mutated) |
| `status` | `open`, `addressed`, `resolved`, `reopened` |
| `path` | file path, or null for a review-level comment |
| `side` | `old` or `new`; removed lines anchor old-side numbers |
| `line`, `endLine` | 1-based; `line` null means file-level |
| `target` | stack reviews only: the target the comment was made against, `{kind: "branch", name}`, `{kind: "head"}`, `{kind: "stack"}`, or `{kind: "worktree"}`; absent outside stacks and on comments older than stacks |
| `snippet` | source captured at comment time (against the comment's own `target` in a stack review); drift detection compares it whitespace-insensitively |
| `body` | the comment; `??` marks a question |
| `response` | the agent's one-liner, set when addressing |
| `author`, `createdAt`, `updatedAt` | provenance |

## Stacks

A stack review (`source.stack`) has several **targets**: one per PR branch
above trunk (bottom to top, `position` 1..n), `head` when commits sit above
the topmost branch, `stack` (merge-base to `HEAD`) and `worktree` (`HEAD`
against the working tree) when the tree is dirty. PR identity is the branch
NAME: an amend, a `git rebase --update-refs`, or a bottom PR landing moves
every tip oid but keeps the names, so comments stay on their PR. Oids are
a cache for display, never a key.

- Every path-anchored comment in a stack review MUST carry `target`;
  `ambidiff comment add` requires `--target <branch|stack|worktree|head>`
  (`branch:<name>` forces a branch named like a keyword). Review-level
  comments may carry one or not. A target outside a stack review is
  rejected.
- Viewing one target hides the comments made on the other live targets. A
  comment whose target left the stack (merged, deleted) joins the
  unattached group with a "was on <name>" badge, visible from every target.
- `ambidiff status --json` in a stack review adds `stack {trunk, base,
  head, targets: [{id, label, position, tip, commitCount, subject,
  aliases, comparison, counts: {todo, total}}]}`, `untargeted` and `wasOn`
  tallies, and `stackError` (a discovery failure; the exit code still
  follows the to-do count). `comment list --target <t>` filters by target.
- To act on a to-do with `target: {kind: "branch", name}`: make the fix in
  the commit at that branch (its current oid is `tip` in `status --json`)
  and restack the branches above it with your stack tooling, then mark the
  comment addressed as usual. Never resolve.
- The selected target is per process (per TUI, per `ambidiff web` server
  and all of its tabs, per engine); it is never written to the review file.

## Lifecycle

```
open ----addressed----> addressed --resolved--> resolved
  ^                        |                       |
  |                        +------reopened---------+
  +--(reopened lands back on the agent's to-do list)
```

- Address: from open or reopened; anyone, typically the agent; optional
  response.
- Resolve: HUMAN ONLY; from open, addressed, or reopened. Exception: an
  agent may run `ambidiff comment resolve` when a human explicitly directs
  it to, in the moment, naming the comment; absent that instruction it is
  still off-limits to agents.
- Reopen: HUMAN ONLY; from addressed or resolved. No agent exception - an
  agent must never run `ambidiff comment reopen`, even under explicit
  instruction.
- Resolve addressed: HUMAN ONLY; bulk-resolves every `addressed` comment in
  one action (TUI `X`, neovim `gX`, browser `X`); open and reopened
  comments are untouched. No agent exception; there is no CLI verb for it,
  so it is reachable only through an interactive frontend.
- The agent to-do list is exactly {open, reopened}.

Illegal transitions fail with typed errors at the CLI and every frontend.
The CLI cannot verify who is human, so the resolve/reopen/resolve-addressed
restriction is a contract, not a code-level check: agent instructions
forbid reopen and resolve-addressed outright, and forbid resolve except
when a human explicitly directs that one action in the conversation; the
interactive frontends remain the ordinary human path for all three.

## Revisions

A pass ends when the agent finishes addressing: once no comment is open or
reopened, the next comment added (or comment reopened) bumps `revision`.
New comments carry the current revision in `rev`. `ambidiff rev bump`
starts a pass manually; like resolve, this is a human call that an agent
may run only when a human explicitly directs it to in the conversation.

## Concurrency

Verbs and frontends hold the guard lock for the whole read-modify-write
and write via an exclusive temp file + rename, so a reader never observes a
torn file and two writers never interleave. A lock is never broken by age:
a crashed holder's sidecar is reclaimed by the next writer under the guard,
and a sidecar written by a pre-repair build is reclaimed only when its
process is provably gone. Close older ambidiff instances before upgrading;
keep review roots on local filesystems. Direct whole-file edits by agents
are tolerated: reads are salvage-mode (bad records quarantined with
warnings, never dropped), a top-level value that cannot be represented
faithfully is retained and makes the file read-only until repaired, and a
file from a newer schema major opens read-only. `ambidiff status --json`
reports `readOnly` and `readOnlyReason`.

## Verbs and exit codes

Every verb accepts `--json` with a stable schema. `ambidiff status` exits
10 when actionable comments exist, 0 when clean; everything else uses 0/1.
See `ambidiff --help` and the README for the full listing.

## Wire protocols

- `ambidiff engine --stdio`: newline-delimited JSON for editor plugins,
  protocol version 1 in the `initialize` handshake, notifications
  `reviewChanged` / `diffChanged`. Methods: `initialize`, `review`, `files`,
  `view`, `expand`, `commands`, `comment.add`, `comment.edit` (`{id, body}`),
  `comment.delete` (`{id}` -> `{deleted}`), `comment.address`,
  `comment.resolve`, `comment.reopen`, `comment.resolveAddressed` (no
  payload), `rev.bump`, `target.select` (`{target}` -> `{selected, targets,
  comparison, generation}`; `unknownTarget` / `notStackReview` are
  `-32602`), `shutdown`. `comment.add` accepts `target`. `comment.resolve` and `rev.bump` are
  human only except one call an agent makes when a human explicitly
  directs it to in the moment; `comment.reopen` and
  `comment.resolveAddressed` are human only with no exception.
  `initialize` also reports `readOnlyReason`, `generation`, `sourceError`,
  `listing`, `comparison`, `targets`, `selected`, `commit`, and `methods`
  (a client feature-detects `target.select` there; the protocol version
  stays 1); `files` adds `skipped`, `sourceError`, `listing`,
  `comparison`, `targets`, `selected`, `commit`, `warnings`, `generation`.
  `listing` is `fresh`, `stale` (the last listing failed; `files` is the
  previous listing, kept) or `unavailable` (no listing; `files` is `[]`),
  so a failed listing is never mistaken for an empty one; a
  `target.select` is
  followed by exactly one `diffChanged`; `expand` returns `rows` plus
  `at`, `gap`, `view`, `comments`, `generation` (a later `view` already
  contains every expansion). Invalid input (decoding, validation, unknown
  path or gap) is error code `-32602` with `error.data.kind`; other
  failures are `-32000`. Request lines are limited to 1 MiB.
- `ambidiff web`: loopback websocket on the exact `/ws` route; the page
  authenticates with the token from the URL fragment as its first message,
  within a five-second handshake deadline. Every request carries an `id`
  that its reply echoes. Requests: `refresh` (-> a full `snapshot`),
  `getFile`, `getSrc` (listed paths only), `comment.add`, `comment.edit` and
  `comment.delete` (`commentId`), `comment.address` / `comment.resolve` /
  `comment.reopen` / `comment.resolveAddressed` / `rev.bump` (all human
  only; `comment.resolve` and `rev.bump` have one exception, an agent call
  made only when a human explicitly directs it to in the moment -
  `comment.reopen` and `comment.resolveAddressed` have none), and
  `target.select` (`{id, target}` -> a `targetSelected` acknowledgement
  `{selected, targets, comparison, generation}` followed by a `diffChanged`
  broadcast to every tab, the requester included; the selection is shared
  by every tab of one server). Errors are `{type: "error", id, code,
  message}`. `hello`, `snapshot`, and `diffChanged` carry `listing`,
  `targets`, `selected`, and `commit`. Broadcasts `reviewChanged` and `diffChanged`
  carry the new state and `generation`. Messages are limited to 1 MiB
  inbound and 16 MiB outbound; at most 32 connections are served.

The exact shapes live in `fixtures/contracts/` and are asserted by the
Rust and browser test suites.
