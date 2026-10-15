# term-flashy

A Rust terminal emulator built on GTK4 and VTE (the same terminal
engine used by GNOME Terminal, Tilix, and Terminator), with a sidebar
tab bar and per-tab visual notifications.

## Features

**Multiple tabs with a sidebar**
Each tab runs its own independent shell process. The sidebar is
resizable (drag the divider) and remembers its width between runs.
Double-click a tab's name to rename it.

**Search in history**
Ctrl+Shift+F opens a search bar under the current tab to find text
in its scrollback.

**Background image (optional)**
Show an image behind the terminal text, like a desktop wallpaper,
with a configurable dim level so text stays readable. Real window
transparency (seeing the actual desktop through the terminal) was
tried and dropped: on this project's target setup, GTK4/VTE composited
alpha against a flat color instead of the real desktop, so it never
actually worked. This feature gets a similar look without that
problem, since it composites within the app's own window instead of
relying on desktop-level transparency.

**Per-tab notifications**
The core feature of this project. When something happens in a tab
that isn't focused, that tab's row is highlighted (amber background)
in the sidebar until you click it. There are two kinds of trigger:

- *Built-in triggers* (term-flashy detects these on its own, no
  cooperation needed from whatever is running in the tab):
  - Terminal bell (`\a`)
  - A command finishing with a non-zero exit code
  - A command that takes too long to finish (configurable, 10 seconds
    by default)
- *App-driven trigger* — any program running inside a tab can flag
  that tab itself by calling the included `term-flashy-notify`
  command. This is meant for long-running command-line tools that
  need to signal completion or that they need attention. The
  motivating use case is [Claude Code](https://claude.com/claude-code):
  configuring a `Stop`/`Notification` hook in `~/.claude/settings.json`
  that calls `term-flashy-notify` makes the tab it's running in flash
  on its own when it finishes responding or needs a permission
  decision, even while you're working in a different tab.

**Attach from another terminal (e.g. over SSH)**
`term-flashy attach`, run from any terminal on the same machine
(typically an SSH session from a laptop or phone), lists the tabs of
every running term-flashy and lets you work inside one as if you were
in front of it. Nothing listens on the network: SSH is the transport.
Off by default — turn on "Allow term-flashy attach" in Settings; it
applies to tabs opened from then on.

- The menu lists the tabs (grouped by window when there's more than
  one) with the pending highlight (●), blocked tabs (✕) and tabs
  someone else is connected to (⇄), and a key bar at the bottom; `?`
  shows the full help. Scroll with arrows / PgUp / PgDn / Home / End or
  the mouse wheel, `/` to filter by name, Enter (or a tap) to enter a
  tab.
- From the menu: `c` opens a new tab next to the selected one, in its
  folder, and enters it (the window keeps showing whatever it was
  showing); `r` renames the selected tab; `x` closes it after a
  confirmation. Blocked tabs can't be touched from here.
- Inside a tab, **Ctrl+]** brings the menu back. From the menu: Esc
  returns to the tab, `n`/`p` jump to the next/previous tab, `q` quits,
  and Ctrl+] again sends a literal Ctrl+] to the tab.
- While connected, the tab takes the size of your terminal and returns
  to its own size when you leave. A tab someone is attached to counts
  as visible: it doesn't get the pending highlight. The sidebar shows
  ⇄ on it.
- Right-click a tab → "Bloquear attach" refuses connections to it (and
  drops current ones), e.g. for a root shell or a production SSH
  session. Saved with the session.
- Every connection, rejection, block, and tab created / renamed /
  closed from attach is logged to
  `~/.local/state/term-flashy/attach.log` (who connected, never what was
  on screen).

How it works: with attach on, each tab runs its shell under
`term-flashy pty-host`, which owns the shell's PTY, feeds VTE as before
and serves attached clients through a Unix socket in
`$XDG_RUNTIME_DIR/term-flashy/` (0700). Each term-flashy process also
has a `control-<pid>.sock` there for the tab list.

Security: anything running as your user can connect to those sockets
and type into attachable tabs — the same trade-off as tmux/screen, but
it does bypass protections that otherwise make this harder (Yama
`ptrace_scope`, disabled `TIOCSTI`). Hence: off by default, per-tab
block, the ⇄ indicator and the log. Sockets only accept the same user
(`SO_PEERCRED`), the directory is refused if it isn't a private
directory owned by you, and the tab's output reaches your terminal
through a vt100 screen model, so escape sequences it doesn't model
(clipboard writes, titles…) are not forwarded. Use key-only SSH
authentication, with the key on the phone protected.

Tabs opened before turning it on (or before upgrading) can't be
attached to; restart term-flashy once.

**Configurable**
`~/.config/term-flashy/config.toml` (or the in-app Settings window):
font family, font size, background image and dim level, scrollback
size (10,000 lines by default, or unlimited), and each
built-in trigger can be turned on or off individually. Changes apply
immediately to already-open tabs, no restart needed.

## Building

System dependencies (Debian/Ubuntu package names):

```
sudo apt install libgtk-4-dev libvte-2.91-gtk4-dev
```

Note: this needs `libvte-2.91-gtk4 >= 0.78`. If your distro's package
is older (e.g. Ubuntu 24.04 ships 0.76), the build fails at `vte4-sys`
— see the BUILD NOTE section of `term-flashy --help` (or `HELP.txt`)
for how to build VTE 0.78 into a user prefix without touching the
system package.

Then:

```
cargo build --release
```

This builds two binaries in `target/release/`:

- `term-flashy` — the terminal app itself.
- `term-flashy-notify` — the companion CLI for the app-driven
  notification trigger (see Features above).

## Running

```
cargo run --release
```

Or run the built binary directly: `target/release/term-flashy`.

See `term-flashy --help` for a summary of the app's features.

## Installing (app launcher / sidebar, with icon)

```
./install.sh
```

Builds the release binaries and installs them for the current user:
`term-flashy` and `term-flashy-notify` into `~/.local/bin`, and
`term-flashy.desktop` into `~/.local/share/applications` so the app
shows up in the GNOME/Ubuntu launcher and dock/sidebar with an icon.
Uses the generic system "utilities-terminal" icon for now — drop a
custom icon file in later and update `Icon=` in `term-flashy.desktop`
if wanted. Make sure `~/.local/bin` is in your `PATH`.

## Configuring the app-driven trigger for Claude Code

Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [
      { "hooks": [{ "type": "command", "command": "/path/to/term-flashy-notify" }] }
    ],
    "Notification": [
      { "hooks": [{ "type": "command", "command": "/path/to/term-flashy-notify" }] }
    ]
  }
}
```

Replace `/path/to/term-flashy-notify` with the actual path to the
built binary (e.g. `~/workspace/term-flashy/target/release/term-flashy-notify`).
