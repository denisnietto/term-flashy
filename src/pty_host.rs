// `term-flashy pty-host`: sits between a tab's VTE and its shell so the
// tab can also be driven from `term-flashy attach`. Owns the shell's PTY,
// copies its output to VTE and to attached clients, keeps a vt100 copy of
// the screen for clients that connect later, and exits with the shell's
// status so the tab behaves exactly like a directly spawned shell.
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::termios;
use serde_json::json;

use crate::attach_ipc::{self, FRAME_INPUT, FRAME_OUTPUT, FRAME_REJECT, FRAME_RESIZE, FRAME_SIZE};

/// Frames queued per client before it's considered stuck and dropped,
/// so a slow SSH link never stalls the tab itself.
const CLIENT_QUEUE: usize = 1024;

const BLOCKED_MESSAGE: &str = "esta aba está bloqueada para attach";

struct Client {
    id: u64,
    tx: SyncSender<Vec<u8>>,
    stream: UnixStream,
    size: Option<(u16, u16)>,
    peer_pid: i32,
}

struct State {
    parser: vt100::Parser,
    vte_size: (u16, u16),
    current_size: (u16, u16),
    /// Ordered by most recent resize last; the last one sets the size.
    clients: Vec<Client>,
    blocked: bool,
    next_client_id: u64,
}

struct Host {
    state: Mutex<State>,
    master: File,
    control: Option<Mutex<UnixStream>>,
}

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    let _ = attach_ipc::write_frame(&mut buf, kind, payload);
    buf
}

impl Host {
    fn report(&self, event: serde_json::Value) {
        if let Some(control) = &self.control {
            let mut line = event.to_string();
            line.push('\n');
            let _ = control.lock().unwrap().write_all(line.as_bytes());
        }
    }

    /// Applies the size of the most recently resized client, or VTE's when
    /// nobody is attached.
    fn apply_size(&self, state: &mut State) {
        let wanted = state
            .clients
            .iter()
            .rev()
            .find_map(|c| c.size)
            .unwrap_or(state.vte_size);
        if wanted == state.current_size {
            return;
        }
        state.current_size = wanted;
        attach_ipc::set_winsize(self.master.as_raw_fd(), wanted.0, wanted.1);
        state.parser.screen_mut().set_size(wanted.0, wanted.1);
        let msg = frame(FRAME_SIZE, &attach_ipc::encode_size(wanted.0, wanted.1));
        broadcast(state, &msg);
    }

    /// Unregisters a client. Its writer thread then flushes what's queued
    /// (`reason`, if given, goes last) and closes the connection.
    fn drop_client(&self, id: u64, reason: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        let Some(index) = state.clients.iter().position(|c| c.id == id) else {
            return;
        };
        let client = state.clients.remove(index);
        if let Some(reason) = reason {
            let _ = client.tx.try_send(frame(FRAME_REJECT, reason.as_bytes()));
        }
        self.apply_size(&mut state);
        let count = state.clients.len();
        drop(state);
        self.report(json!({"event": "disconnect", "clients": count, "peer_pid": client.peer_pid}));
    }

    fn kick_all(&self, reason: &str) {
        let ids: Vec<u64> = self.state.lock().unwrap().clients.iter().map(|c| c.id).collect();
        for id in ids {
            self.drop_client(id, Some(reason));
        }
    }

    fn is_registered(&self, id: u64) -> bool {
        self.state.lock().unwrap().clients.iter().any(|c| c.id == id)
    }
}

