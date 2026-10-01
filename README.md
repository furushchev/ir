# ir

Workspace package manager (Rust): recursively resolve repository dependencies
declared in `.repos` (vcstool format) or `.rosinstall` files, pin versions in a
lockfile, and fetch / place / version-sync every repository into a workspace —
uv-style, but for source workspaces instead of Python packages.

## Status: Phase 1 complete

- [x] Phase 0 — CLI skeleton + `.repos` / `.rosinstall` parsers + normalization
      (`RepoSpec { path, kind, url, version }`, path-traversal rejection,
      URL normalization for cache keys)
- [x] Phase 1 — `Provider` trait (all VCS via `std::process`), `GitProvider`
      (bare-mirror cache, `init --bare` + `remote add` for uniform ref layout,
      resolve default/ref/hash, detached materialize, sparse subpaths,
      dirty detection), `TarProvider`/`ZipProvider` (download, sha256 cache,
      safe extraction with Zip-Slip rejection, `.ir-archive` marker files)
- [ ] Phase 2 — recursive resolver + `ir.lock` (TOML, sha256 pinning) +
      conflict / cycle detection
- [ ] Phase 3 — cache layer + parallel sync + `indicatif` progress UI
- [ ] Phase 4 — Hg / Svn / Bzr providers
- [ ] Phase 5 — `status`, `export`, `update`, `add`, `prune`, JSON output,
      shell completions
- [ ] Phase 6 — retries, shallow-fetch tuning, docs

## Usage (Phase 1)

```sh
cargo build
./target/debug/ir -C /path/to/workspace resolve   # parse + print normalized deps
./target/debug/ir --manifest /path/to/.rosinstall resolve
cargo test     # 13 tests: manifest parsing + git/archive provider round-trips
```

## Layout

- `src/lib.rs` — `ir_core` library root
- `src/manifest.rs` — manifest parsing & normalization (+ unit tests)
- `src/error.rs` — error types
- `src/process.rs` — `std::process::Command` runner with timeouts
- `src/cache.rs` — cache root, bare git mirrors, archive downloads
- `src/provider.rs` — `Provider` trait + `provider_for` dispatch
- `src/providers/git.rs` — `GitProvider` (+ integration tests)
- `src/providers/archive.rs` — `TarProvider` / `ZipProvider` (+ tests)
- `src/main.rs` — `ir` CLI
