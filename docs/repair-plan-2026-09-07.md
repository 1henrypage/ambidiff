# Repair review integrity and unify frontend state

> **Status (9 September 2026).** Executed in full on `main`: task A (contracts), tasks B, C, D (core), tasks E, F (application, transports, verification), tasks G, H (frontends), followed by the documentation pass and an adversarial review. Two defects discovered during execution were added to the review's ledger and fixed (object reads from a subdirectory review root; the watch thread returning before its OS registration). The gate is `scripts/check.sh`.

## Context

ambidiff has useful core algorithms and a meaningful test foundation, but several paths can lose review information or display a different file/version from the one the user selected. The repair should establish trustworthy persistence and source identity, then put shared review/view behaviour behind a small core interface and migrate the frontends to it.

The [review report](repository-review-2026-09-07.md) records 27 grouped behavioural findings, six standards findings, and three bounded hardening areas at commit `245fb51`. Its B/S/H identifiers are used below for traceability. Findings classified as source-confirmed still need an end-user reproduction before their implementation begins; failed hypotheses must be corrected in the report rather than converted into speculative fixes.

The code-review skill kept standards and behavioural findings distinct. The planning and codebase-design skills informed the dependency ordering and the choice to move repeated behaviour behind existing core seams. This plan is a repair of the current product, with all three frontend interfaces retained.

## Decisions and compatibility

- Preserve schema major 1, existing comment IDs and origin revisions, unknown JSON fields, CLI flags/exit codes, and stdio protocol version 1. Rejecting previously coerced invalid requests is a bug fix. Retain existing response fields and discriminator spellings, including `toolarge` in serialised core views and `tooLarge` in the browser's raw-file transport.
- Internal Rust interfaces may change. Additive stdio methods and fields are allowed. New clients must not require a version bump merely to recognise additions; the existing sibling Neovim journey remains a compatibility gate.
- Keep a single pure core and a native feature layer. Reuse `ReviewFile`, lifecycle transitions, `DiffSource`, `build_file_view`, row construction, anchoring, and search. Introduce no daemon, event-sourced store, frontend framework, or separate implementation of diff semantics.
- Keep review roots at their existing locations, including initialisation inside a Git subdirectory. Paths exposed by ambidiff are relative to that review root. Scope filesystem reads to that root; Git may consult repository metadata and selected Git objects.
- Preserve line-comment snippet semantics: automatic capture records the starting line, as today. Range endpoints continue to round-trip. A broader range-snippet format is outside this compatibility-preserving repair.
- Target the existing native macOS/Linux workflow and browser WASM build. Do not claim Windows support without adding its separate platform implementation and acceptance suite.
- Keep local verification as the enforced workflow. CI was discussed but not selected; this plan adds a reproducible local command and asset checks without adding a CI provider.
- Integration base is the existing `main` at `245fb51`, unless the repository has moved before execution. Recheck that baseline first. No branch creation, pushes, PRs, or implementation are authorised by the review itself.
- Use this Markdown plan directly. Do not use Beads or create tracker issues.

## Changes

### 1. Establish shared contracts and a regression ledger

Start by recording the current valid wire shapes as small contract fixtures, including every comment operation and normal/binary/oversized views. Correct the oversized expectation to one occurrence of each count field with the actual preflight counts; do not bless the current duplicate-key output.

Add typed native request structures for comment creation, editing, deletion, lifecycle actions, and view options. Keep the two transport envelopes distinct: stdio still uses `{id, method, params}`, while browser messages retain `type` and `id`. Decode into common payload types after envelope parsing. Optional fields must distinguish omission/null from an incorrect type. Preserve the omitted-side default for a line comment; explicitly supplied unknown sides fail.

Create the internal native application interface used by all adapters: load a review snapshot, refresh its source, load/expand a file, and execute a typed review command. A command returns its value plus warnings, or a typed error. The interface owns no terminal or DOM concepts. Establish these shapes before parallel migrations; avoid exporting a second competing public Rust interface for each frontend.