/// Sends to every client; ones whose queue is full or gone are shut down
/// (their reader thread then unregisters them).
fn broadcast(state: &mut State, msg: &[u8]) {
    for client in &state.clients {
        match client.tx.try_send(msg.to_vec()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                let _ = client.stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

struct Args {
    socket: Option<PathBuf>,
    control: Option<PathBuf>,
    command: Vec<String>,
}

fn parse_args(args: &[String]) -> Option<Args> {
    let mut socket = None;
    let mut control = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket" => socket = iter.next().map(PathBuf::from),
            "--control" => control = iter.next().map(PathBuf::from),
            "--" => break,
            _ => return None,
        }
    }
    let command: Vec<String> = iter.cloned().collect();
    (!command.is_empty()).then_some(Args {
        socket,
        control,
        command,
    })
}

/// Registers with the GUI's control socket. Attach is only offered when
/// this succeeds, so every connection shows up in the GUI and its log.
fn register(control_path: &PathBuf, socket: &PathBuf) -> Option<(UnixStream, bool)> {
    let stream = UnixStream::connect(control_path).ok()?;
    if !attach_ipc::peer_is_same_user(&stream) {
        return None;
    }
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let hello = json!({"cmd": "host", "socket": socket}).to_string() + "\n";
    (&stream).write_all(hello.as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new((&stream).take(attach_ipc::MAX_CONTROL_LINE))
        .read_line(&mut line)
        .ok()?;
    stream.set_read_timeout(None).ok()?;
    let reply: serde_json::Value = serde_json::from_str(&line).ok()?;
    if reply["ok"] != true {
        return None;
    }
    Some((stream, reply["blocked"] == true))
}

pub fn run(args: &[String]) -> ! {
    let Some(args) = parse_args(args) else {
        eprintln!("usage: term-flashy pty-host [--socket PATH --control PATH] -- COMMAND...");
        std::process::exit(2);
    };

    let stdin = std::io::stdin();
    let outer_termios = termios::tcgetattr(stdin.as_fd()).ok();
    let vte_size = attach_ipc::get_winsize(0).unwrap_or((24, 80));
    let winsize = nix::pty::Winsize {
        ws_row: vte_size.0,
        ws_col: vte_size.1,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = match nix::pty::openpty(Some(&winsize), outer_termios.as_ref()) {
        Ok(pty) => pty,
        Err(err) => {
            // Never leave the tab without a shell.
            eprintln!("term-flashy pty-host: openpty failed ({err}), running the shell directly");
            let err = std::process::Command::new(&args.command[0])
                .args(&args.command[1..])
                .exec();
            eprintln!("term-flashy pty-host: exec {} failed: {err}", args.command[0]);
            std::process::exit(127);
        }
    };
    for fd in [&pty.master, &pty.slave] {
        let _ = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC));
    }

    let mut command = std::process::Command::new(&args.command[0]);
    command.args(&args.command[1..]);
    let slave_stdio = |fd: &OwnedFd| fd.try_clone().map(std::process::Stdio::from);
    let (Ok(sin), Ok(sout), Ok(serr)) = (
        slave_stdio(&pty.slave),
        slave_stdio(&pty.slave),
        slave_stdio(&pty.slave),
    ) else {
        eprintln!("term-flashy pty-host: failed to set up the shell's terminal");
        std::process::exit(127);
    };
    command.stdin(sin).stdout(sout).stderr(serr);
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            if nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("term-flashy pty-host: failed to start {}: {err}", args.command[0]);
            std::process::exit(127);
        }
    };
    drop(pty.slave);
    drop(command);

    // VTE's side becomes a plain byte pipe; line discipline now happens in
    // the inner PTY, as the shell expects.
    if let Some(mut raw) = outer_termios.clone() {
        termios::cfmakeraw(&mut raw);
        let _ = termios::tcsetattr(stdin.as_fd(), termios::SetArg::TCSANOW, &raw);
    }

    let registration = match (&args.socket, &args.control) {
        (Some(socket), Some(control)) => register(control, socket).map(|r| (socket.clone(), r)),
        _ => None,
    };
    let (socket_path, control, blocked) = match registration {
        Some((socket, (stream, blocked))) => (Some(socket), Some(stream), blocked),
        None => (None, None, false),
    };

    let host = Arc::new(Host {
        state: Mutex::new(State {
            parser: vt100::Parser::new(vte_size.0, vte_size.1, 0),
            vte_size,
            current_size: vte_size,
            clients: Vec::new(),
            blocked,
            next_client_id: 0,
        }),
        master: File::from(pty.master),
        control: control
            .as_ref()
            .and_then(|s| s.try_clone().ok())
            .map(Mutex::new),
    });

    // Shell output -> VTE, vt100 copy, attached clients.
    let (output_done_tx, output_done_rx) = mpsc::channel::<()>();
    {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let mut stdout = unsafe { std::mem::ManuallyDrop::new(File::from_raw_fd(1)) };
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match (&host.master).read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let data = &buf[..n];
                let _ = stdout.write_all(data);
                let mut state = host.state.lock().unwrap();
                state.parser.process(data);
                if !state.clients.is_empty() {
                    let msg = frame(FRAME_OUTPUT, data);
                    broadcast(&mut state, &msg);
                }
            }
            let _ = output_done_tx.send(());
        });
    }

    // VTE keystrokes -> shell.
    {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let mut stdin = unsafe { std::mem::ManuallyDrop::new(File::from_raw_fd(0)) };
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = (&host.master).write_all(&buf[..n]);
                    }
                }
            }
        });
    }

    // Window resizes from VTE, and termination of the tab.
    {
        let host = Arc::clone(&host);
        let socket_path = socket_path.clone();
        use signal_hook::consts::{SIGHUP, SIGTERM, SIGWINCH};
        if let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGWINCH, SIGHUP, SIGTERM]) {
            std::thread::spawn(move || {
                for signal in signals.forever() {
                    if signal == SIGWINCH {
                        if let Some(size) = attach_ipc::get_winsize(0) {
                            let mut state = host.state.lock().unwrap();
                            state.vte_size = size;
                            host.apply_size(&mut state);
                        }
                    } else {
                        if let Some(path) = &socket_path {
                            let _ = std::fs::remove_file(path);
                        }
                        // Closing the master hangs up the shell's session.
                        std::process::exit(128 + signal);
                    }
                }
            });
        }
    }

    if let (Some(socket_path), Some(control)) = (&socket_path, control) {
        start_attach(&host, socket_path, control);
    }

    let status = child.wait();
    let _ = output_done_rx.recv_timeout(Duration::from_millis(300));
    if let Some(path) = &socket_path {
        let _ = std::fs::remove_file(path);
    }
    let code = match status {
        Ok(status) => status
            .code()
            .or_else(|| status.signal().map(|s| 128 + s))
            .unwrap_or(1),
        Err(_) => 1,
    };
    std::process::exit(code);
}

