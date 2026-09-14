mod library;

use library::{Config, LocalFile, Msg, Playlist, Remote, Row, Status};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use std::{env, fs, process, thread};

enum Input {
    None,
    Url(String),
    Dir { url: String, buf: String },
}

struct App {
    root: PathBuf,
    config: Config,
    /// Keyed by playlist url. Missing while a fetch is in flight.
    remotes: HashMap<String, Result<Remote, String>>,
    /// None while the library is being scanned.
    files: Option<Vec<LocalFile>>,
    log: Vec<String>,
    busy: bool,
    input: Input,
    playlists: ListState,
    tracks: ListState,
    focus_tracks: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

fn main() {
    let arg = PathBuf::from(env::args().nth(1).unwrap_or_else(|| ".".into()));
    let root = fs::create_dir_all(&arg)
        .and_then(|()| arg.canonicalize())
        .unwrap_or_else(|e| die(&format!("{}: {e}", arg.display())));
    let config = Config::load(&root).unwrap_or_else(|e| die(&e));
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        root,
        config,
        remotes: HashMap::new(),
        files: None,
        log: Vec::new(),
        busy: false,
        input: Input::None,
        playlists: ListState::default().with_selected(Some(0)),
        tracks: ListState::default(),
        focus_tracks: false,
        tx,
        rx,
    };
    app.refresh();
    let result = app.run(ratatui::init());
    ratatui::restore();
    if let Err(e) = result {
        die(&e.to_string());
    }
}

fn die(msg: &str) -> ! {
    eprintln!("trackman: {msg}");
    process::exit(1)
}

