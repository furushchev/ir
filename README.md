# ir

Workspace package manager (Rust): recursively resolve repository dependencies
declared in `.repos` (vcstool format) or `.rosinstall` files, pin versions in a
lockfile, and fetch / place / version-sync every repository into a workspace --
uv-style, but for source workspaces instead of Python packages.
(The name is `uv` inverted: packages in, repos out -- or the other way round.)

## Status: Phase 6 complete

- [x] Phase 0 -- CLI skeleton + `.repos` / `.rosinstall` parsers + normalization
      (`RepoSpec { path, kind, url, version }`, path-traversal rejection,
      URL normalization for cache keys)
- [x] Phase 1 -- `Provider` trait (all VCS via `std::process`), `GitProvider`
      (bare-mirror cache, uniform ref layout, default/ref/hash resolution,
      detached materialize, sparse subpaths, dirty detection),
      `TarProvider`/`ZipProvider` (download, sha256 cache, Zip-Slip-safe
      extraction, `.ir-archive` marker files)
- [x] Phase 2 -- recursive resolver + `ir.lock` (TOML, exact pins) +
      cycle / path / version conflict detection
- [x] Phase 3 -- parallel sync engine (rayon, per-URL cache locks) +
      `indicatif` progress UI (overall bar + per-repo spinners),
      `sync --jobs N --force`, up-to-date / skip / force-replace semantics
- [x] Phase 4 -- `HgProvider` / `SvnProvider` / `BzrProvider` (all native CLI
      via `std::process`); numeric versions classify per source kind
      (hg/svn/bzr revision vs git ref); sync is nested-repo aware
      (changes under a managed nested repo don't dirty the parent;
      parents materialize before children)
- [x] Phase 5 -- `status` (nested-aware, vs `ir.lock`), `export` (vcstool
      `.repos`, `--exact` for pinned revisions), `update` (whole workspace
      or one subtree, with pin-change report), `add` / `init` / `prune`,
      `cache clean` / `cache gc`, `--format json` for resolve/sync/status/
      update, `completions` (bash/zsh/fish/powershell/elvish).
      Also: poisoned git mirrors (failed fetch / changed raw URL) are
      detected via `remote.origin.url` and rebuilt instead of failing forever
- [x] Phase 6 -- retries with exponential backoff for all network operations
      (`--retries` / `IR_RETRIES`), fetch throttling (`--fetch-interval` /
      `IR_FETCH_INTERVAL`), docs

## Usage

```sh
cargo build
./target/debug/ir -C /path/to/workspace resolve   # recursive resolve -> ir.lock
./target/debug/ir -C /path/to/workspace sync       # parallel sync from ir.lock
./target/debug/ir -C /path/to/workspace sync --jobs 8 --force
./target/debug/ir -C /path/to/workspace status     # vs ir.lock: up-to-date / modified / outdated / missing
./target/debug/ir -C /path/to/workspace update     # re-resolve, report pin changes
./target/debug/ir -C /path/to/workspace update some/repo  # one subtree only
./target/debug/ir -C /path/to/workspace export --exact -o deps.repos
./target/debug/ir -C /path/to/workspace add https://example.com/foo.git --version main
./target/debug/ir -C /path/to/workspace prune      # remove undeclared checkouts
./target/debug/ir -C /path/to/workspace cache gc   # drop unreferenced cache entries
./target/debug/ir --format json -C /path/to/workspace status
./target/debug/ir --retries 5 -C /path/to/workspace sync
./target/debug/ir --fetch-interval 10m -C /path/to/workspace resolve
./target/debug/ir completions bash >> ~/.bash_completion
cargo test     # 50 tests: manifest, providers, resolver, lockfile, sync, inspect, commands
```

## Command reference

Global flags (accepted before any subcommand):

- `-C, --dir <DIR>` -- run as if started in DIR
- `--manifest <FILE>` -- explicit manifest (overrides `.repos`/`.rosinstall` discovery)
- `--format <text|json>` -- output format for resolve/sync/status/update
- `--retries <N>` -- total attempts per network operation (default 3; see below)
- `--fetch-interval <DURATION>` -- skip fetches for recently-fetched mirrors
  (default `0` = always fetch; see below)