Only the foundation task changes shared manifests and adds initial module declarations. `getrandom` and `libc` appear in the lockfile only as transitive dependencies and `tempfile` only as a dev dependency, so the foundation task adds them explicitly: `tempfile` (optional, under the `native` feature) and `libc` (unix-only, optional, under `native`) to the core, and `getrandom` plus `tempfile` as runtime dependencies of the CLI. All are gated out of WASM. Subsequent task owners use those declared dependencies without concurrently editing manifests.

Create each defect regression in its owning repair task. Run it against the old behaviour to demonstrate failure, implement the fix, then merge the passing test and fix together. Do not introduce an intentionally red integration branch. Add checked internal operations alongside the existing signatures during core fan-out, so adapters continue to compile before their migration. The TUI task owns removal of superseded internal wrappers after the last native caller migrates; the browser task does not edit those core/native interfaces.

### 2. Make review mutation and persistence preserve information

Addresses B01-B03 and the domain portion of S3, centred on `crates/core/src/review.rs` and `crates/core/src/store.rs`.

Give `ReviewFile` checked add, edit, delete, lifecycle, and revision operations. Validate new/edit inputs once at the domain interface, preserve provenance and unknown fields, and return typed errors for missing IDs, duplicate IDs, illegal transitions, invalid coordinates, and revision exhaustion. A failed operation must not partially mutate the in-memory review. Validate the resulting writable review before serialisation; serialization failure must never substitute `{}`.

Retain salvage for individually malformed comments by quarantining their original JSON. If a present top-level value cannot be represented without dropping or changing its content, retain the original value in the load outcome, emit a warning, and make the document read-only. This includes malformed quarantine/source containers and out-of-range revisions. Missing legacy fields may retain documented defaults and warnings. Keep the original JSON available to diagnostics/transport so a read-only load is not accidentally rewritten as a normalised lossy document.

Replace age-only exclusion with an OS advisory guard held for the complete transaction. Use a persistent `.ambidiff.json.guard` file for that guard, keeping the documented `.ambidiff.json.lock` as the create-exclusive ownership sidecar. Never unlink the guard file. Sidecar creation, stale recovery, and owner-checked release occur while holding the guard. Include a fresh ownership nonce alongside the existing PID/timestamp fields. Remove only the sidecar owned by the current guard holder. A live or indeterminate owner is not stale merely because ten seconds passed; legacy dead-owner recovery must positively establish process exit and serialise competing reclaimers. Unknown/corrupt ownership is a recoverable lock error, not permission to delete another process's file. The guard primitive is `std::fs::File::lock` / `try_lock`, stable since Rust 1.89. [Rust provides file-lock operations for this guard.](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)

The stronger guarantee applies to upgraded writers. An older binary that still deletes live locks cannot be made safe by changes to a different process. Document closing older ambidiff instances before upgrading, and keep existing schema-1 review files usable without a data migration.

Use a randomly named, exclusively created temporary file in the review directory; retain file sync, atomic replacement, and directory sync. Preserve sensible existing permissions and clean up only the temporary file owned by the failed operation. Reject symlink destinations for review/ownership files instead of following or replacing them accidentally. Add the new guard file to exclusion and watcher-sidecar handling in the same wave.

Acceptance: racing processes lose no updates; a holder exceeding ten seconds cannot be displaced; a dead holder can be recovered once; a superseded holder cannot delete another owner's sidecar; malformed retained values round-trip or block writes without changing bytes; overflow in all three revision-increment paths returns an error; failed temp/write/rename operations leave the previous review readable.

### 3. Resolve Git endpoints and paths once

Addresses B04-B06, B08, and B27, centred on `crates/core/src/git_source.rs` and the existing `DiffSource` interface.

Replace independent base-string interpretation with one resolved comparison model. Resolve object endpoints for a refresh and use them for listings, preflight, patches, snippet reads, and gap content. Do not run unrelated resolution logic in those operations.

| Requested comparison | Old endpoint | New endpoint |
| --- | --- | --- |
| Default unstaged | Index | Worktree |
| Staged, no base | HEAD, or empty tree for unborn HEAD | Index |
| Base ref | Resolved base | Worktree |
| Base ref with staged | Resolved base | Index |
| `A..B` | A | B |
| `A...B` | Merge base of A and B | B |

