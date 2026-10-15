// `term-flashy attach`: drive term-flashy tabs from another terminal
// (typically an SSH session). Starts on a menu listing the tabs of every
// running term-flashy; inside a tab, Ctrl+] brings the menu back.
//
// The tab's output is replayed through a local vt100 screen and only the
// resulting screen diffs reach the real terminal, so escape sequences the
// parser doesn't model (clipboard writes, titles, ...) never get through.
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::termios;
use unicode_width::UnicodeWidthChar;

use crate::attach_ipc::{self, FRAME_INPUT, FRAME_OUTPUT, FRAME_REJECT, FRAME_RESIZE, FRAME_SIZE};

const CTRL_BRACKET: u8 = 0x1d;
const REFRESH: Duration = Duration::from_secs(1);
const ESC_TIMEOUT: Duration = Duration::from_millis(40);

/// Puts the terminal back as it was, whatever modes the tab turned on.
const RESET_MODES: &[u8] =
    b"\x1b[?1l\x1b>\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[0m";
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1006h";

const KEYS_1: &[(&str, &str)] = &[("Enter", "entrar"), ("c", "nova"), ("r", "nome"), ("x", "fechar")];
const KEYS_2: &[(&str, &str)] = &[("/", "filtrar"), ("Esc", "voltar"), ("?", "ajuda"), ("q", "sair")];

const HELP_SCREEN: &[(&str, &str)] = &[
    ("↑↓ PgUp PgDn", "rolar a lista"),
    ("Home End", "início / fim"),
    ("Enter ou toque", "entrar na aba"),
    ("/", "filtrar pelo nome"),
    ("c", "nova aba (mesma pasta)"),
    ("r", "renomear a aba"),
    ("x", "fechar a aba"),
    ("n  p", "próxima / anterior aba"),
    ("Esc", "voltar à aba atual"),
    ("q", "sair do attach"),
    ("Ctrl+]", "na aba: abre este menu"),
    ("Ctrl+] 2x", "envia Ctrl+] para a aba"),
    ("", ""),
    ("●", "aba pedindo atenção"),
    ("⇄", "outro terminal conectado"),
    ("✕", "bloqueada para attach"),
];

#[derive(Clone)]
struct TabInfo {
    pid: i64,
    window: i64,
    title: String,
    notify: bool,
    socket: Option<PathBuf>,
    blocked: bool,
    clients: u64,
    /// Window number across every running term-flashy, from 1.
    group: usize,
    /// Control socket of the term-flashy process owning the tab.
    control: PathBuf,
}

impl TabInfo {
    fn key(&self) -> String {
        match &self.socket {
            Some(socket) => socket.to_string_lossy().into_owned(),
            None => format!("{}/{}/{}", self.pid, self.window, self.title),
        }
    }
}

#[derive(Default)]
struct Bells(usize);

impl vt100::Callbacks for Bells {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.0 += 1;
    }
}

struct Conn {
    stream: UnixStream,
    decoder: attach_ipc::FrameDecoder,
    parser: vt100::Parser<Bells>,
    /// Last screen drawn to the terminal; `None` = repaint everything.
    drawn: Option<vt100::Screen>,
    socket: PathBuf,
    title: String,
}

enum Line {
    Header(String),
    Tab(usize),
}

enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Backspace,
    CtrlBracket,
    Char(char),
    Wheel(i32),
    Click(u16),
    Other,
}

enum Mode {
    Normal,
    Filter,
    Rename(String),
    ConfirmClose,
    Help,
}

struct App {
    dir: PathBuf,
    out: File,
    size: (u16, u16),
    conn: Option<Conn>,
    in_menu: bool,
    tabs: Vec<TabInfo>,
    selected_key: Option<String>,
    scroll: usize,
    filter: String,
    mode: Mode,
    message: String,
    /// Screen row (1-based) of the first list line in the last render.
    list_top: u16,
    lines_shown: Vec<Option<usize>>,
    quit: bool,
}

struct TerminalGuard {
    saved: termios::Termios,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = unsafe { std::mem::ManuallyDrop::new(File::from_raw_fd(1)) };
        let _ = out.write_all(RESET_MODES);
        let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
        let _ = termios::tcsetattr(std::io::stdin().as_fd(), termios::SetArg::TCSANOW, &self.saved);
    }
}

