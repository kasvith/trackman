//! Everything that touches disk or scdl: config, scanning the library, listing playlists, syncing.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::{fs, thread};

pub const CONFIG: &str = "trackman.toml";
/// Maps each file trackman downloaded or linked to its track, since the filename doesn't carry the id.
const INDEX: &str = "trackman-index.toml";
/// No playlist position: reordering a playlist must never change a filename.
/// No `[id]`: DJ software reads it as part of the title or album. `INDEX` holds the id instead.
const NAME_FORMAT: &str = "%(uploader)s - %(title)s.%(ext)s";
const AUDIO: &[&str] = &["m4a", "mp3", "opus", "flac", "wav", "aiff", "ogg"];

/// File path relative to the library root, `/`-separated on every OS, to track id.
type Index = BTreeMap<String, String>;

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    /// Track ids that failed as DRM protected; never retried.
    #[serde(default)]
    pub unavailable: Vec<String>,
    #[serde(default, rename = "playlist")]
    pub playlists: Vec<Playlist>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Playlist {
    pub url: String,
    /// Folder relative to the library root. Empty until the playlist title is known.
    #[serde(default)]
    pub dir: String,
}

impl Config {
    pub fn load(root: &Path) -> Result<Config, String> {
        load(root, CONFIG)
    }

    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        fs::write(root.join(CONFIG), toml::to_string_pretty(self).expect("config serializes"))
    }
}

/// Reads a TOML file at the library root. A missing file reads as the default.
fn load<T: DeserializeOwned + Default>(root: &Path, name: &str) -> Result<T, String> {
    match fs::read_to_string(root.join(name)) {
        Ok(s) => toml::from_str(&s).map_err(|e| format!("{name}: {e}")),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(format!("{name}: {e}")),
    }
}

#[derive(Deserialize)]
pub struct Track {
    pub id: String,
    pub url: String,
}

pub struct Remote {
    pub title: String,
    pub tracks: Vec<Track>,
}

pub struct LocalFile {
    pub path: PathBuf,
    /// From the index, or from `[id]` at the end of the filename (downloads from older trackman versions).
    pub id: Option<String>,
    /// From the WWWAUDIOFILE tag scdl embeds (files with no known id).
    pub url: Option<String>,
}

pub enum Status {
    Have,
    Link(PathBuf),
    New,
    Drm,
    Extra,
}

pub struct Row {
    pub status: Status,
    pub name: String,
}

pub enum Msg {
    Remote(String, Result<Remote, String>),
    Scanned(Vec<LocalFile>),
    /// A file a sync just linked or downloaded, so the UI updates before the end-of-sync rescan.
    File(LocalFile),
    Log(String),
    Drm(String),
    Done,
}

pub fn dir_name(p: &Playlist, remote: &Remote) -> String {
    if !p.dir.is_empty() {
        return p.dir.clone();
    }
    // The title comes from SoundCloud; keep it a single folder inside the root.
    let title = remote.title.replace(['/', '\\'], "-");
    let title = title.trim().trim_start_matches('.');
    if title.is_empty() { "playlist".into() } else { title.into() }
}

