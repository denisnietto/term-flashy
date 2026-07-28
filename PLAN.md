# Roadmap

Target features: transparent background with opacity control, a
sidebar tab bar, and per-tab notification signaling. Built in Rust,
GUI shell in GTK4 with the VTE terminal widget (`gtk4`/`vte4` crates)
— the same terminal engine used by GNOME Terminal, Tilix, and
Terminator. (Originally attempted with a 100% Rust custom renderer
via Floem; abandoned after release-mode testing still showed poor
scroll/output fluidity — see git history if curious, no longer part
of the codebase.)

Self-testing: GUI behavior can be verified headlessly without asking
the user, using `Xvfb` (virtual X11 display, isolated from the user's
real Sway/Wayland session) + `xdotool` (input) + ImageMagick's
`import` (screenshots). See recent shell history for the pattern:
launch `Xvfb :99`, run the app with `GDK_BACKEND=x11 DISPLAY=:99`,
find window IDs via `xwininfo -root -tree`, click/type via
`xdotool ... --window <id>`, capture via `import -window <id> file.png`.

## Phase 0 — Skeleton (done)

- Project created, GTK4 app window building and running.

## Phase 1 — Terminal core (done)

- Spawn a shell process (PTY) per tab via `vte4::Terminal::spawn_async`.
- Rendering, ANSI parsing, and scrollback handled natively by VTE.
- Keyboard input handled natively by VTE.
- Ctrl+D / shell exit closes the app (`child-exited` signal).
- Ctrl+Shift+C / Ctrl+Shift+V for copy/paste.

## Phase 2 — Multiple tabs + sidebar (done)

- Support multiple PTY sessions at once.
- Sidebar: list of tabs, switch active tab (click, or Ctrl+Shift+Up/Down),
  close tab (per-tab "x" or Ctrl+D), open new tab (button or
  Ctrl+Shift+T). Switching a tab always focuses its terminal.
- Each tab keeps its own terminal/shell process independently.
- Sidebar is resizable (`Paned`) and its width persists across restarts
  (`~/.config/term-flashy/state.txt`).
- Tab names are renamable via double-click (custom `Label`/`Entry`
  swap, not `EditableLabel` — that widget ate single clicks needed for
  tab switching). Enter or clicking away commits the new name, Esc
  cancels.

## Phase 3 — Per-tab notifications (done)

Two trigger sources, both configurable (each can be turned on/off,
and mapped to a visual style):