fn start_attach(host: &Arc<Host>, socket_path: &PathBuf, control: UnixStream) {
    let _ = std::fs::remove_file(socket_path);
    let listener = match UnixListener::bind(socket_path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("term-flashy pty-host: can't listen on {}: {err}", socket_path.display());
            return;
        }
    };
    let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600));

    // Commands from the GUI.
    {
        let host = Arc::clone(host);
        std::thread::spawn(move || {
            let reader = BufReader::new(control);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if msg["cmd"] == "block" {
                    let on = msg["on"] == true;
                    host.state.lock().unwrap().blocked = on;
                    if on {
                        host.kick_all(BLOCKED_MESSAGE);
                    }
                }
            }
            // GUI gone: stop offering attach.
            host.state.lock().unwrap().blocked = true;
            host.kick_all("o term-flashy foi fechado");
        });
    }

    let host = Arc::clone(host);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let host = Arc::clone(&host);
            std::thread::spawn(move || serve_client(&host, stream));
        }
    });
}

fn serve_client(host: &Arc<Host>, mut stream: UnixStream) {
    let (peer_uid, peer_pid) = attach_ipc::peer_credentials(&stream).unwrap_or((u32::MAX, 0));
    let peer_cmd = attach_ipc::process_cmdline(peer_pid);
    if peer_uid != nix::unistd::getuid().as_raw() {
        host.report(json!({"event": "reject", "reason": "other-user", "peer_pid": peer_pid, "peer_cmd": peer_cmd}));
        return;
    }
    if host.state.lock().unwrap().blocked {
        let _ = attach_ipc::write_frame(&mut stream, FRAME_REJECT, BLOCKED_MESSAGE.as_bytes());
        host.report(json!({"event": "reject", "reason": "blocked", "peer_pid": peer_pid, "peer_cmd": peer_cmd}));
        return;
    }

    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let size = match attach_ipc::read_frame(&mut stream) {
        Ok((FRAME_RESIZE, payload)) => attach_ipc::decode_size(&payload),
        _ => None,
    };
    let Some(size) = size else { return };
    let _ = stream.set_read_timeout(None);
    let Ok(writer_stream) = stream.try_clone() else { return };
    let Ok(shutdown_stream) = stream.try_clone() else { return };

    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(CLIENT_QUEUE);
    let _ = writer_stream.set_write_timeout(Some(Duration::from_secs(10)));
    std::thread::spawn(move || {
        let mut writer = writer_stream;
        for msg in rx {
            if writer.write_all(&msg).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Both);
    });

    let id;
    let count;
    {
        let mut state = host.state.lock().unwrap();
        if state.blocked {
            return;
        }
        id = state.next_client_id;
        state.next_client_id += 1;
        state.clients.push(Client {
            id,
            tx: tx.clone(),
            stream: shutdown_stream,
            size: Some(size),
            peer_pid,
        });
        host.apply_size(&mut state);
        // Snapshot under the same lock as the broadcast, so nothing is
        // lost or duplicated between it and the live stream.
        let (rows, cols) = state.current_size;
        let _ = tx.try_send(frame(FRAME_SIZE, &attach_ipc::encode_size(rows, cols)));
        let screen = state.parser.screen();
        let mut snapshot = Vec::new();
        if screen.alternate_screen() {
            snapshot.extend_from_slice(b"\x1b[?1049h");
        }
        snapshot.extend_from_slice(&screen.state_formatted());
        let _ = tx.try_send(frame(FRAME_OUTPUT, &snapshot));
        count = state.clients.len();
    }
    drop(tx);
    host.report(json!({"event": "connect", "clients": count, "peer_pid": peer_pid, "peer_cmd": peer_cmd}));

    loop {
        match attach_ipc::read_frame(&mut stream) {
            Ok((FRAME_INPUT, payload)) => {
                // A kicked client may still be sending; ignore it.
                if !host.is_registered(id) {
                    break;
                }
                let _ = (&host.master).write_all(&payload);
            }
            Ok((FRAME_RESIZE, payload)) => {
                if let Some(size) = attach_ipc::decode_size(&payload) {
                    let mut state = host.state.lock().unwrap();
                    if let Some(index) = state.clients.iter().position(|c| c.id == id) {
                        let mut client = state.clients.remove(index);
                        client.size = Some(size);
                        state.clients.push(client);
                        host.apply_size(&mut state);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    host.drop_client(id, None);
}
