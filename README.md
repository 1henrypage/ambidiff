# ambidiff

Review agent-authored code the way you already work: in a git worktree, over
several rounds, with the agent fixing what you flag. ambidiff is a git-backed
diff viewer with three equal frontends (terminal, neovim, browser) and one
defining feature: the review itself lives in a file at the review root,
`.ambidiff.json`, which AI agents working in the same tree read and write
directly. Your comments are the agent's to-do list; its responses land back
in your viewer, live.

## The loop

```sh
cd my-worktree
ambidiff init --base main     # start a review of this tree against main
ambidiff agent-setup          # brief agents via AGENTS.md / CLAUDE.md
ambidiff                      # open the TUI and leave comments
```

Then hand the tree to an agent. The standing instructions tell it to run:

```sh
ambidiff status --json            # exit code 10: comments await you
ambidiff comment list --todo --json
# ...make each fix...
ambidiff comment addressed c-7f3a2b1c -m "now throws AuthError"
```

Back in any frontend you watch responses arrive live, then pass verdicts:
resolve what is fixed, reopen what is not. A reopened comment goes straight
back onto the agent's to-do list, and the pass counter (`revision`) tracks
how many rounds the review has taken. Repeat until nothing is open.

The same review follows you across frontends because every engine watches
the file: comment in the TUI, re-review in neovim, keep the browser open on
a second screen, and all three converge on each change.

## Frontends

- **Terminal**: `ambidiff`. File tree with annotated/unreviewed filters,
  unified and side-by-side layouts, syntax highlighting, intra-line
  word-diff, collapsed-gap expansion, in-diff search, comment cards with the
  full lifecycle, live reload. `?` shows every binding.
- **Neovim**: [ambidiff-nvim](../ambidiff-nvim), a thin client over
  `ambidiff engine --stdio`.
- **Browser**: `ambidiff web` serves a loopback-only page (no deployed
  server); the URL carries a capability token in the fragment. The page runs
  the same core compiled to WebAssembly, so what it computes is
  byte-identical to the terminal.

All three paint the same rows: every derivation (diff parsing, word-diff,
highlight spans, comment anchoring, search, the file tree) lives in
`ambidiff-core` and runs identically native and in wasm, enforced by a
conformance fixture corpus that both targets must reproduce exactly.

## The review file

`.ambidiff.json` is the source of truth and the sync bus. It is never
git-tracked (`ambidiff init` adds it to `.git/info/exclude`), and git is
only a pluggable diff source for viewing.

```json
{
  "ambidiff": 1,
  "review": "worktree-auth-fix",
  "revision": 2,
  "source": { "kind": "git", "base": "main" },
  "createdAt": "...", "updatedAt": "...",
  "comments": [{
    "id": "c-7f3a2b1c", "rev": 1, "status": "reopened",
    "path": "src/login.ts", "side": "new", "line": 42, "endLine": 45,
    "snippet": "if (user == null) return;",
    "body": "null check inverted?? explain",
    "response": "fixed: now throws AuthError",
    "author": "henry", "createdAt": "...", "updatedAt": "..."
  }]
}
```

- Anchor levels via nulls: `path: null` is a review-level comment,
  `line: null` a file-level one, otherwise a line or range. Removed lines
  anchor old-side numbers; added and context lines anchor new-side.
- `snippet` captures the source at comment time. When the code drifts, the
  comment shows an outdated badge but stays intelligible forever; when a
  file is renamed, the comment follows it with a "was" badge; when nothing
  matches any more, it renders in an unattached group, never silently
  dropped.
- `??` in a body marks a question: the agent answers in its response
  instead of changing code.
- Comment lifecycle: `open -> addressed -> resolved | reopened`. Addressing
  is the agent's move; resolving and reopening are human verdicts only.
- Revisions are lightweight passes, no snapshots: when every comment is
  addressed or resolved, the next comment (or reopen) starts pass N+1.
  `ambidiff rev bump` is the manual escape hatch.

Reads are salvage-mode: malformed records are quarantined with warnings,
never dropped; a top-level value the tool cannot represent faithfully is
retained and the file opens read-only rather than being rewritten lossily,
as does a file written by a newer ambidiff. Writes are atomic (exclusive
temp file + rename) under a lock held for the whole read-modify-write:
`.ambidiff.json.guard` is a persistent OS advisory lock that serialises
every writer, and `.ambidiff.json.lock` is the ownership sidecar created
under it. A lock is never broken by age; a crashed holder is reclaimed
under the guard, and a sidecar left by a pre-repair build only once its
process is provably gone. Close older ambidiff instances before upgrading,
and keep review roots on local filesystems (advisory locks do not travel
well over network mounts).

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | clean |
| 1 | error |
| 10 | `ambidiff status` only: actionable comments (open or reopened) exist |

Agent loops branch on 10 without parsing anything.

## Install

```sh
cargo install --path crates/cli   # from a checkout
```

Building needs stable Rust. The browser frontend's built assets are
committed under `web/dist`, so a plain `cargo build` produces the full
binary; regenerate them with `scripts/build-web.sh` (needs bun,
wasm-bindgen-cli, and the wasm32-unknown-unknown target).

## Development

```sh
scripts/check.sh          # the gate: fmt, clippy, every test leg, asset freshness
scripts/check.sh --fast   # the same without the browser and nvim journeys
cargo test                # unit, property, integration, CLI + PTY E2E
scripts/test-wasm.sh      # conformance + projection corpora under wasm32
cd web && bunx playwright test   # browser E2E
cargo llvm-cov --workspace       # coverage as a discovery tool
```

`scripts/check.sh` resolves the toolchain through rustup, checks every
prerequisite (wasm32 target, `wasm-bindgen-cli` at the locked version, bun,
node, git 2.31+) with an install hint, builds the browser assets into a
temporary directory and compares them with the committed `web/dist`, and
runs the sibling Neovim journey when `../ambidiff-nvim` is checked out.

The conformance corpus under `fixtures/cases/` carries hand-written golden
expectations; regenerate with `UPDATE_FIXTURES=1 cargo test --test
conformance`, then review the diff against intended semantics before
committing.

## License

MIT