- **Built-in triggers** — detected by term-flashy itself from the PTY
  stream/process state, no cooperation from the running app needed.
  - Terminal bell (`\a`) — done: marks the tab's sidebar row (amber
    background via the `notify` CSS class) if it's not the active
    tab; clears when the tab is selected.
  - Non-zero exit code, and long-running command (≥10s,
    `LONG_COMMAND_THRESHOLD`) finishing — done, via VTE's OSC 133
    shell-integration termprops (`vte.shell.preexec` /
    `vte.shell.postexec`, requires VTE 0.78+, gated behind the vte4
    crate's `v0_78` feature — this forced bumping `gtk4`/`glib`/`gio`
    to 0.11/0.22/0.22 and enabling gtk4's `v4_10` feature, since vte4
    0.10's bindings reference `gtk4::Accessible` unconditionally).
    Shell integration activates automatically — VTE sets `$VTE_VERSION`
    for spawned shells, which `/etc/profile.d/vte-2.91.sh` (shipped by
    the distro's VTE package) detects to wire up bash/zsh — no changes
    to the user's shell rc needed. `vte.shell.preexec` marks command
    start time; `vte.shell.postexec` carries the exit code and is where
    both checks fire.
- **App-driven triggers** — done, simplified after two failed attempts:
  - OSC 777 (desktop notification): the installed VTE (0.80) no longer
    implements the `notification-received` signal at all — connecting
    to it crashed the app (`GLib-GObject-CRITICAL: signal
    'notification-received' is invalid`). Modern VTE silently ignores
    OSC 777.
  - OSC 2 (window title) with a marker prefix: technically works, but
    bash (and most shells' default `PS1`) sets the window title on
    every prompt (`\u@\h: \w`), clobbering our marker before the
    signal handler ever reads it back — a race we can't win from
    outside the shell.
  - What's actually implemented: `term-flashy-notify` just rings the
    terminal bell (`\a`/0x07), reusing the bell trigger. No metadata
    (title/body) — matches what was actually asked for ("só quero que
    fique laranja a aba"). If richer app-driven notifications (with a
    message) are wanted later, the reliable way is to stop letting VTE
    own the PTY (`spawn_async`) and instead spawn it ourselves, reading
    the byte stream and forwarding it to VTE via `Terminal::feed()`,
    intercepting a custom escape sequence before VTE ever sees it —
    the only channel a shell can't clobber. Not done; revisit only if
    needed.
- Companion CLI `term-flashy-notify` (`src/bin/term-flashy-notify.rs`)
  — done. Not as simple as "print a bell" in the end: a hook
  subprocess's stdio is connected to Claude Code's internal IPC
  socket, not the terminal's PTY, and the subprocess has no
  controlling terminal at all (`/dev/tty` fails to open too). Fixed
  by walking up `/proc/<pid>/status` PPid chain from the hook process
  until finding an ancestor whose fd 0 resolves (via
  `/proc/<pid>/fd/0`) to a real `/dev/pts/*` device — that's the
  actual interactive shell/`claude` process — and writing the bell
  directly to that device.
- Claude Code integration (the original motivating use case) — hooks
  configured in `~/.claude/settings.json` (global): `Stop` and
  `Notification` both call the built `term-flashy-notify` binary path.
  Points at the debug binary for now — repoint to the release binary
  once Phase 6 (packaging) builds it. **Verified working end-to-end**,
  headlessly (Xvfb): opened a new tab, ran `claude -p '...'` in it,
  switched to a different tab, waited for the nested Claude Code call
  to finish — the origin tab turned amber via the `Stop` hook.
- Reflect the signal visually on the sidebar tab — done (amber row),
  configurable style mapping deferred to Phase 4 (config).
- Config maps trigger → style, and lets each trigger be disabled
  individually — deferred to Phase 4 (config).

## Phase 4 — Config and polish (done)

TOML config at `~/.config/term-flashy/config.toml` (`src/config.rs`),
written with defaults on first run if missing. Initially scoped as
file-only (edit by hand), later extended with an in-app settings
window (`open_settings_window` in `main.rs`) opened via a "Settings"
button below "+ New Tab" in the sidebar — Save writes the file and
updates the running app's in-memory config (`AppUi.config` is a
`RefCell<Config>`) immediately for new tabs; existing tabs' font
doesn't change retroactively, only newly opened ones. Tested headlessly
(Xvfb): open window, edit every field, Save, verify file contents,
restart the app, verify both the terminal (font size) and the
reopened settings window reflect the persisted values.

- `font_family` (default: unset → "Monospace", resolves via
  fontconfig) and `font_size` (default 11.0), applied per-tab via
  `Terminal::set_font`.
- Transient per-tab font zoom (done): Ctrl+'+'/Ctrl+'-' (and the
  Ctrl+Shift variants, since '+' and '_' need Shift on most layouts,
  plus the numpad keys) grow/shrink only the focused tab's own VTE
  font in memory (`adjust_font_size`, reads `terminal.font()`, adjusts
  the `pango::FontDescription` size, calls `set_font` back) — never
  touches `AppUi.config`, so it doesn't persist to `config.toml` and
  doesn't affect other tabs, unlike the Settings font size field above.
- `trigger_bell`, `trigger_exit_code`, `trigger_long_command`: each
  built-in trigger individually disableable (per-trigger config →
  style mapping from the Phase 3 wishlist not implemented — all
  triggers still map to the same amber row style; revisit only if a
  distinct look per trigger is actually wanted).
- `long_command_threshold_secs` (default 10): configurable version of
  what was a hardcoded constant.
- Not done, considered out of scope for now: keybinding config, color
  scheme config, tab behavior on shell exit (currently always closes
  the tab).

## Phase 5 — Transparency (optional) — abandoned, replaced

- Original approach: window made transparency-capable via
  `window { background-color: transparent; }` in the app's CSS, plus
  `Terminal::set_colors` giving the VTE background an alpha channel
  (`background_opacity` config field). Initial pixel-math check on the
  user's real Sway session (alpha blended against a flat backdrop
  color) looked correct and was reported as "done" — **that was
  wrong**. Downloading a real photo and setting it as the Sway
  wallpaper (`swaymsg output "*" bg ... fill`) for a proper visual
  test revealed the terminal area was a uniform flat color regardless
  of what was actually behind the window (sky vs. trees at different
  screen positions gave identical pixels) — real desktop compositing
  was never happening; the earlier math check had coincidentally
  matched alpha-blending-against-a-constant-color, not
  alpha-blending-against-the-desktop. Tried both the GL and cairo
  (`GSK_RENDERER=cairo`) renderers, same result either way, so it
  wasn't a renderer-backend issue. Root cause not fully identified;
  not worth chasing further given the effort already spent.
- **Decision: abandoned and removed entirely** (config field, CSS,
  `set_colors` alpha, settings-window control) — see Phase 5b below
  for the replacement that actually works.
- Lesson: a pixel-value check that matches a plausible formula is not
  proof of the real effect (real desktop bleed-through) — vary the
  actual background content and confirm the result changes with it,
  don't trust a single coincidental-looking match.

## Phase 5b — Background image (optional) — done

Replaces Phase 5. Instead of real window/desktop transparency
(unreliable, abandoned above), shows a static image behind the
terminal text — composited within the app's own window (GTK
`Overlay` + `Picture`), not dependent on Wayland/compositor-level
alpha, so it sidesteps whatever broke Phase 5 entirely.

