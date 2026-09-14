# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

trackman is a Rust TUI (ratatui) that syncs SoundCloud playlists into folders on disk. It works out what's missing; `scdl` does the downloading. Needs Rust 1.88+ (edition 2024, let-chains).

## Commands

```sh
cargo run -- /path/to/library        # library root; defaults to the current dir
cargo test                           # all tests
cargo test scans_audio_files         # one test, by name substring
cargo fmt --check                    # rustfmt.toml: max_width 120, use_small_heuristics = "Max"
cargo clippy --locked --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`) runs tests on Linux, macOS and Windows, plus fmt and clippy on Linux, all with `--locked`, so keep `Cargo.lock` in sync.

## Architecture

Two files. `src/library.rs` holds everything that touches disk or external processes: config, scanning, listing, syncing. `src/main.rs` is the TUI (`App`) and has no I/O beyond calling into `library`.

**Threading.** Slow work runs on spawned threads that report back through an `mpsc` channel of `library::Msg`. `App::run` drains the channel, redraws, and polls keys every 200ms. On refresh, each playlist gets its own `fetch` thread and the library scan gets one more. `sync_all` runs playlists one after another on a single worker thread; `App.busy` guards against starting a second sync.

**Planning is pure.** `plan(remote, files, dir, unavailable)` turns a playlist's remote tracks and the scanned local files into `Row`s with one of five statuses: `Have`, `Link`, `New`, `Drm`, `Extra`. The UI recomputes rows every frame, and `sync` uses the same function, so status logic lives only there. A local file matches a track by the `[id]` in its filename, or by the `WWWAUDIOFILE` URL tag read with `ffprobe` (for older scdl files without an id).

**Sync flow.** For each playlist, `sync`:
1. Hardlinks `Link` rows, copying instead when the folders are on different drives.
2. Writes a temporary scdl `--download-archive` file listing every track *not* marked `New`.
3. Runs scdl once, so it downloads exactly the new tracks.
4. Parses scdl's stderr for DRM errors and sends each id as `Msg::Drm`. `App` then saves those ids to `unavailable` in the config.

**Config.** `trackman.toml` lives at the library root. A playlist added with a blank `dir` gets its folder from the SoundCloud title (`dir_name`) when the first `Msg::Remote` arrives.

## Invariants

- Never rename, move or delete files in the library. Tracks removed from a playlist show as `Extra` and are left alone.
- `NAME_FORMAT` deliberately has no playlist position. Reordering a playlist must never change a filename.
- `ponytail:` comments mark deliberate simplifications and name their limit and the upgrade path. Keep them accurate when changing that code.

## Tests

Tests don't need scdl or ffprobe. When ffprobe is missing, `url_tag` returns `None`. Filesystem tests use per-test folders under `std::env::temp_dir()` (there's no tempfile crate). UI logic is tested without a terminal through `App::new(root, config)` and `app.key(KeyCode::…)`. Avoid tests that add a *new* playlist URL, because `App::add` starts a real `scdl` fetch thread.

## Releases

- Commit messages must follow Conventional Commits. git-cliff's built-in default config (there's no `cliff.toml`) builds `CHANGELOG.md` and the release notes, and skips commits in any other format.
- `./release.sh [X.Y.Z]` bumps `Cargo.toml` and `Cargo.lock`, updates `CHANGELOG.md`, commits, tags `vX.Y.Z` and pushes.
- The tag runs `.github/workflows/release.yml`: CI, then builds for each target, then one GitHub release with `sha256sums.txt`.
- `install.sh` downloads `trackman-<target-triple>.tar.gz` and `sha256sums.txt` by name from the release. Renaming assets or changing targets (Linux builds are `*-unknown-linux-musl`) breaks the installer.