impl App {
    fn run(&mut self, mut terminal: DefaultTerminal) -> std::io::Result<()> {
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                self.handle(msg);
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(200))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && !self.key(key.code)
            {
                return Ok(());
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Remote(url, remote) => {
                // A playlist added with a blank folder takes its SoundCloud title.
                if let Ok(r) = &remote
                    && let Some(p) = self.config.playlists.iter_mut().find(|p| p.url == url && p.dir.is_empty())
                {
                    p.dir = library::dir_name(p, r);
                    self.save();
                }
                self.remotes.insert(url, remote);
            }
            Msg::Scanned(files) => self.files = Some(files),
            Msg::Log(line) => self.push_log(line),
            Msg::Drm(id) => {
                if !self.config.unavailable.contains(&id) {
                    self.config.unavailable.push(id);
                    self.save();
                }
            }
            Msg::Done => self.busy = false,
        }
    }

    /// Returns false to quit.
    fn key(&mut self, code: KeyCode) -> bool {
        match std::mem::replace(&mut self.input, Input::None) {
            Input::Url(mut buf) => match code {
                KeyCode::Enter if !buf.trim().is_empty() => {
                    // Share links carry tracking params; the secret s-… token is in the path, so keep that.
                    let url = buf.trim().split('?').next().unwrap_or_default().to_string();
                    self.input = Input::Dir { url, buf: String::new() };
                }
                KeyCode::Esc => {}
                code => {
                    edit(&mut buf, code);
                    self.input = Input::Url(buf);
                }
            },
            Input::Dir { url, mut buf } => match code {
                KeyCode::Enter => self.add(url, buf.trim().trim_matches('/').to_string()),
                KeyCode::Esc => {}
                code => {
                    edit(&mut buf, code);
                    self.input = Input::Dir { url, buf };
                }
            },
            Input::None => match code {
                KeyCode::Char('q') if self.busy => self.push_log("sync running, press Q to quit anyway".into()),
                KeyCode::Char('q' | 'Q') => return false,
                KeyCode::Char('a') => self.input = Input::Url(String::new()),
                KeyCode::Char('r') => self.refresh(),
                KeyCode::Char('s') => {
                    if let Some(p) = self.selected().cloned() {
                        self.sync(vec![p]);
                    }
                }
                KeyCode::Char('S') => self.sync(self.config.playlists.clone()),
                KeyCode::Char('x') if !self.busy => {
                    if let Some(i) = self.playlists.selected().filter(|&i| i < self.config.playlists.len()) {
                        let p = self.config.playlists.remove(i);
                        self.remotes.remove(&p.url);
                        self.save();
                        self.push_log(format!("removed {} (files kept)", p.url));
                    }
                }
                KeyCode::Tab => self.focus_tracks = !self.focus_tracks,
                KeyCode::Down | KeyCode::Char('j') => self.step(true),
                KeyCode::Up | KeyCode::Char('k') => self.step(false),
                _ => {}
            },
        }
        true
    }

    fn step(&mut self, down: bool) {
        let list = if self.focus_tracks { &mut self.tracks } else { &mut self.playlists };
        if down { list.select_next() } else { list.select_previous() }
        if !self.focus_tracks {
            self.tracks = ListState::default();
        }
    }

    fn selected(&self) -> Option<&Playlist> {
        self.playlists.selected().and_then(|i| self.config.playlists.get(i))
    }

    fn add(&mut self, url: String, dir: String) {
        if self.config.playlists.iter().any(|p| p.url == url) {
            return self.push_log(format!("already added: {url}"));
        }
        self.config.playlists.push(Playlist { url: url.clone(), dir });
        self.playlists.select(Some(self.config.playlists.len() - 1));
        self.save();
        self.fetch(url);
    }

    fn refresh(&mut self) {
        if self.busy {
            return;
        }
        self.remotes.clear();
        self.files = None;
        // ponytail: one scdl process per playlist at once; queue them if SoundCloud starts rate limiting
        for p in &self.config.playlists {
            self.fetch(p.url.clone());
        }
        let (tx, root) = (self.tx.clone(), self.root.clone());
        thread::spawn(move || tx.send(Msg::Scanned(library::scan(&root))));
    }

    fn fetch(&self, url: String) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let remote = library::fetch(&url);
            tx.send(Msg::Remote(url, remote))
        });
    }

    fn sync(&mut self, playlists: Vec<Playlist>) {
        if self.busy || playlists.is_empty() {
            return;
        }
        self.busy = true;
        let (root, unavailable, tx) = (self.root.clone(), self.config.unavailable.clone(), self.tx.clone());
        thread::spawn(move || library::sync_all(root, playlists, unavailable, tx));
    }

    fn push_log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > 1000 {
            self.log.drain(..500);
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.config.save(&self.root) {
            self.push_log(format!("saving {}: {e}", library::CONFIG));
        }
    }

    /// None while the playlist listing or the library scan is still loading.
    // ponytail: recomputed every frame; cache per playlist on Remote/Scanned if big libraries lag
    fn rows(&self, p: &Playlist) -> Option<Result<Vec<Row>, &str>> {
        let remote = match self.remotes.get(&p.url)? {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        let dir = self.root.join(library::dir_name(p, remote));
        Some(Ok(library::plan(remote, self.files.as_ref()?, &dir, &self.config.unavailable)))
    }

    fn draw(&mut self, f: &mut Frame) {
        let [main, log, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(10), Constraint::Length(1)]).areas(f.area());
        let [left, right] = Layout::horizontal([Constraint::Percentage(35), Constraint::Fill(1)]).areas(main);
        let border = |focused: bool| Style::new().fg(if focused { Color::Cyan } else { Color::DarkGray });

        let items: Vec<Line> = (self.config.playlists.iter())
            .map(|p| {
                let mut spans = vec![Span::from(if p.dir.is_empty() { p.url.clone() } else { p.dir.clone() })];
                match self.rows(p) {
                    None => spans.push(" …".dark_gray()),
                    Some(Err(_)) => spans.push(" error".red()),
                    Some(Ok(rows)) => spans.extend(summary(&rows)),
                }
                Line::from(spans)
            })
            .collect();
        let list = List::new(items)
            .highlight_style(Style::new().reversed())
            .block(Block::bordered().title(" playlists ").border_style(border(!self.focus_tracks)));
        f.render_stateful_widget(list, left, &mut self.playlists);

        let (title, body) = match self.selected() {
            None => (String::new(), Err("press a to add a playlist".to_string())),
            Some(p) => (
                format!(" {} ", p.dir),
                match self.rows(p) {
                    None => Err("loading…".into()),
                    Some(Err(e)) => Err(e.to_string()),
                    Some(Ok(rows)) => Ok((rows.into_iter())
                        .map(|r| Line::from(vec![label(&r.status), " ".into(), r.name.into()]))
                        .collect::<Vec<_>>()),
                },
            ),
        };
        let block = Block::bordered().title(title).border_style(border(self.focus_tracks));
        match body {
            Ok(items) => f.render_stateful_widget(
                List::new(items).highlight_style(Style::new().reversed()).block(block),
                right,
                &mut self.tracks,
            ),
            Err(msg) => f.render_widget(Paragraph::new(msg).block(block), right),
        }

        let height = log.height.saturating_sub(2) as usize;
        let lines: Vec<Line> = self.log[self.log.len().saturating_sub(height)..].iter().map(|l| Line::from(l.as_str())).collect();
        let title = if self.busy { " log · syncing… " } else { " log " };
        f.render_widget(Paragraph::new(lines).block(Block::bordered().title(title)), log);

        let help = match &self.input {
            Input::Url(buf) => format!("playlist URL: {buf}▏"),
            Input::Dir { buf, .. } => format!("folder in {} (blank = playlist title): {buf}▏", self.root.display()),
            Input::None => format!(
                "a add · r refresh · s sync · S sync all · x remove · tab switch panel · q quit    {}",
                self.root.display()
            ),
        };
        f.render_widget(Paragraph::new(help).dark_gray(), footer);
    }
}

fn edit(buf: &mut String, code: KeyCode) {
    match code {
        KeyCode::Char(c) => buf.push(c),
        KeyCode::Backspace => {
            buf.pop();
        }
        _ => {}
    }
}

fn label(status: &Status) -> Span<'static> {
    match status {
        Status::Have => "have ".green(),
        Status::Link(_) => "link ".cyan(),
        Status::New => "new  ".yellow(),
        Status::Drm => "drm  ".red(),
        Status::Extra => "extra".magenta(),
    }
}

/// ` 29/42 4 new 2 link 9 drm`
fn summary(rows: &[Row]) -> Vec<Span<'static>> {
    let count = |f: fn(&Status) -> bool| rows.iter().filter(|r| f(&r.status)).count();
    let extra = count(|s| matches!(s, Status::Extra));
    let mut spans = vec![format!(" {}/{}", count(|s| matches!(s, Status::Have)), rows.len() - extra).into()];
    for (n, name, color) in [
        (count(|s| matches!(s, Status::New)), "new", Color::Yellow),
        (count(|s| matches!(s, Status::Link(_))), "link", Color::Cyan),
        (count(|s| matches!(s, Status::Drm)), "drm", Color::Red),
        (extra, "extra", Color::Magenta),
    ] {
        if n > 0 {
            spans.push(format!(" {n} {name}").fg(color));
        }
    }
    spans
}