pub fn run() -> ! {
    let stdin = std::io::stdin();
    let Ok(saved) = termios::tcgetattr(stdin.as_fd()) else {
        eprintln!("term-flashy attach: needs to run in a terminal");
        std::process::exit(1);
    };
    let dir = match attach_ipc::socket_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("term-flashy attach: {err}");
            std::process::exit(1);
        }
    };
    let (signal_read, signal_write) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("term-flashy attach: {err}");
            std::process::exit(1);
        }
    };
    let _ = signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, signal_write);
    let _ = signal_read.set_nonblocking(true);

    let guard = TerminalGuard { saved: saved.clone() };
    let mut raw = saved;
    termios::cfmakeraw(&mut raw);
    let _ = termios::tcsetattr(stdin.as_fd(), termios::SetArg::TCSANOW, &raw);

    let out = unsafe { File::from_raw_fd(nix::libc::dup(1)) };
    let mut app = App {
        dir,
        out,
        size: attach_ipc::get_winsize(1).unwrap_or((24, 80)),
        conn: None,
        in_menu: true,
        tabs: Vec::new(),
        selected_key: None,
        scroll: 0,
        filter: String::new(),
        mode: Mode::Normal,
        message: String::new(),
        list_top: 1,
        lines_shown: Vec::new(),
        quit: false,
    };
    app.write(b"\x1b[?1049h");
    app.open_menu();
    app.event_loop(&signal_read);
    drop(app);
    drop(guard);
    std::process::exit(0);
}

impl App {
    fn write(&mut self, bytes: &[u8]) {
        let _ = self.out.write_all(bytes);
    }

    fn event_loop(&mut self, signal_read: &UnixStream) {
        let mut stdin = unsafe { std::mem::ManuallyDrop::new(File::from_raw_fd(0)) };
        let mut pending: Vec<u8> = Vec::new();
        let mut last_refresh = std::time::Instant::now();
        let mut buf = vec![0u8; 64 * 1024];
        while !self.quit {
            let lone_esc = self.in_menu && pending == [0x1b];
            let timeout = if lone_esc {
                ESC_TIMEOUT
            } else if self.in_menu {
                REFRESH.saturating_sub(last_refresh.elapsed())
            } else {
                Duration::from_secs(3600)
            };
            let conn_fd = self.conn.as_ref().map(|c| c.stream.as_raw_fd());
            let stdin_fd = std::io::stdin();
            let (stdin_ready, signal_ready, conn_ready) = {
                let conn_borrow = conn_fd.map(|fd| unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) });
                let mut fds = vec![
                    PollFd::new(stdin_fd.as_fd(), PollFlags::POLLIN),
                    PollFd::new(signal_read.as_fd(), PollFlags::POLLIN),
                ];
                if let Some(fd) = &conn_borrow {
                    fds.push(PollFd::new(fd.as_fd(), PollFlags::POLLIN));
                }
                let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
                match poll(&mut fds, PollTimeout::try_from(ms).unwrap_or(PollTimeout::MAX)) {
                    Ok(_) => {}
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(_) => break,
                }
                let ready = |fd: &PollFd| fd.revents().is_some_and(|r| !r.is_empty());
                (
                    ready(&fds[0]),
                    ready(&fds[1]),
                    fds.get(2).is_some_and(ready),
                )
            };

