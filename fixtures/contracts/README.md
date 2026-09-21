# Wire contract fixtures

Hand-specified JSON for every message shape the three transports exchange.
One scenario runs through all of them: review `fixture` at `/tmp/review`
comparing commit `1111...` with the worktree, one modified file
`src/login.ts` (`@@ -1,2 +1,3 @@`, old file 5 lines), and one open line
comment `c-7f3a2b1c` on new-side line 2 whose snippet is `B`.

A stack addendum extends the scenario for stacked-PR review: trunk `main`
at commit `1111...`, PR branch `auth-1` at tip `2222...` ("Add login
form"), PR branch `auth-2` at tip `3333...` ("Wire the session cookie"), a
clean stack (`1111...3333`) and a dirty working tree, so the stack has four
targets: `auth-1`, `auth-2`, `stack`, `worktree`. The selected target is
`auth-2`.

These files are the contract. Code is changed to match them, never the
other way round; a deliberate wire change edits the fixture first and the
diff is reviewed as a protocol change.

## Files

| File | Shape | Checked by |
| --- | --- | --- |
| `request-*.json` (except envelopes) | decoder cases: `payload` plus either `ok` (the decoded value) or `error` (`kind`, `field`); `idField` selects the `id` / `commentId` spelling | `crates/core/tests/contracts.rs`, `web/src/protocol.test.ts` |
| `request-envelopes.json` | one valid envelope per stdio method and per browser message type | `crates/core/tests/contracts.rs` (payloads decode), `web/src/protocol.test.ts` (client request union) |
| `stdio-*.json` | the `result` value of each stdio method | core-level shapes in `crates/core/tests/contracts.rs`; envelope builders in `crates/cli/tests/engine_stdio.rs` |
| `web-*.json` | one server-to-page message each | builders in `crates/cli/tests/web_protocol.rs`; decoders in `web/src/protocol.test.ts` |

Stack-specific files: `request-target-select.json` (decoder cases for the
`target.select` payload), `stdio-target-select.json` (the result of the
stdio method), `web-target-selected.json` (the browser acknowledgement),
and `stdio-comment-add-target.json` (a comment carrying a `target`).

Core-level shapes (comment, review file, file entry, file view, anchored
comment, expansion rows) are asserted byte-for-byte against the core's own
serialisation. `stdio-view-toolarge.json` carries each count field exactly
once with the numstat preflight counts; the projection task wires its
assertion in when the view serialisation is repaired (finding B16).

## Decoding rules (all transports)

- absent or `null` means omitted; any other wrong JSON type is `wrongType`;
- `line` and `endLine` are integral and within `1..=4294967295`, never
  truncated (`outOfRange` otherwise);
- `side` is exactly `old` or `new` (`unknown` otherwise); an omitted side on
  a line comment defaults to `new`;
- view options default to `{mode: "unified", wordDiff: true, theme: "dark"}`;
  `theme: "none"` disables highlighting;
- a blank lifecycle `response` is omitted (nothing is stored).
- `target` is an object `{kind, name?}`: `kind` is one of `branch`, `head`,
  `stack`, `worktree` (`unknown` otherwise); `branch` requires a string
  `name` (`missing` / `wrongType` on `target.name`); anything but an object
  is `wrongType` on `target`. `comment.add` takes it optionally;
  `target.select` requires it.

## Targets

Every snapshot-like message (`stdio-initialize`, `stdio-files`, `web-hello`,
`web-snapshot`, `web-diff-changed`) always carries three keys: `targets`
(the stack's targets in order, `[]` for a non-stack review), `selected`
(the selected `TargetId`, `null` when there is none) and `commit` (the
`{oid, subject}` banner of a single-commit review, `null` otherwise). A
target serialises every field, none skipped: `id`, `label`, `position`
(1-based for PR branches, `null` otherwise), `tip` (`null` for the
worktree target), `commitCount`, `subject` (display only), `aliases`
(other branches on the same tip) and `comparison`.

The selected target is per process: one `ambidiff web` server (and every
tab connected to it) or one engine shares a single selection, and it is
never written to `.ambidiff.json`. `target.select` on the stdio wire
returns `{selected, targets, comparison, generation}` (the generation is
bumped because the listing is retaken); in the browser the reply is a
`targetSelected` acknowledgement with the same payload plus `type` and
`id`, followed by a `diffChanged` broadcast to every tab, the requesting
one included, which carries the new file list. Selecting a target on a
non-stack review fails with kind `notStackReview`, a target that is not
in the stack with `unknownTarget`; both are invalid input (`-32602` on
stdio).

## Listing state

Every snapshot-like message also carries `listing`, one of `fresh` (the
files are this load's listing), `stale` (the last listing attempt failed
and the files are the previous successful listing, kept) or `unavailable`
(no listing could be produced: the source never opened, or failed before
any listing succeeded; `files` is `[]`). A failed listing is therefore
never confused with a successful empty one: `sourceError` says why, and
`listing` says what the files mean. `listing-state.json` pins the three
spellings and the tree placeholder each state paints for a given file
count (`no changes`, `showing previous listing`, `source unavailable`, or
none), which every frontend derives from the same rule. A message built
before any snapshot loaded (`snapshot` with `readOnlyReason: "review not
loaded"`, or a `diffChanged` whose `sourceError` says so) is
`unavailable`.

## Error envelopes

Stdio: `{"id", "error": {"code", "message", "data": {"kind"}}}` with
`-32602` for `decode`, `review` validation, `invalidPath`,
`notInChangedSet`, `noSuchGap`, `unknownTarget`, and `notStackReview`; `-32601` for an unknown method;
`-32700` for a parse error; `-32000` otherwise.

Browser: `{"type": "error", "id", "code", "message"}` where `code` is the
application error kind (`store`, `source`, `review`, `decode`, `expand`,
`notInChangedSet`, `noSuchGap`, `noSource`, `notLoaded`, `watch`,
`unknownTarget`, `notStackReview`) or a
transport code (`unauthorized`, `unknownType`, `tooLarge`). Every response
echoes the request `id`.
