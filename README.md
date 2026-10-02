# term-flashy

A Rust terminal emulator built on GTK4 and VTE (the same terminal
engine used by GNOME Terminal, Tilix, and Terminator), with a sidebar
tab bar and per-tab visual notifications.

## Features

**Multiple tabs with a sidebar**
Each tab runs its own independent shell process. The sidebar is
resizable (drag the divider) and remembers its width between runs.
Double-click a tab's name to rename it.

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
