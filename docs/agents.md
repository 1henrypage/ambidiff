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
`source` (`{kind: "git", base?: "<ref>"}`), `createdAt`, `updatedAt`,
`comments`, and `quarantined` (records that failed validation on a read,
preserved verbatim). Unknown fields round-trip untouched.

Comment fields:

| Field | Meaning |
| --- | --- |
| `id` | stable token, e.g. `c-7f3a2b1c`; lifecycle is keyed on it |
| `rev` | pass the comment was raised in (provenance; never mutated) |
| `status` | `open`, `addressed`, `resolved`, `reopened` |
| `path` | file path, or null for a review-level comment |
| `side` | `old` or `new`; removed lines anchor old-side numbers |
| `line`, `endLine` | 1-based; `line` null means file-level |
| `snippet` | source captured at comment time; drift detection compares it whitespace-insensitively |
| `body` | the comment; `??` marks a question |
| `response` | the agent's one-liner, set when addressing |
| `author`, `createdAt`, `updatedAt` | provenance |

## Lifecycle

```
open ----addressed----> addressed --resolved--> resolved
  ^                        |                       |
  |                        +------reopened---------+
  +--(reopened lands back on the agent's to-do list)
```

- Address: from open or reopened; anyone, typically the agent; optional
  response.
- Resolve: HUMAN ONLY; from open, addressed, or reopened.
- Reopen: HUMAN ONLY; from addressed or resolved.
- The agent to-do list is exactly {open, reopened}.

Illegal transitions fail with typed errors at the CLI and every frontend.
The CLI cannot verify who is human, so the resolve/reopen restriction is a
contract: the agent instructions forbid those verbs, and the interactive
frontends are the human path.

## Revisions

A pass ends when the agent finishes addressing: once no comment is open or
reopened, the next comment added (or comment reopened) bumps `revision`.
New comments carry the current revision in `rev`. `ambidiff rev bump`
starts a pass manually.

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
  `comment.resolve`, `comment.reopen`, `rev.bump`, `shutdown`.
  `initialize` also reports `readOnlyReason`, `generation`, `sourceError`,
  `comparison`, and `methods`; `files` adds `skipped`, `sourceError`,
  `comparison`, `warnings`, `generation`; `expand` returns `rows` plus
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
  `comment.reopen`, `rev.bump`. Errors are `{type: "error", id, code,
  message}`. Broadcasts `reviewChanged` and `diffChanged` carry the new
  state and `generation`. Messages are limited to 1 MiB inbound and 16 MiB
  outbound; at most 32 connections are served.

The exact shapes live in `fixtures/contracts/` and are asserted by the
Rust and browser test suites.
