// GUI side of the attach feature: one `control-<pid>.sock` per term-flashy
// process. `attach` asks it for the tab list; each tab's pty-host keeps a
// connection open to report who is attached and to receive block/unblock.
// Socket I/O runs on worker threads; requests are handled on the GTK main
// thread through `handler`.
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::attach_ipc;

pub enum Request {
    List,
    /// `new`, `rename` or `close` on a tab, as sent by `attach`.
    Action {
        command: Value,
        peer_pid: i32,
    },
    HostHello {
        socket: PathBuf,
        peer_pid: i32,
        writer: UnixStream,
    },
    HostEvent {
        socket: PathBuf,
        event: Value,
    },
    HostGone {
        socket: PathBuf,
    },
}

pub type Handler = fn(Request) -> Option<Value>;

pub fn control_socket_path(dir: &Path) -> PathBuf {
    dir.join(format!("control-{}.sock", std::process::id()))
}

pub fn start(dir: &Path, handler: Handler) -> std::io::Result<PathBuf> {
    let path = control_socket_path(dir);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || serve(stream, handler));
        }
    });
    Ok(path)
}

/// Runs `handler` on the main thread and waits for its answer.
fn call(handler: Handler, request: Request) -> Option<Value> {
    let (tx, rx) = std::sync::mpsc::channel();
    glib::MainContext::default().invoke(move || {
        let _ = tx.send(handler(request));
    });
    rx.recv_timeout(Duration::from_secs(5)).ok().flatten()
}

fn read_json_line(reader: &mut BufReader<UnixStream>) -> Option<Value> {
    let mut line = String::new();
    let n = reader
        .by_ref()
        .take(attach_ipc::MAX_CONTROL_LINE)
        .read_line(&mut line)
        .ok()?;
    if n == 0 || !line.ends_with('\n') {
        return None;
    }
    serde_json::from_str(&line).ok()
}

fn serve(stream: UnixStream, handler: Handler) {
    let Ok((uid, peer_pid)) = attach_ipc::peer_credentials(&stream) else {
        return;
    };
    if uid != nix::unistd::getuid().as_raw() {
        return;
    }
    let Ok(mut writer) = stream.try_clone() else { return };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream);
    let Some(first) = read_json_line(&mut reader) else {
        return;
    };

    match first["cmd"].as_str() {
        Some("list") => {
            let reply = call(handler, Request::List).unwrap_or(Value::Null);
            let _ = writer.write_all(format!("{reply}\n").as_bytes());
        }
        Some("new" | "rename" | "close") => {
            let request = Request::Action {
                command: first,
                peer_pid,
            };
            let reply = call(handler, request)
                .unwrap_or_else(|| json!({"ok": false, "error": "sem resposta do term-flashy"}));
            let _ = writer.write_all(format!("{reply}\n").as_bytes());
        }
        Some("host") => {
            let Some(socket) = first["socket"].as_str().map(PathBuf::from) else {
                return;
            };
            let Ok(host_writer) = writer.try_clone() else { return };
            let reply = call(
                handler,
                Request::HostHello {
                    socket: socket.clone(),
                    peer_pid,
                    writer: host_writer,
                },
            )
            .unwrap_or_else(|| json!({"ok": false}));
            let ok = reply["ok"] == true;
            let _ = writer.write_all(format!("{reply}\n").as_bytes());
            if !ok {
                return;
            }
            let _ = reader.get_ref().set_read_timeout(None);
            while let Some(event) = read_json_line(&mut reader) {
                let socket = socket.clone();
                glib::MainContext::default().invoke(move || {
                    handler(Request::HostEvent { socket, event });
                });
            }
            glib::MainContext::default().invoke(move || {
                handler(Request::HostGone { socket });
            });
        }
        _ => {}
    }
}