/// Lists a playlist through scdl so its auth token applies. Takes a few seconds, downloads nothing.
pub fn fetch(url: &str) -> Result<Remote, String> {
    #[derive(Deserialize)]
    struct Listing {
        title: String,
        entries: Vec<Track>,
    }
    let out = Command::new("scdl")
        .args(["-l", url, "--yt-dlp-args", "--flat-playlist -J --no-update"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("scdl: {e}"))?;
    let listing: Listing = serde_json::from_slice(&out.stdout).map_err(|_| {
        let stderr = String::from_utf8_lossy(&out.stderr);
        stderr.lines().rfind(|l| l.contains("ERROR")).unwrap_or("could not list playlist").to_string()
    })?;
    Ok(Remote { title: listing.title, tracks: listing.entries })
}

pub fn scan(root: &Path) -> Vec<LocalFile> {
    // An unreadable index only slows this down (its files fall back to the URL tag); sync refuses to overwrite it.
    let index: Index = load(root, INDEX).unwrap_or_default();
    let mut paths = Vec::new();
    walk(root, &mut paths);
    (paths.into_iter())
        .map(|path| {
            // Index first: a title like "Intro [2024]" puts `[digits]` at the end of a new filename.
            let id = index.get(&index_key(root, &path)).cloned().or_else(|| id_from_name(&path));
            // ponytail: one ffprobe per file without a known id (~30ms each); cache by path+mtime if big libraries feel slow
            let url = if id.is_none() { url_tag(&path) } else { None };
            LocalFile { path, id, url }
        })
        .collect()
}

fn walk(dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&path, paths);
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        if AUDIO.contains(&ext.as_str()) {
            paths.push(path);
        }
    }
}

/// `<root>/sets/warmup/A.m4a` -> `sets/warmup/A.m4a`, with `/` on every OS so a library drive works on any machine.
fn index_key(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// Folders under the root that complete `typed`, ignoring case: `sets/w` -> `["sets/warmup/", "sets/Winter/"]`.
pub fn complete_dir(root: &Path, typed: &str) -> Vec<String> {
    let (parent, prefix) = typed.split_at(typed.rfind('/').map_or(0, |i| i + 1));
    let Ok(entries) = fs::read_dir(root.join(parent.trim_start_matches('/'))) else { return Vec::new() };
    let prefix = prefix.to_lowercase();
    let mut dirs: Vec<String> = (entries.flatten())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.') && name.to_lowercase().starts_with(&prefix))
        .map(|name| format!("{parent}{name}/"))
        .collect();
    dirs.sort_by_key(|d| d.to_lowercase());
    dirs
}

fn id_from_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let id = stem.strip_suffix(']')?.rsplit_once('[')?.1;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

fn url_tag(path: &Path) -> Option<String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format_tags=WWWAUDIOFILE", "-of", "default=nw=1:nk=1"])
        .arg(path)
        .output()
        .ok()?;
    let url = String::from_utf8(out.stdout).ok()?.trim().to_string();
    url.starts_with("http").then_some(url)
}

/// One row per remote track, in playlist order, then local files in `dir` the playlist no longer has.
/// `shared` are other playlists synced into the same `dir`; their files aren't `Extra`.
pub fn plan(remote: &Remote, shared: &[&Remote], files: &[LocalFile], dir: &Path, unavailable: &[String]) -> Vec<Row> {
    let matches = |t: &Track, f: &LocalFile| f.id.as_ref() == Some(&t.id) || f.url.as_ref() == Some(&t.url);
    let in_dir = |f: &LocalFile| f.path.parent() == Some(dir);
    let file_name = |f: &LocalFile| f.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let slug = |t: &Track| t.url.trim_start_matches("https://soundcloud.com/").to_string();

    let mut rows: Vec<Row> = remote
        .tracks
        .iter()
        .map(|t| {
            let here = files.iter().find(|f| matches(t, f) && in_dir(f));
            match (here, files.iter().find(|f| matches(t, f))) {
                (Some(f), _) => Row { status: Status::Have, name: file_name(f) },
                (None, Some(f)) => Row { status: Status::Link(f.path.clone()), name: file_name(f) },
                _ if unavailable.contains(&t.id) => Row { status: Status::Drm, name: slug(t) },
                _ => Row { status: Status::New, name: slug(t) },
            }
        })
        .collect();
    rows.extend(
        files
            .iter()
            .filter(|f| in_dir(f))
            .filter(|f| {
                !std::iter::once(remote).chain(shared.iter().copied()).any(|r| r.tracks.iter().any(|t| matches(t, f)))
            })
            .map(|f| Row { status: Status::Extra, name: file_name(f) }),
    );
    rows
}

