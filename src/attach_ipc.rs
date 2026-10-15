// Shared pieces of the attach feature: socket directory, wire framing
// between `attach` and `pty-host`, and small /proc helpers.
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// Client -> host: raw keyboard bytes.
pub const FRAME_INPUT: u8 = 1;
/// Client -> host: the client's terminal size (rows, cols). Also the
/// first frame a client must send.
pub const FRAME_RESIZE: u8 = 2;
/// Host -> client: raw output bytes from the tab.
pub const FRAME_OUTPUT: u8 = 3;
/// Host -> client: connection refused, payload is the reason.
pub const FRAME_REJECT: u8 = 4;
/// Host -> client: size the tab is running at (rows, cols).
pub const FRAME_SIZE: u8 = 5;

const MAX_FRAME: usize = 1 << 20;
/// Longest JSON line accepted on the control socket.
pub const MAX_CONTROL_LINE: u64 = 64 * 1024;

/// `$XDG_RUNTIME_DIR/term-flashy`, created 0700 if missing. Refuses to
/// use it unless it is a real directory owned by us with no group/other
/// access; never falls back to /tmp.
pub fn socket_dir() -> io::Result<PathBuf> {
    let uid = nix::unistd::getuid().as_raw();
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    check_private_dir(&base, uid, false)?;
    let dir = base.join("term-flashy");
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    check_private_dir(&dir, uid, true)?;
    Ok(dir)
}

fn check_private_dir(dir: &Path, uid: u32, strict_mode: bool) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    let bad = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{}: {why}", dir.display()),
        ))
    };
    if !meta.file_type().is_dir() {
        return bad("not a directory (or is a symlink)");
    }
    if meta.uid() != uid {
        return bad("not owned by the current user");
    }
    if strict_mode && meta.mode() & 0o077 != 0 {
        return bad("accessible by other users (expected mode 0700)");
    }
    Ok(())
}

/// Uid and pid of the process on the other end of `stream`.
pub fn peer_credentials(stream: &UnixStream) -> io::Result<(u32, i32)> {
    let creds = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
        .map_err(io::Error::from)?;
    Ok((creds.uid(), creds.pid()))
}

pub fn peer_is_same_user(stream: &UnixStream) -> bool {
    peer_credentials(stream).is_ok_and(|(uid, _)| uid == nix::unistd::getuid().as_raw())
}

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(kind);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

/// Incremental frame decoder for non-blocking reads.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next_frame(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let kind = self.buf[0];
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some((kind, payload)))
    }
}

pub fn encode_size(rows: u16, cols: u16) -> [u8; 4] {
    let [r0, r1] = rows.to_be_bytes();
    let [c0, c1] = cols.to_be_bytes();
    [r0, r1, c0, c1]
}

pub fn decode_size(payload: &[u8]) -> Option<(u16, u16)> {
    let bytes: [u8; 4] = payload.try_into().ok()?;
    let rows = u16::from_be_bytes([bytes[0], bytes[1]]);
    let cols = u16::from_be_bytes([bytes[2], bytes[3]]);
    (rows > 0 && cols > 0).then_some((rows, cols))
}

/// Drops control characters so untrusted text can't inject escape
/// sequences when printed to a terminal or a log line.
pub fn sanitize(text: &str, max_chars: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max_chars)
        .collect()
}

pub fn process_cmdline(pid: i32) -> String {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let text = String::from_utf8_lossy(&raw).replace('\0', " ");
    sanitize(text.trim(), 200)
}

pub fn parent_pid(pid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))?
        .trim()
        .parse()
        .ok()
}

/// Pid encoded in a `control-<pid>.sock` / `tab-<pid>-<n>.sock` name.
pub fn socket_owner_pid(name: &str) -> Option<i32> {
    let rest = name
        .strip_prefix("control-")
        .or_else(|| name.strip_prefix("tab-"))?
        .strip_suffix(".sock")?;
    rest.split('-').next()?.parse().ok()
}

/// Removes sockets left behind by term-flashy processes that no longer exist.
pub fn remove_stale_sockets(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = socket_owner_pid(&name.to_string_lossy()) else {
            continue;
        };
        if !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub fn get_winsize(fd: i32) -> Option<(u16, u16)> {
    let mut ws: nix::libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { nix::libc::ioctl(fd, nix::libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
}

pub fn set_winsize(fd: i32, rows: u16, cols: u16) {
    let ws = nix::libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        nix::libc::ioctl(fd, nix::libc::TIOCSWINSZ, &ws);
    }
}