Omitted range endpoints mean HEAD. Invalid refs, unresolved merge bases, and staged/range combinations unsupported by Git return explicit source errors. Never silently fall back to an unrelated endpoint. These comparison meanings follow [Git's diff documentation](https://git-scm.com/docs/git-diff).

Keep Git commands rooted at the review directory, as the repository requires. Resolve its repository prefix once. Request review-relative listings scoped to that directory with `--relative`, and read objects through `ls-tree` / `ls-files --stage` followed by `cat-file blob` (literal pathspecs, no prefix translation) instead of `git show REF:path`. A rename crossing the review-root boundary appears as an addition or deletion within the review, rather than exposing an ancestor path.

Apply literal pathspec semantics to all path-bearing Git operations, retain NUL-delimited path records, forced rename detection, locale control, and disabled external diff. Disable text conversion where raw Git content is required. Paths containing spaces, tabs, newlines, brackets, wildcard characters, colons, and leading dashes must address exactly one intended file. For non-UTF-8 path bytes, return a visible unsupported-path diagnostic instead of lossy conversion that could identify another file; preserving the current JSON string interface takes precedence over inventing a new path encoding in this repair.

Add one native review-path reader used by `getSrc`, untracked-file handling, and snippet capture. Reject absolute/parent-escaping paths, traverse relative to an opened review directory without following escaping symlinks, and read symlink entries as their link text to match Git. Avoid canonicalise-then-open checks that leave a replacement race. Reading a file that is not in the changed set remains allowed for a valid in-root comment path; browser gap requests must additionally reference a current file/gap. Old-side snippets for a selected rename resolve its recorded origin.

Keep the existing 10,000-changed-line, 2 MiB untracked-file, and highlight limits. Add a shared 8 MiB raw-patch/content-response budget and a ten-second Git-command deadline, with explicit oversized/timeout results. Stream line counting and signature hashing so bounded-memory checks do not allocate an entire patch first. Hash untracked content, not just timestamps, and include endpoint identity in the source generation. Check highlight input size before assembling duplicate pseudo-file buffers. Larger source files may be streamed for a bounded snippet or gap slice; avoid fetching a whole file when only a range is needed.

Acceptance: stage one version and leave another in the worktree; diverge branches for a true merge-base comparison; exercise unborn HEAD, omitted endpoints, renamed files, special pathspec characters, subdirectory review roots, symlinks, and deleted files. Rows, counts, snippets, and expanded context must agree on endpoints. A content-preserving touch must not refresh; same-size content changes with restored timestamps must refresh. Large tracked files must not bypass the memory budget through polling.

### 4. Own projections and expansion in the pure core

Addresses S1, S6, B10, B15-B16, B18, and B24. Extend the existing view/row/anchor/tree modules instead of reproducing their behaviour in a new hierarchy.

Introduce a pure `ReviewProjection` over the complete review and complete changed-file list. It owns placements, per-file counts, overview/unattached groups, and filtered tree output. Filtering is a view option and never changes the canonical changed-file set. Compute path/rename lookups once per projection and reuse them for comments and tallies.

Introduce a pure per-file `ViewState` that owns the parsed diff, view options, expanded context, and the derived rows. It can return a `FileProjection` containing the view and anchored comment records, and perform search over those same rows. Expansion uses the existing `GapInfo` and `build_expansion_rows`; its result updates rows, anchors, and search together. Theme/layout/word-diff changes rederive from retained input and expansion state. A changed source generation invalidates stale gap content; reload valid expansions from that generation rather than attaching old slices to new hunks.

Expose the same operations through the WASM wrapper. Keep existing exports available while adding a resulting-view accessor/expansion result for the browser migration. Native stdio `expand` retains its `{rows}` result and may add full view/anchor data; `view` remains compatible with the existing plugin's fetch-and-splice flow. Test that flow explicitly, including repeated expansion and intervening review changes.

Represent text, binary, empty/mode-only, and oversized files through one projection path. A line comment on a view with no code rows must render in its file comment group, rather than receive an unusable row-zero anchor. Preserve all placed comments. Serialise view discriminators and counts explicitly so each key appears once; oversized counts come from preflight, not empty hunks.

Add a core anchor-target helper taking a row and active cell. In unified mode, removed lines select old-side coordinates and context/add lines select new-side coordinates. In split mode, a selected removed left cell selects old-side coordinates; context comments use new-side coordinates per the existing invariant. Keyboard-only selection defaults to the new/right cell where available, then the old/left cell. Frontends retain active-cell identity from hit testing.

Use wider intermediate arithmetic for hunk/gap endpoints and checked conversion into public line coordinates. Reject unrepresentable parsed extents with a typed parse failure and make builders safe for hand-constructed models too. Invalid synthetic gaps must not produce wrapped coordinates or allocate by an unbounded declared length.

Acceptance: native and WASM produce equivalent projections for full/filtered trees, rename/unattached placement, all placeholder kinds, Unicode search, and expansion before/after option changes. Test line-coordinate extrema across parse, rows, view, expansion, and anchor construction, not just parser totality.

### 5. Make refresh scheduling deterministic

Addresses B12-B13 and the notification portion of B21.

Extract debounce/deadline decisions into a small pure scheduler with injected time and dirty events. Track review and diff check deadlines independently. Unrelated partial checks cannot reset the other deadline. Retain current quiet/max debounce and normal/degraded poll defaults, and keep file-content signatures authoritative.

The native application module performs startup as watch/baseline establishment followed by the held snapshot load. If source configuration changes during initialisation or refresh, establish the new comparison/watch and reload before exposing that generation. Review-source changes update both the data readers and the broadcast signature source. Load failures retain the last valid snapshot and produce diagnostics; a transient malformed review must recover after a valid replacement.

Use bounded/coalesced refresh hints rather than accumulating identical snapshots in queues. Join/stop background work on connection/session shutdown, and propagate source/signature failures instead of folding them into a zero hash that resembles a valid empty state.

Acceptance: deterministic tests cover a write in the original startup window, continuous unrelated events, missed events, degraded operation, source changes, and stop/drop. Keep a small real-notify integration suite for platform wiring. Do not fix notification tests by increasing every sleep or loosening eventual-consistency assertions.

### 6. Route native operations through one application module

Addresses S3-S5, B07, B17, B19-B23, B26, and H1-H2. Own CLI/stdio/web orchestration together in this task so shared files and transport shapes are not edited by competing workers.

Implement the established native application interface using the repaired store/source/projections. It owns load/refresh, source selection, automatic snippets, author/ID/time injection, review commands, and warning propagation. Keep filesystem/process dependencies out of pure domain methods. CLI, stdio, TUI, and web call this module for the same operation. Domain edit/delete methods replace direct comment-vector surgery.

Deserialize requests into the foundation's typed payloads. Validate integral coordinates without truncation, validate explicit enums, and map domain failures into each transport's existing error envelope. Keep valid older requests and omitted defaults working. Extend stdio and browser dispatch with `comment.edit` and `comment.delete`, preserving existing ID conventions. Add a browser `refresh` request returning a complete current snapshot. Keep protocol version 1 and all existing result fields.

Make setup reads fail on errors other than absence. Update managed blocks atomically while preserving unrelated UTF-8 content. Read and update Git excludes as bytes and preserve every existing rule. Reject unsafe symlink destinations and malformed managed-block delimiters with an actionable error. Preflight all setup targets before any write to minimise partial setup, and report the exact target on failure. `--check` remains read-only. Do not hide exclusion-write failure during init; report the created review plus the warning accurately so the user can correct exclusion before committing.

Apply terminal sanitisation at human-output boundaries to each line of comments, snippets, names, paths, warnings, and errors. Leave stored/JSON text intact. Do not send untrusted metadata through an unsanitised formatting branch.

For the browser server, use tungstenite's complete handshake machinery for the exact `/ws` route rather than a substring-based manual upgrade. Accept only the bound loopback host; browser Origins must match this server. Non-browser clients without Origin may authenticate normally. Require a cryptographically random token and return an error if entropy acquisition fails.

Keep the synchronous architecture, but cap concurrent connections at 32, set a five-second total handshake/authentication deadline, limit incoming command messages to 1 MiB, and set a five-second socket write deadline. The 8 MiB source budget sits below a 16 MiB outgoing message budget; oversized reviews/messages get a visible error without overwriting the underlying review. Coalesce pending review/diff notifications per subscriber and service them between requests as well as on idle timeouts. Stdio gets a 1 MiB request-line limit and explicit broken-output handling. Retain the original review file regardless of transport limits.

Acceptance: same valid/invalid command cases across CLI, stdio, and websocket; add/edit/delete/lifecycle/revision with preserved unknown fields; denied path traversal with innocuous external fixture files; failed setup reads with byte-identical original files; valid/invalid authentication, wrong host/origin, slow handshake, oversized input, slow subscriber, and clean shutdown. Source errors remain distinguishable from an empty changed set.

### 7. Repair TUI identity, saving, and geometry

Addresses B11, B18-B19, B25-B26 after the shared application interface is implemented.

Replace file indices in selected targets with stable review-relative paths. On refresh, restore the selected path, then a rename mapping when available; if it disappeared, select the overview and explain the transition. Restore row key, semantic line/cell, and scroll offset in that order. A stale selection must never silently become another file.

Adopt the shared projections and application commands. Keep editor buffers and selection until a mutation succeeds; on failure, keep the draft open with an actionable message. A successful acknowledgement is the only path to “comment added”. Read-only reviews disable mutation entrypoints and retain any already composed draft for copying/retry after recovery.

Compute pane rectangles, visible tree offset, gutter positions, and split halves once per layout. Paint and mouse handling use this same result. Reflow wrapped display rows when terminal size or pane layout changes while preserving the semantic cursor. Record the clicked split cell and use the core anchor-target helper. Audit metadata rendering for terminal controls.

Acceptance: PTY journeys add/remove an earlier-sorting file while a later file is selected; compose through a lock/read-only/write failure and retry; comment on the left side of a paired row; click a scrolled tree; resize through hidden-tree and wrapped layouts. Use the existing escape-stripped PTY helpers and semantic assertions, plus focused ratatui buffer checks for layout.

### 8. Replace browser ad hoc state with typed transport and view updates

Addresses S4, B09-B10, B14-B23, and browser portions of H3.

Split the browser into a small entrypoint plus transport, typed WASM access, view state, rendering, and editor modules. DOM/layout stays in TypeScript; diff/review semantics come from the core. Add strict TypeScript checking with discriminated request/response/view/row types and an `unknown`-to-validated-value ingress. Keep explicit TypeScript interfaces in `web/src/protocol.ts`; exercise their decoders against contract fixtures produced and verified by Rust tests. This repair needs no additional type-generation framework. Remove `Json = any` from application boundaries.

Use monotonically increasing request IDs, a pending-request table with ten-second timeouts, explicit error rejection, and a navigation/source generation guard. Ignore obsolete navigation responses even if the requested path matches. Close rejects all pending requests and disables writes. Reconnection is explicit via a reconnect action that authenticates and reloads a full snapshot; do not automatically replay writes whose acknowledgement was lost.

Consume one projection for the canonical changed set. Recompute tree/comment/overview membership on both review and diff refreshes. When a selected file vanishes, switch to its appropriate overview/unattached presentation. Manual refresh fetches a full server snapshot. Display persistent read-only, salvage, source, and connection diagnostics while keeping the last valid view.

Compose text spans using UTF-8 byte offsets bounded by the encoded byte length, validating boundaries before decoding. Cache the encoder/decoder and avoid recomputing whole-file gutter measurements for every visible row. Preserve exact DOM text for accents, CJK, combining sequences, emoji, tabs, and HTML metacharacters. Paint expansion results directly from the updated WASM view, including recalculated comments/search; do not reload raw input as an expansion step.

Implement all five missing advertised browser commands: focus switching, wrapping, line-number visibility, editing, and deletion. Deletion requires the existing style of confirmation; editing uses the same acknowledgement-preserving draft flow as addition. Empty optional address responses submit; empty required comment bodies do not. Keep active split-cell identity for commenting.

Wrapping cannot be implemented by changing `white-space` while retaining fixed 20-pixel virtual-row offsets. Keep fixed-height virtual rows for unwrapped display. In wrapped mode, use measured DOM row heights with a prefix-height index and `ResizeObserver`; render a bounded viewport window, and preserve the semantic cursor during width changes. This is presentation logic; row numbers and anchors remain core-derived.

Acceptance: extend real-browser journeys for Unicode text/highlight/search; expansion and comments within gaps; oversized/binary/mode-only comments; annotated/unreviewed changes; source changes; delayed same-path and cross-path replies; vanished files; full refresh; all advertised commands; read-only/disconnected/failed-save draft retention; optional responses; narrow layouts and wrapping. Add focused state tests for ordering and pending-request completion, then retain a few broad end-to-end journeys.

### 9. Make verification portable and generated assets accountable

Addresses S6/H3 and guards all repairs.

Add `scripts/check.sh` as the documented local gate: prerequisite checks; `cargo fmt --all -- --check`; strict Clippy; workspace tests including doc tests; strict TypeScript checking; native/WASM conformance; browser journeys; and committed-asset verification. Provide an explicit optional sibling-Neovim leg when its checkout is available, always selecting this repository's built binary. Missing prerequisites fail with installation guidance rather than silently skipping a required leg.

Resolve the active Rust toolchain through rustup/configuration, with an explicit override for the known Homebrew compiler mismatch. Remove the machine-specific Apple Silicon path from portable scripts. Keep the LLVM library-path workaround conditional on the environment that needs it. Check the wasm-bindgen CLI against the lockfile version. Add TypeScript as a locked development dependency and use frozen Bun installs.

Replace fixture-source `touch` with explicit Cargo rebuild dependencies for embedded fixtures. Extend semantic conformance to shared projections and expansion, and keep human-reviewed expected values. A generated golden must not be accepted just because both targets reproduce it.

Build browser assets into an isolated temporary directory and compare them with committed `web/dist`. The check must not rewrite source-controlled output. The ordinary regeneration command remains the only way to update committed assets and is run after core/WASM/browser changes are integrated. Check the WASM feature without native dependencies and execute wrapper/projection tests, not only the parser corpus.

Improve test harnesses to capture child stderr and early exit, fail promptly when a server cannot bind/start, clean up their own temporary resources, and use readiness signals instead of fixed sleeps. Add local Linux/macOS platform coverage instructions for locking, notification, and Git behaviour; hosted CI remains deferred.

Acceptance: a fresh supported checkout can run the documented gates with explicit prerequisites; stale committed assets or mismatched wire contracts fail without modifying the tree; a server startup failure reports its real cause; the full repaired suite passes with no warnings or unexplained flaky reruns.

## Files and ownership

Use the following ownership boundaries for parallel implementation. Files may pass to a later-wave task, but tasks in the same wave must not edit the same files. The section references contain the implementation and acceptance requirements.

| Task | Wave | Owned files and responsibilities | Plan sections |
| --- | --- | --- | --- |
| A: Contracts | 0 | Workspace/crate Cargo manifests and lockfile; new core contract types and tests; `fixtures/contracts`; initial TypeScript protocol interfaces; required module declarations. | 1 |
| B: Review integrity and watching | 1 | Core review, lifecycle, store, system helpers, and watch modules; store-race/watch tests and new review-integrity tests. | 2, 5 |
| C: Git source and root-relative IO | 1 | Core Git source and source interface; private native path-reader module; Git integration/source-limit tests. Own Git-exclude byte preservation and new sidecar exclusions. | 3 |
| D: Pure projections | 1 | Core model, parser, rows, view, anchors, tree, search, highlighting, new projection/view-state modules, WASM wrapper, and core exports. Own new projection/boundary tests and `fixtures/projections`; leave existing conformance harness wiring to F. | 4 |
| E: Native application and transports | 2 | New CLI application module; context, verbs, args, main, stdio engine and web server; CLI/engine/web protocol tests. Own all source-to-session integration and setup error propagation, including the AGENTS.md / CLAUDE.md / skill halves of B07 (the Git-exclude half is task C). | 5, 6 |
| F: Local verification | 2 | Build/check scripts; core build script and existing conformance harness; web package/lockfile, TypeScript and Playwright configuration. Browser journey bodies stay with H. | 9 |
| G: TUI migration | 3 | TUI modules and PTY tests. After migrating the final native caller, own removal of superseded compatibility wrappers in core review/source/Git/watch modules and corresponding test call-site updates. | 7 |
| H: Browser migration | 3 | Browser entrypoint, protocol/transport/core-access/state/render/editor modules, CSS/HTML, focused browser state tests and E2E journey/helper files. Do not edit core/native interfaces or manifests owned by earlier tasks. | 8 |

All implementations include the assigned regression tests. A single documentation owner updates README, CLAUDE.md, the agent contract, and this finding ledger after each integrated wave. Run the asset-generation script after each integration affected by core/WASM/browser changes, before its interaction review. Workers never hand-edit generated browser output.

New modules are limited to meaningful seams: typed contracts, native application operations, pure review projection/view state, safe root-relative IO, and separated browser responsibilities. Internal helpers stay private unless another real caller needs them. Tests exercise these interfaces and their observable outputs rather than mirroring each helper's implementation.

## Verification and completion criteria

Every finding in the report has a corresponding numbered repair section and acceptance scenario. The ownership table assigns those sections to implementation tasks. The final review checks both standards and behavioural requirements, verifies no supported wire/file contract changed accidentally, and checks cross-task interactions rather than merely rerunning unit tests.

Completion requires: data-loss/incorrect-file regressions passing; all advertised in-repository commands implemented; failed mutations preserving drafts and on-disk bytes; consistent source endpoints and shared projections; fresh embedded assets; native, WASM, browser, and available sibling compatibility journeys passing. Timing assertions must have deterministic scheduler coverage and limited real-platform tests. A percentage coverage target is not a substitute for these scenarios.

Inspect representative terminal/browser layouts in unified/split and dark/light modes, with comments, Unicode, wrapping, narrow windows, and empty/oversized content. Run all required gates after integration once; repeat only for a failure, a subsequent change, or an unresolved concern.

No unsupported-platform release, plugin rewrite, public schema redesign, CI installation, or unrelated feature expansion is part of completion. Any newly discovered defect should be assessed and added to the ledger with evidence; do not claim the review proves the absence of all possible issues.

## Execution

Execute the eight tasks directly from this document, sequentially or with at most three parallel implementation workers:

1. Complete A and review its contracts/dependencies before beginning the core changes.
2. Run B, C, and D independently. Integrate all three, regenerate affected assets, and review their interactions before starting E or F. In particular, check new source error types, watch inputs, placeholder wire shapes, and feature guards together.
3. Run E and F independently. Integrate both and check native application behaviour, transport compatibility, and verification commands before starting either frontend migration.
4. Run G and H independently. Integrate both, regenerate embedded assets, and run the complete verification plan. G exclusively owns final internal-wrapper cleanup; H consumes the established WASM interface.
5. Conduct one independent adversarial review of the complete result after all documentation is current. Return concrete findings to their owning task; allow at most two review/fix cycles before explicitly reporting any unresolved issue. An unresolved issue is not completion.

Every integrated wave must be green. Documentation updates follow each successful interaction review and are serialised; they may run alongside the next implementation wave because their files are separate. The final review waits for every implementation and documentation update.

This plan is complete without Beads, a task database, or a machine-readable execution graph. No implementation has started. The proposed future integration base is the existing `main`, and execution begins only when implementation is requested.

Evidence: [repository-review-2026-09-07.md](repository-review-2026-09-07.md).
