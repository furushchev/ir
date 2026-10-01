# wspm

Workspace package manager (Rust): recursively resolve repository dependencies
declared in `.repos` (vcstool format) or `.rosinstall` files, pin versions in a
lockfile, and fetch / place / version-sync every repository into a workspace —
uv-style, but for source workspaces instead of Python packages.

## Status: Phase 0 complete

- [x] Phase 0 — CLI skeleton + `.repos` / `.rosinstall` parsers + normalization
      (`RepoSpec { path, kind, url, version }`, path-traversal rejection,
      URL normalization for cache keys)
- [ ] Phase 1 — `Provider` trait (all VCS via `std::process`), `GitCli`,
      `Tar`/`Zip` providers (download, extract, marker files)
- [ ] Phase 2 — recursive resolver + `wspm.lock` (TOML, sha256 pinning) +
      conflict / cycle detection
- [ ] Phase 3 — cache layer + parallel sync + `indicatif` progress UI
- [ ] Phase 4 — Hg / Svn / Bzr providers
- [ ] Phase 5 — `status`, `export`, `update`, `add`, `prune`, JSON output,
      shell completions
- [ ] Phase 6 — retries, shallow-fetch tuning, docs

## Usage (Phase 0)

```sh
cargo build
./target/debug/wspm -C /path/to/workspace resolve   # parse + print normalized deps
./target/debug/wspm --manifest /path/to/.rosinstall resolve
cargo test
```

## Layout

- `src/lib.rs` — `wspm_core` library root
- `src/manifest.rs` — manifest parsing & normalization (+ unit tests)
- `src/error.rs` — error types
- `src/main.rs` — `wspm` CLI