| Command | Description |
| ------- | ----------- |
| `init` | Create an empty `.repos` manifest in the workspace |
| `resolve` | Recursively resolve all dependencies, write `ir.lock` |
| `sync [--jobs N] [--force]` | Fetch, place and version-sync every repo from `ir.lock` |
| `update [PATH]` | Re-resolve and rewrite `ir.lock`, reporting pin changes; with PATH, only that subtree |
| `status` | Compare the workspace against `ir.lock` (network-free) |
| `export [--exact] [-o FILE]` | Write a vcstool-compatible `.repos`; `--exact` pins exact revisions |
| `add <URL> [--kind K] [--version V]` | Append a repo to the manifest (path inferred from the URL) |
| `prune` | Remove materialized checkouts not declared by any manifest |
| `cache clean` | Drop the whole cache |
| `cache gc` | Drop cache entries not referenced by `ir.lock` |
| `completions <SHELL>` | Print shell completions (bash/zsh/fish/powershell/elvish) |

## Network behavior

### Retries

Every network operation (git fetch, hg clone/pull, bzr branch/pull, svn
info/cat/checkout, archive downloads) is retried with exponential backoff:

- `--retries <N>` / `IR_RETRIES` -- total attempts per operation (default 3,
  minimum 1)
- `IR_RETRY_BASE_MS` -- base backoff in milliseconds (default 1000, doubled
  after each failed attempt)

Each attempt is still bounded by the per-command timeout (300 s for network
commands). Retries are silent; only the final error is reported. Archive
downloads delete partial files before retrying.

### Fetch throttling

By default every `resolve`/`update`/`sync` fetches all mirrors. On a large
workspace that is slow and unfriendly to servers. `--fetch-interval <DURATION>`
(`IR_FETCH_INTERVAL`) skips the network fetch for mirrors that were fetched
more recently than the interval:

```sh
ir --fetch-interval 10m resolve   # mirrors fetched within 10 min are reused
```

Durations look like `30s`, `10m`, `1h` (a bare number means seconds); `0`
disables throttling. Freshness is recorded in `<cache>/<vcs>/<key>.fetch-stamp`
files next to each mirror. A fresh clone always fetches (no stamp yet), and
the poisoned-mirror self-repair still runs regardless of the interval.

## Cache layout

`<cache>/ir/` where `<cache>` is `$XDG_CACHE_HOME` (or `~/.cache`):

```text
ir/
  git/<sha256(url)>/          bare git mirrors, one per repository URL
  git/<sha256(url)>.fetch-stamp
  hg/<sha256(url)>/           local hg clones (--noupdate)
  bzr/<sha256(url)>/          local bzr branches
  archives/<sha256(url)>.<ext>  downloaded tar/zip files
```

svn keeps no local mirror; the URL itself is the cache entry.

## Troubleshooting

- **Fetch fails forever for one repo**: `ir` detects poisoned mirrors (a
  failed fetch that left a bare repo behind, or a raw URL that changed while
  the normalized cache key stayed the same) by comparing
  `remote.origin.url` and rebuilds the mirror automatically.
- **`sync` skips a repo as dirty**: local changes were detected. Inspect them
  with `ir status`; re-run with `--force` to replace the checkout.
- **A repo resolves to an unexpected revision**: numeric versions mean
  different things per VCS -- hg/svn/bzr treat them as revisions, git treats
  them as refs. Use a full hash or an explicit ref name to be unambiguous.
- **Slow repeated resolves**: use `--fetch-interval` (e.g. `10m`) to reuse
  recently-fetched mirrors.
- **Flaky network**: raise `--retries` (e.g. `--retries 5`).

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

- `src/lib.rs` -- `ir_core` library root
- `src/manifest.rs` -- manifest parsing & normalization (+ unit tests)
- `src/error.rs` -- error types
- `src/process.rs` -- `std::process::Command` runner with timeouts
- `src/cache.rs` -- cache root, bare git mirrors, archive downloads
- `src/provider.rs` -- `Provider` trait + `provider_for` dispatch
- `src/resolve.rs` -- recursive `Resolver` (cycle / conflict detection)
- `src/lock.rs` -- `ir.lock` read/write (TOML)
- `src/providers/git.rs` -- `GitProvider` (+ integration tests)
- `src/providers/hg.rs` -- `HgProvider` (+ integration tests)
- `src/providers/svn.rs` -- `SvnProvider` (+ integration tests)
- `src/providers/bzr.rs` -- `BzrProvider` (+ integration tests)
- `src/providers/archive.rs` -- `TarProvider` / `ZipProvider` (+ tests)
- `src/inspect.rs` -- workspace inspection shared by `status` and sync
- `src/commands.rs` -- Phase 5 command implementations (binary-private)
- `src/main.rs` -- `ir` CLI
