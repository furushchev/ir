# ir

Workspace package manager (Rust): recursively resolve repository dependencies
declared in `.repos` (vcstool format) or `.rosinstall` files, pin versions in a
lockfile, and fetch / place / version-sync every repository into a workspace —
uv-style, but for source workspaces instead of Python packages.
(The name is `uv` inverted: packages in, repos out — or the other way round.)

## Status: Phase 4 complete

- [x] Phase 0 — CLI skeleton + `.repos` / `.rosinstall` parsers + normalization
      (`RepoSpec { path, kind, url, version }`, path-traversal rejection,
      URL normalization for cache keys)
- [x] Phase 1 — `Provider` trait (all VCS via `std::process`), `GitProvider`
      (bare-mirror cache, uniform ref layout, default/ref/hash resolution,
      detached materialize, sparse subpaths, dirty detection),
      `TarProvider`/`ZipProvider` (download, sha256 cache, Zip-Slip-safe
      extraction, `.ir-archive` marker files)
- [x] Phase 2 — recursive resolver + `ir.lock` (TOML, exact pins) +
      cycle / path / version conflict detection
- [x] Phase 3 — parallel sync engine (rayon, per-URL cache locks) +
      `indicatif` progress UI (overall bar + per-repo spinners),
      `sync --jobs N --force`, up-to-date / skip / force-replace semantics
- [x] Phase 4 — `HgProvider` / `SvnProvider` / `BzrProvider` (all native CLI
      via `std::process`); numeric versions classify per source kind
      (hg/svn/bzr revision vs git ref); sync is nested-repo aware
      (changes under a managed nested repo don't dirty the parent;
      parents materialize before children)
- [ ] Phase 5 — `status`, `export`, `update`, `add`, `prune`, JSON output,
      shell completions
- [ ] Phase 6 — retries, shallow-fetch tuning, docs

## Usage (Phase 4)

```sh
cargo build
./target/debug/ir -C /path/to/workspace resolve   # recursive resolve -> ir.lock
./target/debug/ir -C /path/to/workspace sync       # parallel sync from ir.lock
./target/debug/ir -C /path/to/workspace sync --jobs 8 --force
cargo test     # 34 tests: manifest, providers, resolver, lockfile, sync
```

## Providers

| Kind | Provider | Notes |
| ---- | -------- | ----- |
| `git` | `GitProvider` | bare-mirror cache; branch/tag/full or unique-prefix hash |
| `hg` | `HgProvider` | `hg clone --noupdate` cache; bookmark/tag/full or unique-prefix node |
| `svn` | `SvnProvider` | no local mirror (remote URL cached); numeric revisions incl. `r` prefix; branches/tags belong in the URL |
| `bzr` | `BzrProvider` | `bzr branch` cache; uses `bzr` or `brz` (Breezy) whichever is installed; revnos, numeric revs, tags |
| `tar`/`zip` | `TarProvider` / `ZipProvider` | sha256-verified download cache |

If a libgit2-based implementation is ever added, the git providers split
into `GitCliProvider` / `GitLibProvider`; until then the role-based
`GitProvider` name stands.

## Layout

- `src/lib.rs` — `ir_core` library root
- `src/manifest.rs` — manifest parsing & normalization (+ unit tests)
- `src/error.rs` — error types
- `src/process.rs` — `std::process::Command` runner with timeouts
- `src/cache.rs` — cache root, bare git mirrors, archive downloads
- `src/provider.rs` — `Provider` trait + `provider_for` dispatch
- `src/resolve.rs` — recursive `Resolver` (cycle / conflict detection)
- `src/lock.rs` — `ir.lock` read/write (TOML)
- `src/providers/git.rs` — `GitProvider` (+ integration tests)
- `src/providers/hg.rs` — `HgProvider` (+ integration tests)
- `src/providers/svn.rs` — `SvnProvider` (+ integration tests)
- `src/providers/bzr.rs` — `BzrProvider` (+ integration tests)
- `src/providers/archive.rs` — `TarProvider` / `ZipProvider` (+ tests)
- `src/main.rs` — `ir` CLI