/// Runs on a worker thread: syncs playlists one after another, reporting through `tx`.
pub fn sync_all(root: PathBuf, playlists: Vec<Playlist>, mut unavailable: Vec<String>, tx: Sender<Msg>) {
    for p in &playlists {
        tx.send(Msg::Log(format!("── {}", if p.dir.is_empty() { &p.url } else { &p.dir }))).ok();
        if let Err(e) = sync(&root, p, &mut unavailable, &tx) {
            tx.send(Msg::Log(format!("error: {e}"))).ok();
        }
    }
    tx.send(Msg::Scanned(scan(&root))).ok();
    tx.send(Msg::Done).ok();
}

fn sync(root: &Path, p: &Playlist, unavailable: &mut Vec<String>, tx: &Sender<Msg>) -> Result<(), String> {
    let log = |s: String| {
        tx.send(Msg::Log(s)).ok();
    };
    let remote = fetch(&p.url)?;
    let dir = root.join(dir_name(p, &remote));
    // Loaded before anything changes on disk, so an unreadable index stops the sync instead of being overwritten.
    let mut index: Index = load(root, INDEX)?;
    let before = index.clone();
    // No `shared`: sync never acts on `Extra` rows.
    let rows = plan(&remote, &[], &scan(root), &dir, unavailable);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    // Rows start with one per track, in playlist order, so zip pairs each Link row with its track.
    for (t, row) in remote.tracks.iter().zip(&rows) {
        if let Status::Link(src) = &row.status {
            let dst = dir.join(&row.name);
            let linked = match fs::hard_link(src, &dst) {
                Err(e) if e.kind() == ErrorKind::CrossesDevices => fs::copy(src, &dst).map(|_| ()),
                r => r,
            };
            match linked {
                Ok(()) => {
                    index.insert(index_key(root, &dst), t.id.clone());
                    log(format!("linked {}", row.name));
                    tx.send(Msg::File(LocalFile { path: dst, id: Some(t.id.clone()), url: None })).ok();
                }
                Err(e) => log(format!("link failed {}: {e}", row.name)),
            }
        }
    }

    // Every track not marked New goes into scdl's archive, so scdl downloads exactly the new ones.
    let skip: Vec<&str> = (remote.tracks.iter().zip(&rows))
        .filter(|(_, r)| !matches!(r.status, Status::New))
        .map(|(t, _)| t.id.as_str())
        .collect();
    let new = remote.tracks.len() - skip.len();
    log(format!("{new} new of {} tracks", remote.tracks.len()));
    if new > 0 {
        let drm = download(&p.url, &dir, &skip, tx, |id, name| {
            let path = dir.join(&name);
            let key = index_key(root, &path);
            match index.get(&key) {
                // ponytail: tracks with the same uploader and title share a filename, so only the first one is kept;
                // download clashes again with `[id]` in the name if this comes up
                Some(other) if *other != id => log(format!("{name} is already track {other}, skipped track {id}")),
                _ => {
                    index.insert(key, id.clone());
                    tx.send(Msg::File(LocalFile { path, id: Some(id), url: None })).ok();
                }
            }
        })?;
        for id in drm {
            unavailable.push(id.clone());
            tx.send(Msg::Drm(id)).ok();
        }
    }
    if index != before {
        fs::write(root.join(INDEX), toml::to_string_pretty(&index).expect("index serializes"))
            .map_err(|e| format!("{INDEX}: {e}"))?;
    }
    tx.send(Msg::Remote(p.url.clone(), Ok(remote))).ok();
    Ok(())
}