            if signal_ready {
                let mut drain = [0u8; 64];
                while (&*signal_read).read(&mut drain).is_ok_and(|n| n > 0) {}
                self.on_resize();
            }
            if conn_ready {
                self.on_conn_readable(&mut buf);
            }
            if stdin_ready {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => pending.extend_from_slice(&buf[..n]),
                }
            } else if lone_esc && pending == [0x1b] {
                pending.clear();
                self.on_key(Key::Esc);
            }
            self.process_input(&mut pending);
            if self.in_menu && last_refresh.elapsed() >= REFRESH {
                last_refresh = std::time::Instant::now();
                self.refresh_tabs();
                self.render_menu();
            }
        }
    }

    fn process_input(&mut self, pending: &mut Vec<u8>) {
        while !pending.is_empty() && !self.quit {
            if self.in_menu {
                if pending == &[0x1b] {
                    return; // maybe the start of a sequence; wait a bit
                }
                match parse_key(pending) {
                    Some((key, used)) => {
                        pending.drain(..used);
                        self.on_key(key);
                    }
                    None => return,
                }
            } else {
                match pending.iter().position(|&b| b == CTRL_BRACKET) {
                    Some(pos) => {
                        let before: Vec<u8> = pending.drain(..=pos).take(pos).collect();
                        self.send_input(&before);
                        self.open_menu();
                    }
                    None => {
                        let data = std::mem::take(pending);
                        self.send_input(&data);
                    }
                }
            }
        }
    }

    fn send_input(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Some(conn) = &mut self.conn
            && attach_ipc::write_frame(&mut conn.stream, FRAME_INPUT, data).is_err()
        {
            self.lost_connection("a conexão com a aba caiu");
        }
    }

    fn on_resize(&mut self) {
        if let Some(size) = attach_ipc::get_winsize(1) {
            self.size = size;
        }
        if let Some(conn) = &mut self.conn {
            let _ = attach_ipc::write_frame(
                &mut conn.stream,
                FRAME_RESIZE,
                &attach_ipc::encode_size(self.size.0, self.size.1),
            );
            conn.drawn = None;
        }
        if self.in_menu {
            self.write(b"\x1b[2J");
            self.render_menu();
        } else {
            self.render_tab();
        }
    }

    fn on_conn_readable(&mut self, buf: &mut [u8]) {
        let Some(conn) = &mut self.conn else { return };
        let n = match (&conn.stream).read(buf) {
            Ok(0) | Err(_) => {
                let title = conn.title.clone();
                self.lost_connection(&format!("a aba \"{title}\" foi encerrada ou desconectada"));
                return;
            }
            Ok(n) => n,
        };
        conn.decoder.push(&buf[..n]);
        loop {
            match conn.decoder.next_frame() {
                Ok(Some((FRAME_OUTPUT, payload))) => conn.parser.process(&payload),
                Ok(Some((FRAME_SIZE, payload))) => {
                    if let Some((rows, cols)) = attach_ipc::decode_size(&payload) {
                        conn.parser.screen_mut().set_size(rows, cols);
                    }
                }
                Ok(Some((FRAME_REJECT, payload))) => {
                    let reason = attach_ipc::sanitize(&String::from_utf8_lossy(&payload), 100);
                    self.lost_connection(&reason);
                    return;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    self.lost_connection("resposta inválida da aba");
                    return;
                }
            }
        }
        if !self.in_menu {
            self.render_tab();
        }
    }

    fn lost_connection(&mut self, message: &str) {
        self.conn = None;
        self.message = message.to_string();
        self.open_menu();
    }

    fn render_tab(&mut self) {
        let Some(conn) = &mut self.conn else { return };
        let screen = conn.parser.screen();
        let mut bytes = Vec::new();
        match &conn.drawn {
            Some(prev) => bytes.extend_from_slice(&screen.state_diff(prev)),
            None => {
                bytes.extend_from_slice(RESET_MODES);
                bytes.extend_from_slice(&screen.state_formatted());
            }
        }
        let bells = std::mem::take(&mut conn.parser.callbacks_mut().0);
        if bells > 0 {
            bytes.push(0x07);
        }
        conn.drawn = Some(conn.parser.screen().clone());
        self.write(&bytes);
    }

    fn open_menu(&mut self) {
        self.in_menu = true;
        self.mode = Mode::Normal;
        if let Some(conn) = &self.conn {
            self.selected_key = Some(conn.socket.to_string_lossy().into_owned());
        }
        self.write(RESET_MODES);
        self.write(MOUSE_ON);
        self.write(b"\x1b[?25l\x1b[2J");
        self.refresh_tabs();
        self.render_menu();
    }

    fn back_to_tab(&mut self) {
        let Some(conn) = &mut self.conn else { return };
        conn.drawn = None;
        self.in_menu = false;
        self.mode = Mode::Normal;
        self.message.clear();
        self.write(RESET_MODES);
        self.write(b"\x1b[?25h");
        self.render_tab();
    }

    fn refresh_tabs(&mut self) {
        self.tabs = list_tabs(&self.dir);
        let visible = self.visible_tabs();
        let selected_ok = self
            .selected_key
            .as_ref()
            .is_some_and(|key| visible.iter().any(|&i| &self.tabs[i].key() == key));
        if !selected_ok {
            self.selected_key = visible.first().map(|&i| self.tabs[i].key());
        }
    }

    fn visible_tabs(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        (0..self.tabs.len())
            .filter(|&i| filter.is_empty() || self.tabs[i].title.to_lowercase().contains(&filter))
            .collect()
    }

    fn selected_index(&self, visible: &[usize]) -> Option<usize> {
        let key = self.selected_key.as_ref()?;
        visible.iter().position(|&i| &self.tabs[i].key() == key)
    }

    fn selected_tab(&self) -> Option<TabInfo> {
        let visible = self.visible_tabs();
        self.selected_index(&visible).map(|i| self.tabs[visible[i]].clone())
    }

    fn move_selection(&mut self, delta: i64) {
        let visible = self.visible_tabs();
        if visible.is_empty() {
            return;
        }
        let current = self.selected_index(&visible).unwrap_or(0) as i64;
        let next = (current + delta).clamp(0, visible.len() as i64 - 1) as usize;
        self.selected_key = Some(self.tabs[visible[next]].key());
    }

    /// Lines available for the tab list: title + blank above, 3 footer lines.
    fn list_rows(&self) -> usize {
        (self.size.0 as usize).saturating_sub(5).max(1)
    }

    fn on_key(&mut self, key: Key) {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.on_key_normal(key),
            Mode::Help => {
                if !matches!(key, Key::Esc | Key::Char('?') | Key::Char('q') | Key::Enter) {
                    self.mode = Mode::Help;
                }
            }
            Mode::Filter => self.on_key_filter(key),
            Mode::Rename(text) => self.on_key_rename(key, text),
            Mode::ConfirmClose => match key {
                Key::Char('s' | 'S' | 'y' | 'Y') => self.close_selected(),
                _ => self.message = "nada foi fechado".to_string(),
            },
        }
        if self.in_menu && !self.quit {
            self.render_menu();
        }
    }

    fn on_key_normal(&mut self, key: Key) {
        self.message.clear();
        let page = self.list_rows() as i64;
        match key {
            Key::Up | Key::Char('k') => self.move_selection(-1),
            Key::Down | Key::Char('j') => self.move_selection(1),
            Key::PageUp => self.move_selection(-page),
            Key::PageDown => self.move_selection(page),
            Key::Home | Key::Char('g') => self.move_selection(i64::MIN / 2),
            Key::End | Key::Char('G') => self.move_selection(i64::MAX / 2),
            Key::Wheel(delta) => self.move_selection(delta as i64 * 3),
            Key::Click(row) => {
                let index = row.checked_sub(self.list_top).map(usize::from);
                if let Some(Some(tab)) = index.and_then(|i| self.lines_shown.get(i)).cloned() {
                    self.selected_key = Some(self.tabs[tab].key());
                    self.enter_selected();
                }
            }
            Key::Enter => self.enter_selected(),
            Key::Char('/') => self.mode = Mode::Filter,
            Key::Char('?') => self.mode = Mode::Help,
            Key::Char('c') => self.new_tab(),
            Key::Char('r') => {
                if let Some(tab) = self.actionable_selected() {
                    self.mode = Mode::Rename(tab.title);
                }
            }
            Key::Char('x') => {
                if self.actionable_selected().is_some() {
                    self.mode = Mode::ConfirmClose;
                }
            }
            Key::Char('n') => self.step_tab(1),
            Key::Char('p') => self.step_tab(-1),
            Key::Char('q') => self.quit = true,
            Key::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refresh_selection();
                } else if self.conn.is_some() {
                    self.back_to_tab();
                } else {
                    self.message = "nenhuma aba aberta: Enter entra, q sai".to_string();
                }
            }
            Key::CtrlBracket if self.conn.is_some() => {
                self.back_to_tab();
                self.send_input(&[CTRL_BRACKET]);
            }
            _ => {}
        }
    }

    fn on_key_filter(&mut self, key: Key) {
        match key {
            Key::Char(c) => {
                self.filter.push(c);
                self.selected_key = None;
                self.refresh_selection();
                self.mode = Mode::Filter;
            }
            Key::Backspace => {
                self.filter.pop();
                self.refresh_selection();
                self.mode = Mode::Filter;
            }
            Key::Esc => {
                self.filter.clear();
                self.refresh_selection();
            }
            Key::Enter => self.enter_selected(),
            Key::Up => {
                self.move_selection(-1);
                self.mode = Mode::Filter;
            }
            Key::Down => {
                self.move_selection(1);
                self.mode = Mode::Filter;
            }
            _ => self.mode = Mode::Filter,
        }
    }

    fn on_key_rename(&mut self, key: Key, mut text: String) {
        match key {
            Key::Char(c) => {
                if text.chars().count() < 100 {
                    text.push(c);
                }
                self.mode = Mode::Rename(text);
            }
            Key::Backspace => {
                text.pop();
                self.mode = Mode::Rename(text);
            }
            Key::Enter => self.rename_selected(&text),
            Key::Esc => self.message = "nome mantido".to_string(),
            _ => self.mode = Mode::Rename(text),
        }
    }

    fn refresh_selection(&mut self) {
        let visible = self.visible_tabs();
        if self.selected_index(&visible).is_none() {
            self.selected_key = visible.first().map(|&i| self.tabs[i].key());
        }
        self.scroll = 0;
    }

    /// Selected tab, if `c`/`r`/`x` may act on it; sets a message otherwise.
    fn actionable_selected(&mut self) -> Option<TabInfo> {
        let tab = self.selected_tab()?;
        if tab.blocked {
            self.message = "esta aba está bloqueada para attach".to_string();
            return None;
        }
        if tab.socket.is_none() {
            self.message = "esta aba não aceita attach".to_string();
            return None;
        }
        Some(tab)
    }

    fn tab_command(&mut self, tab: &TabInfo, mut command: serde_json::Value) -> Option<serde_json::Value> {
        command["socket"] = serde_json::json!(tab.socket);
        match control_request(&tab.control, &command) {
            Ok(reply) => Some(reply),
            Err(err) => {
                self.message = err;
                None
            }
        }
    }

    /// Opens a tab next to the selected one, in its folder, and enters it.
    fn new_tab(&mut self) {
        let Some(tab) = self.actionable_selected() else { return };
        let Some(reply) = self.tab_command(&tab, serde_json::json!({"cmd": "new"})) else {
            return;
        };
        let Some(socket) = reply["socket"].as_str().map(PathBuf::from) else {
            self.message = "o term-flashy não devolveu a aba nova".to_string();
            return;
        };
        // The new tab's pty-host needs a moment to start listening.
        let mut stream = None;
        for _ in 0..50 {
            if let Ok(s) = connect(&socket, self.size) {
                stream = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.refresh_tabs();
        self.selected_key = Some(socket.to_string_lossy().into_owned());
        let title = self
            .tabs
            .iter()
            .find(|t| t.socket.as_ref() == Some(&socket))
            .map_or_else(|| "nova aba".to_string(), |t| t.title.clone());
        match stream {
            Some(stream) => self.start_conn(stream, socket, title),
            None => self.message = "aba criada, mas não respondeu ainda".to_string(),
        }
    }

    fn rename_selected(&mut self, text: &str) {
        let Some(tab) = self.actionable_selected() else { return };
        let command = serde_json::json!({"cmd": "rename", "title": text});
        if self.tab_command(&tab, command).is_some() {
            self.refresh_tabs();
            if let Some(conn) = &mut self.conn
                && Some(&conn.socket) == tab.socket.as_ref()
            {
                conn.title = text.trim().to_string();
            }
            self.message = "aba renomeada".to_string();
        }
    }

    fn close_selected(&mut self) {
        let Some(tab) = self.actionable_selected() else { return };
        // Keep the selection where it was: on the next tab, or the previous one.
        let visible = self.visible_tabs();
        let neighbor = self.selected_index(&visible).and_then(|i| {
            visible
                .get(i + 1)
                .or_else(|| i.checked_sub(1).and_then(|p| visible.get(p)))
                .map(|&t| self.tabs[t].key())
        });
        if self.tab_command(&tab, serde_json::json!({"cmd": "close"})).is_some() {
            if self.conn.as_ref().is_some_and(|c| Some(&c.socket) == tab.socket.as_ref()) {
                self.conn = None;
            }
            self.selected_key = neighbor;
            self.refresh_tabs();
            self.message = format!("aba \"{}\" fechada", tab.title);
        }
    }

    /// Opens the next/previous attachable tab after the current one.
    fn step_tab(&mut self, delta: i64) {
        let candidates: Vec<usize> = (0..self.tabs.len())
            .filter(|&i| self.tabs[i].socket.is_some() && !self.tabs[i].blocked)
            .collect();
        if candidates.is_empty() {
            self.message = "nenhuma aba aceita attach".to_string();
            return;
        }
        let reference = self
            .conn
            .as_ref()
            .map(|c| c.socket.to_string_lossy().into_owned())
            .or_else(|| self.selected_key.clone());
        let position = reference.and_then(|key| candidates.iter().position(|&i| self.tabs[i].key() == key));
        let len = candidates.len() as i64;
        let next = match position {
            Some(p) => (p as i64 + delta).rem_euclid(len) as usize,
            None => 0,
        };
        // n/p walk every tab, so leave any filter behind.
        self.filter.clear();
        self.selected_key = Some(self.tabs[candidates[next]].key());
        self.enter_tab(candidates[next]);
    }

    fn enter_selected(&mut self) {
        let visible = self.visible_tabs();
        if let Some(index) = self.selected_index(&visible) {
            self.enter_tab(visible[index]);
        }
    }

    fn enter_tab(&mut self, tab_index: usize) {
        let tab = self.tabs[tab_index].clone();
        let Some(socket) = tab.socket.clone() else {
            self.message = "esta aba não aceita attach".to_string();
            return;
        };
        if tab.blocked {
            self.message = "esta aba está bloqueada para attach".to_string();
            return;
        }
        if self.conn.as_ref().is_some_and(|c| c.socket == socket) {
            self.back_to_tab();
            return;
        }
        self.conn = None;
        match connect(&socket, self.size) {
            Ok(stream) => self.start_conn(stream, socket, tab.title.clone()),
            Err(err) => self.message = format!("não foi possível conectar: {err}"),
        }
    }

    fn start_conn(&mut self, stream: UnixStream, socket: PathBuf, title: String) {
        self.conn = Some(Conn {
            stream,
            decoder: attach_ipc::FrameDecoder::default(),
            parser: vt100::Parser::new_with_callbacks(self.size.0, self.size.1, 0, Bells::default()),
            drawn: None,
            socket,
            title,
        });
        self.back_to_tab();
    }

    fn render_menu(&mut self) {
        let (rows, cols) = (self.size.0 as usize, self.size.1 as usize);
        let mut screen: Vec<String> = Vec::new();

        if matches!(self.mode, Mode::Help) {
            screen.push(spread(&bold(" Ajuda"), &dim("Esc fecha "), cols));
            screen.push(String::new());
            for (keys, text) in HELP_SCREEN {
                if keys.is_empty() {
                    screen.push(String::new());
                    continue;
                }
                let keys = format!(" {keys:<15}");
                screen.push(format!(
                    "{}{}",
                    bold(&keys),
                    dim(&fit(text, cols.saturating_sub(text_width(&keys))))
                ));
            }
            self.lines_shown.clear();
            self.draw(screen, rows);
            return;
        }

        let visible = self.visible_tabs();
        let selected = self.selected_index(&visible);
        let multi_window = self.tabs.iter().map(|t| t.group).max().unwrap_or(0) > 1;

        let mut lines: Vec<Line> = Vec::new();
        let mut selected_line = None;
        let mut last_group = None;
        for (pos, &i) in visible.iter().enumerate() {
            let group = self.tabs[i].group;
            if multi_window && last_group != Some(group) {
                lines.push(Line::Header(format!("JANELA {group}")));
            }
            last_group = Some(group);
            if Some(pos) == selected {
                selected_line = Some(lines.len());
            }
            lines.push(Line::Tab(i));
        }

        let list_rows = self.list_rows();
        if let Some(line) = selected_line {
            if line < self.scroll {
                // keep the window header visible when possible
                self.scroll = line.saturating_sub(1);
            } else if line >= self.scroll + list_rows {
                self.scroll = line + 1 - list_rows;
            }
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(list_rows));

        let current_title = self.conn.as_ref().map(|c| c.title.clone());
        let right = match &current_title {
            Some(title) => format!("atual: {} ", fit(title, cols.saturating_sub(20))),
            None => "nenhuma aba aberta ".to_string(),
        };
        screen.push(spread(&bold(" term-flashy"), &dim(&right), cols));
        screen.push(String::new());

        self.list_top = screen.len() as u16 + 1;
        self.lines_shown.clear();
        let current = self.conn.as_ref().map(|c| c.socket.clone());
        for line in lines.iter().skip(self.scroll).take(list_rows) {
            match line {
                Line::Header(text) => {
                    self.lines_shown.push(None);
                    screen.push(format!("\x1b[1;36m {text}\x1b[0m"));
                }
                Line::Tab(i) => {
                    self.lines_shown.push(Some(*i));
                    let tab = &self.tabs[*i];
                    screen.push(tab_line(tab, Some(tab.key()) == self.selected_key, tab.socket == current && current.is_some(), cols));
                }
            }
        }
        if visible.is_empty() {
            let text = if self.tabs.is_empty() {
                "nenhum term-flashy com attach ligado (Settings → attach)"
            } else {
                "nenhuma aba com esse nome"
            };
            screen.push(dim(&format!(" {}", fit(text, cols.saturating_sub(1)))));
        }
        while screen.len() < rows.saturating_sub(3) {
            screen.push(String::new());
        }
        screen.truncate(rows.saturating_sub(3));

        let position = format!("{}/{} ", selected.map_or(0, |p| p + 1), visible.len());
        let (status, keys1, keys2): (String, &[(&str, &str)], &[(&str, &str)]) = match &self.mode {
            Mode::Filter => (
                format!(" /{}▏", self.filter),
                &[("Enter", "entrar"), ("↑↓", "mover")],
                &[("Esc", "limpar filtro")],
            ),
            Mode::Rename(text) => (
                format!(" nome: {text}▏"),
                &[("Enter", "salvar")],
                &[("Esc", "cancelar")],
            ),
            Mode::ConfirmClose => {
                let title = self.selected_tab().map(|t| t.title).unwrap_or_default();
                (
                    format!("\x1b[33m fechar \"{}\"?\x1b[0m", fit(&title, cols.saturating_sub(12))),
                    &[("s", "sim, fechar a aba")],
                    &[("n", "não"), ("Esc", "cancelar")],
                )
            }
            _ => {
                let status = if !self.message.is_empty() {
                    format!("\x1b[33m {}\x1b[0m", fit(&self.message, cols.saturating_sub(position.len() + 2)))
                } else if !self.filter.is_empty() {
                    dim(&format!(" filtro: {}", fit(&self.filter, cols.saturating_sub(position.len() + 10))))
                } else {
                    String::new()
                };
                (status, KEYS_1, KEYS_2)
            }
        };
        screen.push(spread(&status, &dim(&position), cols));
        screen.push(key_bar(keys1));
        screen.push(key_bar(keys2));
        self.draw(screen, rows);
    }

    fn draw(&mut self, mut screen: Vec<String>, rows: usize) {
        while screen.len() < rows {
            screen.push(String::new());
        }
        screen.truncate(rows);
        let mut bytes = Vec::new();
        for (row, text) in screen.iter().enumerate() {
            bytes.extend_from_slice(format!("\x1b[{};1H\x1b[0m{text}\x1b[0m\x1b[K", row + 1).as_bytes());
        }
        self.write(&bytes);
    }
}

/// One tab row: status column, name, tag on the right; selected = inverse bar.
fn tab_line(tab: &TabInfo, selected: bool, current: bool, cols: usize) -> String {
    let unavailable = tab.socket.is_none() || tab.blocked;
    let (mark, mark_color) = if tab.notify {
        ("●", "\x1b[33m")
    } else if tab.blocked {
        ("✕", "\x1b[2m")
    } else {
        (" ", "")
    };
    let tag = if current {
        "atual"
    } else if tab.blocked {
        "bloqueada"
    } else if tab.socket.is_none() {
        "sem attach"
    } else if tab.clients > 0 {
        "⇄ conectada"
    } else {
        ""
    };
    let name = fit(&tab.title, cols.saturating_sub(5 + text_width(tag)));
    let gap = cols.saturating_sub(4 + text_width(&name) + text_width(tag));
    if selected {
        return format!("\x1b[7m {mark} {name}{}{tag} \x1b[0m", " ".repeat(gap));
    }
    let style = if unavailable { "\x1b[2m" } else { "" };
    format!(
        " {mark_color}{mark}\x1b[0m {style}{name}\x1b[0m{}{}",
        " ".repeat(gap),
        dim(&format!("{tag} "))
    )
}

fn bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[0m")
}

fn dim(text: &str) -> String {
    format!("\x1b[2m{text}\x1b[0m")
}

/// Visible width of text that may contain SGR escape sequences.
fn styled_width(text: &str) -> usize {
    let mut width = 0;
    let mut in_escape = false;
    for c in text.chars() {
        match (in_escape, c) {
            (false, '\x1b') => in_escape = true,
            (true, 'm') => in_escape = false,
            (true, _) => {}
            (false, c) => width += c.width().unwrap_or(0),
        }
    }
    width
}

/// `left` and `right` on one line, `right` flush to the edge.
fn spread(left: &str, right: &str, cols: usize) -> String {
    let gap = cols.saturating_sub(styled_width(left) + styled_width(right));
    format!("{left}{}{right}", " ".repeat(gap))
}

fn key_bar(keys: &[(&str, &str)]) -> String {
    let mut line = String::from(" ");
    for (i, (key, text)) in keys.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(&format!("\x1b[1;97;48;5;238m{key}\x1b[0m \x1b[2m{text}\x1b[0m"));
    }
    line
}

