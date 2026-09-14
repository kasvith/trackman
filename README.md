# trackman

A terminal UI that keeps SoundCloud playlists in sync with folders on disk, downloading each track only once.

trackman works out what's missing and [scdl](https://github.com/scdl-org/scdl) does the downloading.

## Why

Running `scdl -l <playlist>` again re-downloads tracks you already have. Its filenames include the playlist position, so when a track moves in the playlist, scdl no longer finds the file and downloads it again. A track that's in two playlists also gets downloaded twice.

## Features

- Pick a library root; each playlist syncs into its own folder inside it.
- Downloads only tracks that aren't already in the playlist's folder.
- A track already saved in another playlist's folder is hardlinked, not downloaded: no extra disk space, and it shows up as a normal file in both folders.
- DRM-protected tracks (which can't be downloaded) are recorded and skipped on later syncs.
- Files are never renamed or moved. New downloads are named `Uploader - Title [id].m4a`, with no playlist position.
- Recognises files downloaded earlier with scdl through the SoundCloud URL scdl embeds in each file.
- Tracks removed from a SoundCloud playlist are shown as `extra` and never deleted.

## Requirements

- [scdl](https://github.com/scdl-org/scdl), set up so it can download your playlists. Private playlists need either their secret share link or an `auth_token` in `~/.config/scdl/scdl.cfg`.
- `ffprobe` from ffmpeg, which scdl needs anyway.
- Rust 1.88 or newer to build.

## Install

```sh
cargo install --git https://github.com/kasvith/trackman
```

or from a clone:

```sh
cargo install --path .
```

## Usage

```sh
trackman ~/Music/deejay
```

The folder is the library root; it's created if it doesn't exist, and defaults to the current directory. On start, trackman scans the root and fetches every playlist, then shows what each one has, is missing, or can't get.

| Key | Action |
| --- | --- |
| `a` | Add a playlist: paste the URL, then a folder name (leave it blank to use the playlist title) |
| `s` | Sync the selected playlist |
| `S` | Sync all playlists |
| `r` | Refresh: rescan the library and fetch the playlists again |
| `x` | Remove the selected playlist from the config (files stay) |
| `Tab` | Switch between the playlist and track panels |
| `j` / `k`, `↓` / `↑` | Move the selection |
| `q` | Quit (while a sync is running, `Q` quits anyway) |

Each track's status:

| Status | Meaning |
| --- | --- |
| `have` | Already in this playlist's folder |
| `link` | Saved in another folder; syncing hardlinks it here |
| `new` | Syncing downloads it |
| `drm` | DRM protected; skipped |
| `extra` | In the folder but no longer in the playlist; left alone |

## Library layout

```
~/Music/deejay/
├── trackman.toml
├── ideas 2/
│   ├── Marsh - Marsh - Aloft [2383362741].m4a
│   └── ...
└── warmup/
    └── ...
```

`trackman.toml` holds your playlists and the DRM-protected track IDs. You can edit it by hand while trackman isn't running:

```toml
unavailable = ["775475710"]

[[playlist]]
url = "https://soundcloud.com/you/sets/warmup/s-XXXXXXXX"
dir = "warmup"
```

`dir` is relative to the library root and can be nested, e.g. `sets/2026/warmup`.

## How a sync works

1. Lists the playlist with `scdl --yt-dlp-args "--flat-playlist -J"`. This uses your scdl login and downloads nothing.
2. Scans the library root. A file's track ID comes from `[id]` in its name, or from the URL tag scdl embeds (`WWWAUDIOFILE`) for older files.
3. Hardlinks tracks found in other folders (falls back to copying if the folders are on different drives).
4. Writes a temporary scdl archive file listing every track it shouldn't download, then runs scdl on the playlist once, so scdl downloads exactly the new tracks.
5. Saves the IDs of any tracks that failed as DRM protected to `unavailable`.

## Limitations

- Older files without `[id]` in their name are matched by their SoundCloud URL. If an uploader changes their profile URL, trackman won't recognise the file and will download the track again.
- Older MP3 files without `[id]` in their name haven't been tested; only M4A has.
- Duplicate files already on disk aren't cleaned up.
- To retry a DRM-protected track, delete its ID from `unavailable`.

## Development

```sh
cargo test
cargo run -- /path/to/library
```

`src/library.rs` handles config, scanning, listing and syncing. `src/main.rs` is the TUI.
