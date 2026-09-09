# Projection conformance fixtures

Hand-written golden expectations for `ReviewProjection` / `ViewState` /
`FileView::assemble`, in the same spirit as `fixtures/cases/` (S6): each
case's `expected.json` was worked out from the documented semantics, not
copied from whatever the code currently produces, so a buggy primitive
cannot self-validate. `crates/core/tests/projections.rs` runs every case
natively (reading from disk) and under wasm32 (embedded via `include_dir`,
the same pattern `tests/conformance.rs` uses).

Each case directory holds:

- `input.json` -- case-specific inputs (files, review comments, raw diffs,
  filter/options), shaped however that case's runner in `projections.rs`
  expects it (there is one runner function per case, not one generic
  schema, because the cases exercise different parts of the API).
- `expected.json` -- the golden output for that case's runner.

## Cases

- `filtered-tree` -- `ReviewProjection::tree_rows` under `Annotated`: an
  unreviewed file drops out, its directory is pruned when nothing else in
  it passes, and every surviving leaf keeps its canonical `file_index` and
  correct `commentsTodo`/`commentsTotal` (S1, B15).
- `rename-unattached` -- a comment on a file's rename origin still carries
  its `wasPath` badge in `file_comments`; a comment on a file that vanished
  beyond git's rename detection surfaces in `overview()` as unattached,
  after the review-level comments (never silently dropped).
- `placeholder-kinds` -- `FileView::assemble` over `FileDiff::placeholder`
  for both `Binary` and `TooLarge`: each wire count appears exactly once,
  `rows` stays empty, and `desc` only exists for binary (B16).
- `unicode-search` -- `ViewState::search` over CJK content: byte offsets
  index the original UTF-8 text, not a codepoint count.
- `expansion-option-change` -- expanding a gap, then changing view options
  (unified -> split): the held expansion is replayed over the rebuilt row
  stream, so the option change does not silently re-collapse it.
- `line-extrema` -- the full parse -> row pipeline at the edges of the u32
  line-number range: an accepted header at `old_start = u32::MAX` builds
  rows without panicking (B24), and a trailing gap computed against
  `old_total_lines = u32::MAX` reports the exact boundary range. This case
  has no `expected.json`: its expectations are the boundary values
  asserted directly in its runner.
