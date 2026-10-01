# ir

Workspace package manager (Rust): recursively resolve repository dependencies
declared in `.repos` (vcstool format) or `.rosinstall` files, pin versions in a
lockfile, and fetch / place / version-sync every repository into a workspace —
uv-style, but for source workspaces instead of Python packages.
(The name is `uv` inverted: packages in, repos out — or the other way round.)

## Status: Phase 3 complete

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
- [ ] Phase 4 — Hg / Svn / Bzr providers
- [ ] Phase 5 — `status`, `export`, `update`, `add`, `prune`, JSON output,
      shell completions
- [ ] Phase 6 — retries, shallow-fetch tuning, docs

## Usage (Phase 3)

```sh
cargo build
./target/debug/ir -C /path/to/workspace resolve   # recursive resolve -> ir.lock
./target/debug/ir -C /path/to/workspace sync       # parallel sync from ir.lock
./target/debug/ir -C /path/to/workspace sync --jobs 8 --force
cargo test     # 24 tests: manifest, providers, resolver, lockfile, sync
```

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
- `src/providers/archive.rs` — `TarProvider` / `ZipProvider` (+ tests)
- `src/main.rs` — `ir` CLI