/// Runs scdl on the playlist with `skip` pre-recorded as downloaded. Calls `saved(id, file name)` as each file
/// lands. Returns ids that failed as DRM protected.
fn download(
    url: &str,
    dir: &Path,
    skip: &[&str],
    tx: &Sender<Msg>,
    mut saved: impl FnMut(String, String),
) -> Result<Vec<String>, String> {
    let archive = std::env::temp_dir().join(format!("trackman-{}.txt", std::process::id()));
    let lines: String = skip.iter().map(|id| format!("soundcloud {id}\n")).collect();
    fs::write(&archive, lines).map_err(|e| format!("{}: {e}", archive.display()))?;

    let spawned = Command::new("scdl")
        .args(["-l", url, "--no-playlist-folder", "--hide-progress", "--playlist-name-format", NAME_FORMAT])
        // scdl logs everything (its own and yt-dlp's) to stderr, so stdout carries only these `id path` lines,
        // which yt-dlp prints and flushes as each file is moved into place.
        .args(["--yt-dlp-args", "--no-update --print 'after_move:%(id)s %(filepath)s'"])
        .arg("--path")
        .arg(dir)
        .arg("--download-archive")
        .arg(&archive)
        // scdl writes the playlist cover to `..jpg` in its working dir; keep it out of the library and the user's cwd.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            fs::remove_file(&archive).ok();
            return Err(format!("scdl: {e}"));
        }
    };

    let (stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let err_tx = tx.clone();
    let errors = thread::spawn(move || {
        (BufReader::new(stderr).lines().map_while(Result::ok))
            .filter_map(|line| {
                let id = drm_id(&line).map(str::to_string);
                err_tx.send(Msg::Log(line)).ok();
                id
            })
            .collect::<Vec<_>>()
    });
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        match saved_file(&line) {
            Some((id, name)) => saved(id, name),
            None => {
                tx.send(Msg::Log(line)).ok();
            }
        }
    }
    let drm = errors.join().unwrap_or_default();
    let status = child.wait();
    fs::remove_file(&archive).ok();
    // scdl exits non-zero when any track fails (DRM included); the log already shows why.
    if let Ok(s) = status
        && !s.success()
    {
        tx.send(Msg::Log(format!("scdl finished with {s}"))).ok();
    }
    Ok(drm)
}

/// `ERROR: [soundcloud] 775475710: This video is DRM protected` -> `775475710`
fn drm_id(line: &str) -> Option<&str> {
    if !line.contains("DRM protected") {
        return None;
    }
    line.split("] ").nth(1)?.split(':').next()
}

