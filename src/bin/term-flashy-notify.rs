// Companion CLI for term-flashy's app-driven tab notifications.
// Just rings the terminal bell (BEL, 0x07), which term-flashy already
// picks up (via VTE's "bell" signal) to mark the tab's sidebar row.
// Exists so hooks (e.g. a Claude Code "Stop hook") can call one
// command instead of needing to know they should print `\a`.
//
// Can't just print to stdout or /dev/tty: callers like Claude Code
// run hook commands as a subprocess whose stdio is connected to an
// internal IPC socket, not the terminal's PTY, and that subprocess
// has no controlling terminal at all (so /dev/tty fails too). The
// actual PTY only exists on an ancestor process further up the tree
// (the interactive shell / `claude` CLI process itself) — so this
// walks up parent PIDs via /proc until it finds one whose fd 0 is a
// real PTY device, and writes the bell there directly.
use std::io::Write;

fn ppid_of(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))?
        .trim()
        .parse()
        .ok()
}

fn tty_of(pid: u32) -> Option<std::path::PathBuf> {
    let link = std::fs::read_link(format!("/proc/{pid}/fd/0")).ok()?;
    let s = link.to_string_lossy();
    (s.starts_with("/dev/pts/") || s.starts_with("/dev/tty")).then_some(link)
}

fn find_controlling_tty() -> Option<std::path::PathBuf> {
    let mut pid = std::process::id();
    for _ in 0..64 {
        if let Some(tty) = tty_of(pid) {
            return Some(tty);
        }
        pid = ppid_of(pid)?;
        if pid <= 1 {
            return None;
        }
    }
    None
}

fn main() {
    let Some(tty_path) = find_controlling_tty() else {
        return;
    };
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open(tty_path) {
        let _ = tty.write_all(b"\x07");
        let _ = tty.flush();
    }
}
