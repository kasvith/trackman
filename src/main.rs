mod library;

use library::{Config, LocalFile, Msg, Playlist, Remote, Row, Status};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use std::{env, fs, process, thread};

enum Input {
    None,
    Url(String),
    /// `menu` holds folder completions after Tab, and `pick` the one Tab cycled to.
    Dir {
        url: String,
        buf: String,
        menu: Vec<String>,
        pick: Option<usize>,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Playlists,
    Tracks,
    Log,
}

const PANES: [Pane; 3] = [Pane::Playlists, Pane::Tracks, Pane::Log];

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
    focus: Pane,
    /// Lines scrolled up from the newest; 0 follows new output.
    log_scroll: usize,
    /// Where each pane was last drawn, indexed by `Pane`, for the mouse and page keys.
    areas: [Rect; 3],
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

fn main() {
    let arg = PathBuf::from(env::args().nth(1).unwrap_or_else(|| ".".into()));
    let root = fs::create_dir_all(&arg)
        .and_then(|()| arg.canonicalize())
        .unwrap_or_else(|e| die(&format!("{}: {e}", arg.display())));
    let config = Config::load(&root).unwrap_or_else(|e| die(&e));
    let mut app = App::new(root, config);
    app.refresh();
    let terminal = ratatui::init();
    // ponytail: mouse capture stops plain drag-to-select; most terminals still select with Shift or Option held
    execute!(stdout(), EnableMouseCapture).ok();
    let result = app.run(terminal);
    execute!(stdout(), DisableMouseCapture).ok();
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
    fn new(root: PathBuf, config: Config) -> App {
        let (tx, rx) = mpsc::channel();
        App {
            root,
            config,
            remotes: HashMap::new(),
            files: None,
            log: Vec::new(),
            busy: false,
            input: Input::None,
            playlists: ListState::default().with_selected(Some(0)),
            tracks: ListState::default(),
            focus: Pane::Playlists,
            log_scroll: 0,
            areas: [Rect::default(); 3],
            tx,
            rx,
        }
    }

    fn run(&mut self, mut terminal: DefaultTerminal) -> std::io::Result<()> {
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                self.handle(msg);
            }
            terminal.draw(|f| self.draw(f))?;
            if !event::poll(Duration::from_millis(200))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press && !self.key(key.code) => return Ok(()),
                Event::Mouse(m) => self.mouse(m),
                _ => {}
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
            Msg::File(f) => {
                // While a scan is running, that scan's result replaces `files` anyway.
                if let Some(files) = &mut self.files {
                    files.push(f);
                }
            }
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
                    if self.config.playlists.iter().any(|p| p.url == url) {
                        self.push_log(format!("already added: {url}"));
                    } else {
                        // Listed now, so the folder prompt can show the playlist title as its default.
                        self.fetch(url.clone());
                        self.input = Input::Dir { url, buf: String::new(), menu: Vec::new(), pick: None };
                    }
                }
                KeyCode::Esc => {}
                code => {
                    edit(&mut buf, code);
                    self.input = Input::Url(buf);
                }
            },
            Input::Dir { url, mut buf, mut menu, mut pick } => match code {
                KeyCode::Enter => self.add(url, buf.trim().trim_matches('/').to_string()),
                KeyCode::Esc if menu.is_empty() => {}
                // zsh style: the first Tab fills in what all matches share, the next ones cycle through them.
                KeyCode::Tab | KeyCode::BackTab => {
                    if menu.is_empty() {
                        menu = library::complete_dir(&self.root, &buf);
                        let shared = common_prefix(&menu);
                        if shared.len() > buf.len() {
                            buf = shared.to_string();
                        }
                        if menu.len() == 1 {
                            menu.clear();
                        }
                    } else {
                        let (n, back) = (menu.len(), code == KeyCode::BackTab);
                        let i = match pick {
                            None if back => n - 1,
                            None => 0,
                            Some(i) if back => (i + n - 1) % n,
                            Some(i) => (i + 1) % n,
                        };
                        pick = Some(i);
                        buf = menu[i].clone();
                    }
                    self.input = Input::Dir { url, buf, menu, pick };
                }
                code => {
                    // A completed folder ends in `/`, so typing `/` next to it just goes into that folder.
                    if !(code == KeyCode::Char('/') && buf.ends_with('/')) {
                        edit(&mut buf, code);
                    }
                    self.input = Input::Dir { url, buf, menu: Vec::new(), pick: None };
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
                KeyCode::Tab => self.focus = PANES[(self.focus as usize + 1) % 3],
                KeyCode::BackTab => self.focus = PANES[(self.focus as usize + 2) % 3],
                KeyCode::Down | KeyCode::Char('j') => self.scroll(self.focus, true, 1),
                KeyCode::Up | KeyCode::Char('k') => self.scroll(self.focus, false, 1),
                KeyCode::PageDown => self.scroll(self.focus, true, self.page()),
                KeyCode::PageUp => self.scroll(self.focus, false, self.page()),
                KeyCode::End | KeyCode::Char('G') => self.scroll(self.focus, true, u16::MAX),
                KeyCode::Home | KeyCode::Char('g') => self.scroll(self.focus, false, u16::MAX),
                _ => {}
            },
        }
        true
    }

    /// Clicking focuses a pane and picks the row under the pointer; the wheel scrolls the pane under it.
    fn mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        let Some(pane) = PANES.into_iter().find(|&p| self.areas[p as usize].contains(pos)) else { return };
        let area = self.areas[pane as usize];
        match m.kind {
            MouseEventKind::ScrollDown => self.scroll(pane, true, 3),
            MouseEventKind::ScrollUp => self.scroll(pane, false, 3),
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = pane;
                // Inside the border only.
                if m.row > area.y
                    && m.row + 1 < area.bottom()
                    && let Some(list) = self.list(pane)
                {
                    list.select(Some(list.offset() + (m.row - area.y - 1) as usize));
                }
            }
            _ => {}
        }
    }

    /// The list shown in `pane`. Moving through playlists starts the track list over from the top.
    fn list(&mut self, pane: Pane) -> Option<&mut ListState> {
        match pane {
            Pane::Playlists => {
                self.tracks = ListState::default();
                Some(&mut self.playlists)
            }
            Pane::Tracks => Some(&mut self.tracks),
            Pane::Log => None,
        }
    }

    /// Moves `n` rows. Lists clamp to their last row when drawn, the log in `draw`.
    fn scroll(&mut self, pane: Pane, down: bool, n: u16) {
        match self.list(pane) {
            Some(list) if down => list.scroll_down_by(n),
            Some(list) => list.scroll_up_by(n),
            None if down => self.log_scroll = self.log_scroll.saturating_sub(n.into()),
            None => self.log_scroll = self.log_scroll.saturating_add(n.into()),
        }
    }

    /// Rows inside the focused pane's border.
    fn page(&self) -> u16 {
        self.areas[self.focus as usize].height.saturating_sub(2).max(1)
    }

    fn selected(&self) -> Option<&Playlist> {
        self.playlists.selected().and_then(|i| self.config.playlists.get(i))
    }

    /// The playlist was listed when its URL was entered. A blank `dir` takes its title, now if the listing is in,
    /// otherwise when `Msg::Remote` arrives.
    fn add(&mut self, url: String, dir: String) {
        let mut p = Playlist { url, dir };
        if let Some(Ok(r)) = self.remotes.get(&p.url) {
            p.dir = library::dir_name(&p, r);
        }
        self.config.playlists.push(p);
        self.playlists.select(Some(self.config.playlists.len() - 1));
        self.save();
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
        // Scrolled up, the view stays on the same lines.
        if self.log_scroll > 0 {
            self.log_scroll += 1;
        }
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
        let shared: Vec<&Remote> = (self.config.playlists.iter())
            .filter(|o| o.url != p.url)
            .filter_map(|o| match self.remotes.get(&o.url) {
                Some(Ok(r)) if self.root.join(library::dir_name(o, r)) == dir => Some(r),
                _ => None,
            })
            .collect();
        Some(Ok(library::plan(remote, &shared, self.files.as_ref()?, &dir, &self.config.unavailable)))
    }

    fn draw(&mut self, f: &mut Frame) {
        let [main, log, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(10), Constraint::Length(1)]).areas(f.area());
        let [left, right] = Layout::horizontal([Constraint::Percentage(35), Constraint::Fill(1)]).areas(main);
        self.areas = [left, right, log];
        let focus = self.focus;
        let border = |pane: Pane| Style::new().fg(if pane == focus { Color::Cyan } else { Color::DarkGray });

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
            .block(Block::bordered().title(" playlists ").border_style(border(Pane::Playlists)));
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
        let block = Block::bordered().title(title).border_style(border(Pane::Tracks));
        match body {
            Ok(items) => f.render_stateful_widget(
                List::new(items).highlight_style(Style::new().reversed()).block(block),
                right,
                &mut self.tracks,
            ),
            Err(msg) => f.render_widget(Paragraph::new(msg).block(block), right),
        }

        let height = log.height.saturating_sub(2) as usize;
        self.log_scroll = self.log_scroll.min(self.log.len().saturating_sub(height));
        let end = self.log.len() - self.log_scroll;
        let lines: Vec<Line> =
            self.log[end.saturating_sub(height)..end].iter().map(|l| Line::from(l.as_str())).collect();
        let mut title = String::from(" log ");
        if self.busy {
            title += "· syncing… ";
        }
        if self.log_scroll > 0 {
            title += &format!("· {} newer, End to follow ", self.log_scroll);
        }
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title).border_style(border(Pane::Log))),
            log,
        );

        let help = match &self.input {
            Input::Url(buf) => format!("playlist URL: {buf}▏"),
            Input::Dir { url, buf, menu, pick } => {
                if !menu.is_empty() {
                    let height = (menu.len() as u16 + 2).min(main.height + log.height);
                    let area = Rect { y: footer.y - height, height, ..footer };
                    let items = List::new(menu.iter().map(String::as_str))
                        .highlight_style(Style::new().reversed())
                        .block(Block::bordered().title(" folders · tab next · shift+tab back "));
                    f.render_widget(Clear, area);
                    f.render_stateful_widget(items, area, &mut ListState::default().with_selected(*pick));
                }
                let title = match self.remotes.get(url) {
                    Some(Ok(r)) => library::dir_name(&Playlist { url: url.clone(), dir: String::new() }, r),
                    Some(Err(_)) => "playlist title".into(),
                    None => "playlist title, loading…".into(),
                };
                format!("folder in library (blank = {title}, tab completes): {buf}▏")
            }
            Input::None => format!(
                "a add · r refresh · s sync · S sync all · x remove · tab/click focus · ↑↓ pgup pgdn scroll · q quit    {}",
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

/// The longest start all `items` share, ignoring case, as spelled in the first.
fn common_prefix(items: &[String]) -> &str {
    let Some(first) = items.first() else { return "" };
    let same = |s: &String| {
        (first.char_indices().zip(s.chars()))
            .take_while(|((_, a), b)| a.to_lowercase().eq(b.to_lowercase()))
            .last()
            .map_or(0, |((i, a), _)| i + a.len_utf8())
    };
    &first[..items.iter().map(same).min().unwrap_or(0)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            assert!(app.key(KeyCode::Char(c)), "{c} quit");
        }
    }

    #[test]
    fn handles_keys() {
        let root = env::temp_dir().join(format!("trackman-test-{}-keys", process::id()));
        fs::create_dir_all(&root).unwrap();
        let url = "https://soundcloud.com/you/sets/warmup/s-abc";
        let config =
            Config { playlists: vec![Playlist { url: url.into(), dir: "warmup".into() }], ..Default::default() };
        let mut app = App::new(root.clone(), config);

        // The share link's tracking params are dropped, so pasting the same playlist again doesn't add it twice.
        press(&mut app, &format!("a{url}?si=q1"));
        app.key(KeyCode::Enter);
        app.key(KeyCode::Enter);
        assert_eq!(app.config.playlists.len(), 1);
        assert_eq!(app.log.last(), Some(&format!("already added: {url}")));

        press(&mut app, "aother");
        app.key(KeyCode::Esc);
        assert!(matches!(app.input, Input::None));

        // While a sync runs, q and x do nothing; Q still quits.
        app.busy = true;
        press(&mut app, "qx");
        assert_eq!(app.config.playlists.len(), 1);
        assert!(!app.key(KeyCode::Char('Q')));

        app.busy = false;
        press(&mut app, "x");
        assert!(Config::load(&root).unwrap().playlists.is_empty());
        assert!(!app.key(KeyCode::Char('q')));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn completes_folders() {
        let root = env::temp_dir().join(format!("trackman-test-{}-complete", process::id()));
        for d in ["warmup", "Workout", "sets/2026", ".hidden"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let mut app = App::new(root.clone(), Config::default());
        let dir = |app: &App| match &app.input {
            Input::Dir { buf, menu, .. } => (buf.clone(), menu.len()),
            _ => panic!("left the folder prompt"),
        };
        // Set directly: entering a URL would start a real scdl listing.
        app.input = Input::Dir { url: "u".into(), buf: String::new(), menu: Vec::new(), pick: None };

        press(&mut app, "w");
        app.key(KeyCode::Tab);
        assert_eq!(dir(&app), ("w".into(), 2));
        app.key(KeyCode::Tab);
        assert_eq!(dir(&app), ("warmup/".into(), 2));
        app.key(KeyCode::Tab);
        assert_eq!(dir(&app), ("Workout/".into(), 2));
        app.key(KeyCode::BackTab);
        assert_eq!(dir(&app), ("warmup/".into(), 2));
        // Typing closes the menu; `/` after a completed folder adds nothing.
        press(&mut app, "/");
        assert_eq!(dir(&app), ("warmup/".into(), 0));

        for _ in 0..7 {
            app.key(KeyCode::Backspace);
        }
        press(&mut app, "s");
        app.key(KeyCode::Tab);
        assert_eq!(dir(&app), ("sets/".into(), 0));
        app.key(KeyCode::Tab);
        assert_eq!(dir(&app), ("sets/2026/".into(), 0));

        // Esc closes an open menu first, then the prompt.
        app.input = Input::Dir { url: "u".into(), buf: String::new(), menu: vec!["a/".into()], pick: None };
        app.key(KeyCode::Esc);
        assert_eq!(dir(&app), (String::new(), 0));
        app.key(KeyCode::Esc);
        assert!(matches!(app.input, Input::None));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scrolls_and_draws() {
        let mut app = App::new(env::temp_dir(), Config::default());
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        for i in 0..50 {
            app.push_log(format!("line {i}"));
        }
        app.key(KeyCode::Tab);
        app.key(KeyCode::Tab);
        assert!(app.focus == Pane::Log);
        app.key(KeyCode::Up);
        app.push_log("line 50".into());
        assert_eq!(app.log_scroll, 2);
        // Scrolling past the top stops at the oldest line.
        app.key(KeyCode::Home);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.log_scroll, 51 - 8);
        app.key(KeyCode::End);
        assert_eq!(app.log_scroll, 0);
        app.key(KeyCode::BackTab);
        assert!(app.focus == Pane::Tracks);

        app.input = Input::Dir { url: "u".into(), buf: "w".into(), menu: vec!["warmup/".into()], pick: Some(0) };
        terminal.draw(|f| app.draw(f)).unwrap();
    }

    #[test]
    fn summarises_rows() {
        let row = |status| Row { status, name: String::new() };
        let rows =
            [row(Status::Have), row(Status::Have), row(Status::New), row(Status::Link("x".into())), row(Status::Extra)];
        let text: String = summary(&rows).iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " 2/4 1 new 1 link 1 extra");
    }
}