- Config: `background_image` (`Option<String>`, path to an image
  file) and `background_dim` (0.0–1.0, how much the terminal's own
  color covers the image; default 0.55). Both editable in the
  settings window, applied live to already-open tabs on Save
  (`AppUi::apply_background_settings`), no restart needed — matches
  the "dynamic, like swaybg" ask.
- Implementation: `gtk4::Overlay` with a `gtk4::Picture`
  (`ContentFit::Cover`) as the base child and the tab `Stack` as the
  overlay child; `Terminal::set_colors` gives the VTE background an
  alpha of `background_dim` (or fully opaque `1.0` when no image is
  set, preserving the original look).
- Non-obvious gotcha hit during implementation: setting alpha via
  `set_colors` alone wasn't enough — the VTE widget has its own CSS
  "background" paint (from the GTK theme, opaque white by default)
  that's drawn *underneath* VTE's own custom rendering. Needed an
  explicit `.term-flashy-transparent { background-color: transparent;
  background-image: none; }` CSS class added to the `Terminal` widget
  itself (and to the `Stack`/`Overlay` wrapping it), or the image
  never showed through regardless of the `set_colors` alpha value.
- Verified on the user's real Sway session: downloaded a photo,
  configured it as `background_image`, confirmed visually (screenshot
  sent to user) that the image renders behind the terminal text with
  dim=0.5, with the prompt still clearly readable on top.

## Phase 6 — Packaging

- `term-flashy --help` — done. Prints `HELP.txt` (embedded in the
  binary via `include_str!`) and exits before GTK/GLib's own arg
  parsing gets a chance to print its generic option-parser help
  instead. Explains the project and, in particular, the notification
  feature (built-in triggers + the app-driven `term-flashy-notify`
  hook, with Claude Code as the motivating example) for people who
  find the project without prior context.
- Build in release mode, check binary size and startup time.
- Write the README (can lean on `HELP.txt`'s content), add build/run
  instructions.
- Only then: init git, first commit, push to GitHub, publish setup
  (crates.io only if it makes sense for a GUI app).

## Phase 7 — Cleanup (final phase)

- Go through any throwaway test/benchmark code accumulated during
  development and clean up: remove what's no longer needed, keep
  what's worth turning into a real regression test.
- Delete this file (`PLAN.md`) once everything above is done — it's a
  working roadmap, not permanent project documentation.