fn connect(socket: &Path, size: (u16, u16)) -> std::io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)?;
    if !attach_ipc::peer_is_same_user(&stream) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "socket de outro usuário",
        ));
    }
    attach_ipc::write_frame(&mut stream, FRAME_RESIZE, &attach_ipc::encode_size(size.0, size.1))?;
    Ok(stream)
}

/// Tabs of every running term-flashy that has attach turned on.
fn list_tabs(dir: &Path) -> Vec<TabInfo> {
    attach_ipc::remove_stale_sockets(dir);
    let mut controls: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("control-") && n.ends_with(".sock"))
                })
                .collect()
        })
        .unwrap_or_default();
    controls.sort();

    let mut tabs = Vec::new();
    for path in controls {
        let Some(reply) = control_request(&path, &serde_json::json!({"cmd": "list"})).ok() else {
            continue;
        };
        let pid = reply["pid"].as_i64().unwrap_or(0);
        for window in reply["windows"].as_array().into_iter().flatten() {
            let window_no = window["window"].as_i64().unwrap_or(0);
            for tab in window["tabs"].as_array().into_iter().flatten() {
                tabs.push(TabInfo {
                    pid,
                    window: window_no,
                    title: attach_ipc::sanitize(tab["title"].as_str().unwrap_or("?"), 200),
                    notify: tab["notify"] == true,
                    socket: tab["socket"].as_str().map(PathBuf::from),
                    blocked: tab["blocked"] == true,
                    clients: tab["clients"].as_u64().unwrap_or(0),
                    group: 0,
                    control: path.clone(),
                });
            }
        }
    }
    tabs.sort_by_key(|t| (t.pid, t.window));
    let mut group = 0;
    let mut last = None;
    for tab in &mut tabs {
        if last != Some((tab.pid, tab.window)) {
            group += 1;
            last = Some((tab.pid, tab.window));
        }
        tab.group = group;
    }
    tabs
}

