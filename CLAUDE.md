# ambidiff

Git-backed diff review for humans and AI agents: one Rust core, three equal
frontends (terminal TUI, neovim via a stdio engine, browser via wasm), and a
file-based review representation (`.ambidiff.json`) that agents working in
the same tree read and write directly. The neovim plugin lives in the
sibling repo `../ambidiff-nvim`.

## Architecture: the drift firewall

Every derivation lives in `ambidiff-core` and runs identically native and
compiled to wasm32. Frontends ONLY paint rows and forward intents. If you
find yourself computing diff semantics (parsing, pairing, ranges, anchors,
tree shape, search) in `crates/cli/src/tui`, `web/src`, or the nvim plugin,
stop: that logic belongs in `crates/core`, where the conformance corpus and
the wasm parity test can hold it still.

```
            .ambidiff.json  (source of truth AND sync bus; lock sidecar)
                  ^  watched + read-modify-written by every engine, agents, CLI
    +-------------+------------------------------+
    |        ambidiff-core (one crate)           |
    |  review store | git source | diff model    |
    |  word-diff | rows | anchors | highlighting |
    |  lifecycle | tree | search | watch | cmds  |
    +----+----------------+----------------+-----+
         | native          | native         | wasm32 (same crate)
    ratatui TUI       engine --stdio    TS painter served by
    (in binary)       (nvim client)     `ambidiff web` (loopback)
```

- One binary (`ambidiff`), no daemon: each frontend runs its own engine
  instance; the review file is the sync bus. The watch controller
  (debounce, signature gate, degrade-to-poll) drives live convergence.
- The browser gets RAW diff bytes over the websocket and parses them with
  the same core in wasm; `GitSource::file_diff_raw` is the shared
  raw-bytes path, so native and browser parse identical input.
- Keybindings come from one command table (`core/src/commands.rs`) with
  per-frontend chords; the TUI dispatches from it, the engine serves it,
  the web page embeds it. Add commands there, never inline.

## Crate and directory map

| Path | What lives there |
| --- | --- |
| `crates/core/src/model.rs` | Diff vocabulary: `DiffLine`, `Hunk`, `FileDiff`, `FileEntry` |
| `crates/core/src/parser.rs` | Unified-diff parser (tolerant, never panics; overlong lines truncate) |
| `crates/core/src/worddiff.rs` | Intra-line LCS port from revdiff: tokenize, 30% gate, 500-byte cap, greedy pairing |
| `crates/core/src/rows.rs` | Split/unified row builder, gap addressing (`before:<hunk>` / `trailing`), expansion rows |
| `crates/core/src/highlight.rs` | syntect (fancy-regex, wasm-clean) over pseudo-file reconstruction; RGB spans out |
| `crates/core/src/review.rs` | The `.ambidiff.json` schema, salvage parsing with retention, checked domain operations (pure) |
| `crates/core/src/protocol.rs` | Typed request payloads every transport decodes into (`ViewOptionsWire`, comment/lifecycle/view/expand requests) |
| `crates/core/src/projection.rs` | `ReviewProjection`: placements, tallies, filtered tree, overview, derived once from the whole review + listing |
| `crates/core/src/view_state.rs` | `ViewState`: one file's parsed diff, options, expansions, rows, anchored comments, search |
| `crates/core/src/lifecycle.rs` | The verify-loop state machine (resolve/reopen are HUMAN ONLY) |
| `crates/core/src/anchor.rs` | Placement (rename-aware, unattached group) and row anchoring (outdated/clamped) |
| `crates/core/src/store.rs` | Native IO: guard lock + ownership sidecar, atomic exclusive-temp+rename writes, `mutate()` |
| `crates/core/src/git_source.rs` | Git shell-outs: resolved comparison, literal `-z` pipelines, forced `-M`, streamed budgets, content signatures |
| `crates/core/src/rootio.rs` | Root-relative reads (openat walk, no escaping symlinks, links read as link text) |
| `crates/core/src/watch.rs` | Watch controller (notify + debounce + signature gate + safety poll) |
| `crates/core/src/wasm.rs` | wasm-bindgen exports for the browser painter (feature `wasm`) |
| `crates/cli/src/application.rs` | The native application: snapshot load/refresh, source reconfiguration, watch, file views, review commands; every transport calls it |
| `crates/cli/src/verbs.rs` | CLI verbs: the agent surface; `--json` twins; status exits 10 on to-dos |
| `crates/cli/src/tui/` | ratatui frontend (app state, render, editor widget, themes) |
| `crates/cli/src/engine.rs` | NDJSON stdio engine for editor plugins (protocol v1) |
| `crates/cli/src/web.rs` | Loopback HTTP + websocket server, embedded `web/dist` assets |
| `web/src/*.ts` | Browser painter: `protocol` (typed wire), `transport`, `core` (typed wasm), `state` (single read model), `render`, `editor`, `commands`, `paint`; `main.ts` only boots |
| `fixtures/cases/` | Conformance corpus: hand-written golden expectations |
| `fixtures/projections/` | Projection corpus (filtered tree, placements, placeholders, search, expansion) run natively and under wasm |
| `fixtures/contracts/` | Wire contract fixtures every transport and the browser decoders are held to |
| `skills/ambidiff-review/SKILL.md` | Claude Code skill, embedded into the binary for `agent-setup --claude-skill` |
| `docs/agents.md` | The full agent interoperability contract |

## Non-negotiable invariants

- `crates/core` must stay wasm32-clean: fs/process/notify code sits behind
  the `native` feature only. Check with the wasm build before assuming.
