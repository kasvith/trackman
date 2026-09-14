//! Everything that touches disk or scdl: config, scanning the library, listing playlists, syncing.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::{fs, thread};

pub const CONFIG: &str = "trackman.toml";
/// No playlist position in the name: reordering a playlist must never change a filename.
const NAME_FORMAT: &str = "%(uploader)s - %(title)s [%(id)s].%(ext)s";
const AUDIO: &[&str] = &["m4a", "mp3", "opus", "flac", "wav", "aiff", "ogg"];

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
        match fs::read_to_string(root.join(CONFIG)) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("{CONFIG}: {e}")),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{CONFIG}: {e}")),
        }
    }

    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        fs::write(root.join(CONFIG), toml::to_string_pretty(self).expect("config serializes"))
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
    /// From `[id]` at the end of the filename (files trackman downloaded).
    pub id: Option<String>,
    /// From the WWWAUDIOFILE tag scdl embeds (older files without an id in the name).
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
    let mut files = Vec::new();
    walk(root, &mut files);
    files
}

fn walk(dir: &Path, files: &mut Vec<LocalFile>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&path, files);
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        if !AUDIO.contains(&ext.as_str()) {
            continue;
        }
        let id = id_from_name(&path);
        // ponytail: one ffprobe per file without an [id] (~30ms each); cache by path+mtime if big libraries feel slow
        let url = if id.is_none() { url_tag(&path) } else { None };
        files.push(LocalFile { path, id, url });
    }
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
pub fn plan(remote: &Remote, files: &[LocalFile], dir: &Path, unavailable: &[String]) -> Vec<Row> {
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
            .filter(|f| in_dir(f) && !remote.tracks.iter().any(|t| matches(t, f)))
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
    let rows = plan(&remote, &scan(root), &dir, unavailable);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    for row in &rows {
        if let Status::Link(src) = &row.status {
            let dst = dir.join(&row.name);
            let linked = match fs::hard_link(src, &dst) {
                Err(e) if e.kind() == ErrorKind::CrossesDevices => fs::copy(src, &dst).map(|_| ()),
                r => r,
            };
            match linked {
                Ok(()) => log(format!("linked {}", row.name)),
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
        for id in download(&p.url, &dir, &skip, tx)? {
            unavailable.push(id.clone());
            tx.send(Msg::Drm(id)).ok();
        }
    }
    tx.send(Msg::Remote(p.url.clone(), Ok(remote))).ok();
    Ok(())
}

/// Runs scdl on the playlist with `skip` pre-recorded as downloaded. Returns ids that failed as DRM protected.
fn download(url: &str, dir: &Path, skip: &[&str], tx: &Sender<Msg>) -> Result<Vec<String>, String> {
    let archive = std::env::temp_dir().join(format!("trackman-{}.txt", std::process::id()));
    let lines: String = skip.iter().map(|id| format!("soundcloud {id}\n")).collect();
    fs::write(&archive, lines).map_err(|e| format!("{}: {e}", archive.display()))?;

    let spawned = Command::new("scdl")
        .args(["-l", url, "--no-playlist-folder", "--hide-progress", "--playlist-name-format", NAME_FORMAT])
        .args(["--yt-dlp-args", "--no-update"])
        .arg("--path")
        .arg(dir)
        .arg("--download-archive")
        .arg(&archive)
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
        tx.send(Msg::Log(line)).ok();
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
        let rows = plan(&remote, &files, Path::new("/r/p"), &["4".into()]);
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

        let p = Playlist { url: String::new(), dir: String::new() };
        assert_eq!(dir_name(&p, &Remote { title: "../ideas/2".into(), tracks: vec![] }), "-ideas-2");
    }
}