/// `2383362741 /lib/warmup/Marsh - Aloft.m4a` -> `("2383362741", "Marsh - Aloft.m4a")`
fn saved_file(line: &str) -> Option<(String, String)> {
    let (id, path) = line.split_once(' ')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((id.to_string(), Path::new(path).file_name()?.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_and_parses() {
        let track = |id: &str| Track { id: id.into(), url: format!("https://soundcloud.com/a/{id}") };
        let file = |path: &str, id: Option<&str>, url: Option<&str>| LocalFile {
            path: path.into(),
            id: id.map(Into::into),
            url: url.map(Into::into),
        };
        let remote = Remote { title: "x".into(), tracks: ["1", "2", "3", "4", "5"].map(track).into() };
        let files = [
            file("/r/p/A [1].m4a", Some("1"), None),
            file("/r/p/2. Legacy.m4a", None, Some("https://soundcloud.com/a/2")),
            file("/r/other/B [3].m4a", Some("3"), None),
            file("/r/p/Gone [9].m4a", Some("9"), None),
        ];
        // Track 9 left this playlist but is in another one synced into the same folder, so it isn't extra.
        let other = Remote { title: "y".into(), tracks: vec![track("9")] };
        assert!(
            plan(&remote, &[&other], &files, Path::new("/r/p"), &[]).iter().all(|r| !matches!(r.status, Status::Extra))
        );
        let rows = plan(&remote, &[], &files, Path::new("/r/p"), &["4".into()]);
        let got: Vec<_> = (rows.iter())
            .map(|r| match r.status {
                Status::Have => "have",
                Status::Link(_) => "link",
                Status::New => "new",
                Status::Drm => "drm",
                Status::Extra => "extra",
            })
            .collect();
        assert_eq!(got, ["have", "have", "link", "drm", "new", "extra"]);

        assert_eq!(id_from_name(Path::new("d/Marsh - Aloft [123].m4a")).as_deref(), Some("123"));
        assert_eq!(id_from_name(Path::new("d/Mind Opener [Meanwhile].m4a")), None);
        assert_eq!(drm_id("ERROR: [soundcloud] 775475710: This video is DRM protected"), Some("775475710"));
        assert_eq!(drm_id("ERROR: [soundcloud] 775475710: HTTP Error 404"), None);
        assert_eq!(saved_file("123 /lib/warmup/Marsh - Aloft.m4a"), Some(("123".into(), "Marsh - Aloft.m4a".into())));
        assert_eq!(saved_file(""), None);
        assert_eq!(saved_file("[soundcloud] 123: oops"), None);

        let p = Playlist { url: String::new(), dir: String::new() };
        assert_eq!(dir_name(&p, &Remote { title: "../ideas/2".into(), tracks: vec![] }), "-ideas-2");
        assert_eq!(dir_name(&p, &Remote { title: " .. ".into(), tracks: vec![] }), "playlist");
        let named = Playlist { url: String::new(), dir: "sets/warmup".into() };
        assert_eq!(dir_name(&named, &Remote { title: "Warmup".into(), tracks: vec![] }), "sets/warmup");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trackman-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_round_trips() {
        let root = temp_dir("config");
        assert!(Config::load(&root).unwrap().playlists.is_empty());

        let url = "https://soundcloud.com/you/sets/warmup";
        let config = Config {
            unavailable: vec!["775475710".into()],
            playlists: vec![Playlist { url: url.into(), dir: "warmup".into() }],
        };
        config.save(&root).unwrap();
        let loaded = Config::load(&root).unwrap();
        assert_eq!(loaded.unavailable, ["775475710"]);
        assert_eq!((loaded.playlists[0].url.as_str(), loaded.playlists[0].dir.as_str()), (url, "warmup"));

        // Added by hand without a dir: it's filled in once the playlist title is known.
        fs::write(root.join(CONFIG), "[[playlist]]\nurl = \"u\"\n").unwrap();
        assert_eq!(Config::load(&root).unwrap().playlists[0].dir, "");

        fs::write(root.join(CONFIG), "playlist = 3").unwrap();
        assert!(matches!(Config::load(&root), Err(e) if e.starts_with(CONFIG)));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scans_audio_files() {
        let root = temp_dir("scan");
        for f in [
            "p/A [1].m4a",
            "p/nested/B [2].MP3",
            "p/nested/Indexed.m4a",
            "p/Intro [2024].m4a",
            "p/Legacy.m4a",
            "p/cover.jpg",
            "p/.C [3].m4a",
            ".hidden/D [4].m4a",
            "E [5].flac",
        ] {
            let path = root.join(f);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let index =
            Index::from([("p/nested/Indexed.m4a".into(), "6".into()), ("p/Intro [2024].m4a".into(), "7".into())]);
        fs::write(root.join(INDEX), toml::to_string_pretty(&index).unwrap()).unwrap();
        let mut ids: Vec<_> = scan(&root).into_iter().map(|f| f.id.unwrap_or_default()).collect();
        ids.sort();
        assert_eq!(ids, ["", "1", "2", "5", "6", "7"]);

        fs::create_dir_all(root.join("P2")).unwrap();
        assert_eq!(complete_dir(&root, ""), ["p/", "P2/"]);
        assert_eq!(complete_dir(&root, "p"), ["p/", "P2/"]);
        assert_eq!(complete_dir(&root, "p/N"), ["p/nested/"]);
        assert!(complete_dir(&root, "missing/").is_empty());
        fs::remove_dir_all(&root).unwrap();
    }
}