- Review-file semantics: reads are salvage-mode (quarantine bad records,
  warn, never brick, never drop data; an unrepresentable top-level value is
  retained and makes the file read-only); writes are strict-validated,
  locked, and atomic (exclusive temp + rename). The lock is the persistent
  `.ambidiff.json.guard` advisory lock held for the whole read-modify-write,
  with `.ambidiff.json.lock` as the ownership sidecar created under it;
  nothing is ever broken by age. `.ambidiff.json` is never git-tracked;
  init adds it and the sidecars to `.git/info/exclude`.
- Comparison model: a git source resolves its two endpoints once (`Comparison`
  of `Endpoint::{Commit, EmptyTree, Index, Worktree}`) and every listing,
  patch, snippet, and gap read uses those endpoints; never re-derive them
  per operation. Reads of review-root files go through `rootio` (root
  scoped, no escaping symlinks).
- Lifecycle: open -> addressed (agent) -> resolved | reopened (HUMAN ONLY).
  Transitions are id-keyed; renames never gate lifecycle.
- Side rule everywhere: removed lines anchor old-side numbers; added and
  context lines anchor new-side.
- Revision rule: when no comment is open/reopened, the next comment or
  reopen bumps `revision`; `comment.rev` is origin provenance, never mutated.
- Watch baselines are computed synchronously in
  `WatchController::start_checked` on the caller's thread; consumers arm
  the watch BEFORE their first held load (there was a real absorbed-write
  race here). `Application::start_watch` then `Application::load` is the
  only order; a source change reconfigures the source and the watch
  signature together and bumps the generation.
- Wire compatibility: bump `PROTOCOL_VERSION` in `engine.rs` on breaking
  stdio changes (the nvim plugin warns on mismatch) and the `ambidiff`
  schema major in `review.rs` on breaking file changes (older builds open
  newer files read-only).
- Unicode status symbols in output, no emoji.
- All git plumbing goes through `git_source.rs` conventions: `cwd = root`,
  `LC_ALL=C`, `-c core.quotePath=true --no-color --no-ext-diff`, paths from
  `-z` listings only (never parsed out of patch headers), forced `-M`.

## Build and test

`scripts/check.sh` is the gate; every change lands with it green. It
resolves the active rustup toolchain itself (`scripts/lib.sh`; the rustup
proxies do not reliably beat Homebrew's rust on this machine, and
`AMBIDIFF_TOOLCHAIN_BIN` overrides), checks every prerequisite with an
install hint, and never writes into the tree.

```sh
scripts/check.sh               # fmt, clippy -D warnings, workspace tests, wasm feature build,
                               # wasm lib/conformance/projection tests, typecheck + bun tests,
                               # committed-asset freshness, playwright journeys, nvim journey
scripts/check.sh --fast        # everything except the browser and nvim journeys
scripts/test-wasm.sh           # just the wasm32 leg
scripts/build-web.sh           # regenerate web/dist (committed!) after web/ or wasm-facing core changes
cargo llvm-cov --workspace     # coverage as a DISCOVERY tool, never a target
```

For manual cargo commands, source the same toolchain first:
`. scripts/lib.sh` (or export the toolchain bin dir it computes). Fixture
embeds for the wasm tests stay fresh through `crates/core/build.rs`
(`rerun-if-changed` on the fixture directories); no script touches
sources. There is deliberately no CI yet.

### Testing philosophy (Aniche-style pyramid)

- Bulk: fast unit tests on core domain logic, specification-based
  partitions and boundary analysis. Domain logic needs neither git nor
  disk: `DiffSource` is a trait, clock and id-generator are injected.
- Integration tests at every infrastructure boundary against the real
  thing: scratch git repos (pin `core.autocrlf false` in them or a
  developer's global config skews content assertions), real fs locking
  races (the test binary re-invokes itself as a second process), real
  notify events.
- Few journey-scoped E2E per frontend: spawned binary for the CLI,
  expectrl PTY for the TUI (match on escape-stripped whitespace-normalized
  streams; ratatui cell-diffing splits words across cursor moves),
  playwright for the browser, headless nvim for the plugin.
- Conformance corpus (`fixtures/cases/`): golden expectations are written
  FROM SEMANTICS, by hand, so a buggy primitive cannot self-validate.
  Regenerate with `UPDATE_FIXTURES=1 cargo test --test conformance`, then
  review the diff against intended semantics before committing. Native
  reads fixtures from disk; wasm embeds them (`build.rs` declares the
  fixture directories as rebuild inputs so the embed cannot go stale).
- Wire contracts (`fixtures/contracts/`): the transports and the browser
  decoders are asserted against the same hand-written JSON; a wire change
  edits the fixture first.

## Environment gotchas (hard-won)

- `scripts/lib.sh` sets `DYLD_FALLBACK_LIBRARY_PATH` on macOS because
  rustup's `rust-lld` cannot find `libLLVM.dylib` on this install.
- wasm builds use cargo + `wasm-bindgen` CLI directly, NOT wasm-pack
  (wasm-pack does not forward `--no-default-features`). The installed
  `wasm-bindgen-cli` version must equal the `wasm-bindgen` in `Cargo.lock`.
- `web/dist` (including the ~2.4MB wasm) is committed so a plain
  `cargo build` produces the full binary; `.gitattributes` marks
  `fixtures/cases/**` as `-text` to protect the CRLF fixture from eol
  normalization. `scripts/check.sh` fails when `web/dist` is stale.
- macOS FSEvents reports canonicalized paths (`/private/var` vs `/var`);
  the watch controller classifies events by file NAME, not full path.
- git's `-M` rename detection needs >= 50% similarity: in tests, use files
  big enough that an edit plus rename stays above it, or you will get
  delete+add (which is itself the anchor resolver's "unattached" case).
- `vim.json.decode` maps JSON null to truthy `vim.NIL`; the nvim plugin
  routes every nullable engine field through its `denil` helper.