/// One request/reply on a term-flashy control socket.
fn control_request(path: &Path, command: &serde_json::Value) -> Result<serde_json::Value, String> {
    let failed = |err: std::io::Error| format!("term-flashy não respondeu: {err}");
    let mut stream = UnixStream::connect(path).map_err(failed)?;
    if !attach_ipc::peer_is_same_user(&stream) {
        return Err("socket de outro usuário".to_string());
    }
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(failed)?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).map_err(failed)?;
    stream.write_all(format!("{command}\n").as_bytes()).map_err(failed)?;
    let mut line = String::new();
    BufReader::new(stream.take(4 * 1024 * 1024)).read_line(&mut line).map_err(failed)?;
    let reply: serde_json::Value =
        serde_json::from_str(&line).map_err(|_| "resposta inválida do term-flashy".to_string())?;
    if reply["ok"] == false {
        let error = reply["error"].as_str().unwrap_or("o term-flashy recusou o pedido");
        return Err(attach_ipc::sanitize(error, 100));
    }
    Ok(reply)
}

fn text_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Cuts `text` to `width` terminal columns, marking the cut with "…".
fn fit(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// Parses one key from the start of `buf`; `None` = incomplete sequence.
fn parse_key(buf: &[u8]) -> Option<(Key, usize)> {
    let first = *buf.first()?;
    match first {
        b'\r' | b'\n' => Some((Key::Enter, 1)),
        0x7f | 0x08 => Some((Key::Backspace, 1)),
        CTRL_BRACKET => Some((Key::CtrlBracket, 1)),
        0x03 => Some((Key::Esc, 1)),
        0x1b => parse_escape(buf),
        0x00..=0x1f => Some((Key::Other, 1)),
        _ => {
            let len = match first {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            if buf.len() < len {
                return None;
            }
            let key = std::str::from_utf8(&buf[..len])
                .ok()
                .and_then(|s| s.chars().next())
                .map_or(Key::Other, Key::Char);
            Some((key, len))
        }
    }
}

fn parse_escape(buf: &[u8]) -> Option<(Key, usize)> {
    let second = *buf.get(1)?;
    match second {
        b'[' => {
            let end = buf[2..].iter().position(|b| (0x40..=0x7e).contains(b))? + 2;
            let params = &buf[2..end];
            let final_byte = buf[end];
            let key = match (params, final_byte) {
                (b"", b'A') => Key::Up,
                (b"", b'B') => Key::Down,
                (b"", b'H') | (b"1", b'~') | (b"7", b'~') => Key::Home,
                (b"", b'F') | (b"4", b'~') | (b"8", b'~') => Key::End,
                (b"5", b'~') => Key::PageUp,
                (b"6", b'~') => Key::PageDown,
                (p, b'M') if p.starts_with(b"<") => parse_mouse(&p[1..]),
                _ => Key::Other,
            };
            Some((key, end + 1))
        }
        b'O' => {
            let third = *buf.get(2)?;
            let key = match third {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'H' => Key::Home,
                b'F' => Key::End,
                _ => Key::Other,
            };
            Some((key, 3))
        }
        0x1b => Some((Key::Esc, 1)),
        _ => Some((Key::Other, 2)),
    }
}

/// SGR mouse press: "button;col;row".
fn parse_mouse(params: &[u8]) -> Key {
    let text = String::from_utf8_lossy(params);
    let parts: Vec<u16> = text.split(';').filter_map(|p| p.parse().ok()).collect();
    match parts.as_slice() {
        [64, _, _] => Key::Wheel(-1),
        [65, _, _] => Key::Wheel(1),
        [0, _, row] => Key::Click(*row),
        _ => Key::Other,
    }
}
