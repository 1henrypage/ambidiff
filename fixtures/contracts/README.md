# Wire contract fixtures

Hand-specified JSON for every message shape the three transports exchange.
One scenario runs through all of them: review `fixture` at `/tmp/review`
comparing commit `1111...` with the worktree, one modified file
`src/login.ts` (`@@ -1,2 +1,3 @@`, old file 5 lines), and one open line
comment `c-7f3a2b1c` on new-side line 2 whose snippet is `B`.

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

## Error envelopes

Stdio: `{"id", "error": {"code", "message", "data": {"kind"}}}` with
`-32602` for `decode`, `review` validation, `invalidPath`,
`notInChangedSet`, and `noSuchGap`; `-32601` for an unknown method;
`-32700` for a parse error; `-32000` otherwise.

Browser: `{"type": "error", "id", "code", "message"}` where `code` is the
application error kind (`store`, `source`, `review`, `decode`, `expand`,
`notInChangedSet`, `noSuchGap`, `noSource`, `notLoaded`, `watch`) or a
transport code (`unauthorized`, `unknownType`, `tooLarge`). Every response
echoes the request `id`.
