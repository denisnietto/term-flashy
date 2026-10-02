mod config;
mod session;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use gtk4::prelude::*;
use gtk4::{gdk, Application, ApplicationWindow, Box as GtkBox, Button, EventControllerKey};
use gtk4::{Entry, GestureClick, Label, ListBox, Orientation, Overlay, Paned, PolicyType};
use gtk4::{Picture, ScrolledWindow, SelectionMode, Stack};
use vte4::{Format, PtyFlags, TerminalExt, TerminalExtManual};

use config::Config;

const APP_ID: &str = "dev.term-flashy";

const SIDEBAR_CSS: &str = "
.term-flashy-transparent {
    background-color: transparent;
    background-image: none;
}
.term-flashy-sidebar {
    background-color: #2b2b2b;
    color: #dddddd;
}
.term-flashy-sidebar row {
    background-color: #2b2b2b;
    color: #dddddd;
}
.term-flashy-sidebar row:selected {
    background-color: #3c3c3c;
}
.term-flashy-sidebar row.notify,
.term-flashy-sidebar row.notify:selected {
    background-color: #a35d00;
}
.term-flashy-btn {
    background-color: #3c3c3c;
    background-image: none;
    box-shadow: none;
    border: none;
    color: #dddddd;
}
.term-flashy-btn:hover {
    background-color: #4a4a4a;
    background-image: none;
}
";

// vte4 0.10's typed termprop getters (termprop_string, termprop_uint, ...)
// assert on the GVariant type of the underlying property; "vte.cwd" is
// URI-typed, and the crate doesn't bind vte_terminal_ref_termprop_uri (its
// auto-generated wrapper is commented out, unimplemented). The pre-0.78
// current-directory-uri property/signal are deprecated but still backed by
// the same value on this VTE version, so use those instead of crashing into
// a type-assertion CRITICAL.
#[allow(deprecated)]
fn connect_cwd_changed(terminal: &vte4::Terminal, f: impl Fn() + 'static) {
    terminal.connect_current_directory_uri_changed(move |_| f());
}

#[allow(deprecated)]
fn terminal_cwd_uri(terminal: &vte4::Terminal) -> Option<glib::GString> {
    terminal.current_directory_uri()
}

// Ctrl+'+'/Ctrl+Shift+'-' zoom: adjusts only this tab's own VTE font, in
// memory. Never touches `AppUi.config`, so it doesn't persist and doesn't
// affect other tabs or newly opened ones.
fn adjust_font_size(terminal: &vte4::Terminal, delta: f64) {
    let mut desc = terminal
        .font()
        .unwrap_or_else(|| pango::FontDescription::from_string("Monospace 11"));
    let new_size = (desc.size() as f64 / pango::SCALE as f64 + delta).clamp(6.0, 72.0);
    desc.set_size((new_size * pango::SCALE as f64) as i32);
    terminal.set_font(Some(&desc));
}

fn state_file_path() -> std::path::PathBuf {
    glib::user_config_dir().join("term-flashy").join("state.txt")
}

fn load_sidebar_width() -> i32 {
    std::fs::read_to_string(state_file_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(180)
}

fn save_sidebar_width(width: i32) {
    let path = state_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, width.to_string());
}

struct TabEntry {
    terminal: vte4::Terminal,
    row: gtk4::ListBoxRow,
    label: Label,
}

struct AppUi {
    app: Application,
    window: ApplicationWindow,
    stack: Stack,
    list_box: ListBox,
    background_picture: Picture,
    tabs: RefCell<HashMap<u32, TabEntry>>,
    next_id: Cell<u32>,
    active_rename: RefCell<Option<(Entry, Box<dyn Fn(bool)>)>>,
    config: RefCell<Config>,
    pending_session_save: Cell<Option<glib::SourceId>>,
    wallpaper_queue: RefCell<Vec<std::path::PathBuf>>,
    wallpaper_timer: Cell<Option<glib::SourceId>>,
    // Marking the currently-active tab as pending fights a GTK quirk: closing
    // the context-menu popover makes the row's own ListBox re-fire
    // "row-selected" for it (even though it was already selected and stays
    // selected), which would otherwise immediately strip the "notify" class
    // we just added. Set right after marking, consumed by the very next
    // row-selected for that same row, so exactly one such spurious clear is
    // skipped without suppressing real future ones.
    suppress_notify_clear: Cell<Option<gtk4::ListBoxRow>>,
}

// The system's own VTE shell integration (/etc/profile.d/vte-2.91.sh,
// sourced via /etc/bash.bashrc) is what normally reports each tab's
// directory via OSC 7, which cwd tracking (session persistence) depends
// on. That chain is easy to have broken outside of term-flashy's control:
// a customized ~/.bashrc that doesn't source /etc/bash.bashrc, or an older
// libvte-2.91-common package without the hook at all. To not depend on
// that, inject a minimal OSC 7 emitter directly into the spawned shell's
// environment; it only takes effect if nothing later in the shell's own
// startup unconditionally overwrites PROMPT_COMMAND (same inherent
// limitation the system's own script has). Bash only for now — zsh's
// equivalent (precmd_functions) is a shell array, not something settable
// through the environment.
const BASH_OSC7_HOOK: &str = r#"printf '\033]7;file://%s%s\033\\' "$HOSTNAME" "$PWD""#;

fn shell_env(shell: &str) -> Vec<String> {
    let is_bash = std::path::Path::new(shell)
        .file_name()
        .is_some_and(|name| name == "bash");
    if is_bash {
        vec![format!("PROMPT_COMMAND={BASH_OSC7_HOOK}")]
    } else {
        Vec::new()
    }
}

fn scan_wallpaper_folder(folder: &str) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    matches!(ext.to_lowercase().as_str(), "jpg" | "jpeg" | "png")
                })
        })
        .collect()
}

impl AppUi {
    fn mark_tab_notify(self: &Rc<Self>, page_name: &str, row: &gtk4::ListBoxRow) {
        if self.stack.visible_child_name().as_deref() != Some(page_name) {
            row.add_css_class("notify");
            self.schedule_session_save();
        }
        // Ask the compositor to (re-)activate our own already-mapped window.
        // On Wayland this goes through xdg-activation; a wlroots compositor
        // like Sway won't steal focus for it (per focus_on_window_activation),
        // it just flags the window/workspace as urgent — exactly the "something
        // happened over there" signal browsers give for background notifications.
        if !self.window.is_active() {
            self.window.present();
        }
    }

    /// Picks the next background image: refills and shuffles the queue
    /// (a fresh lap through every image in `background_folder`, so none
    /// repeats until all have shown once) when it runs out.
    fn advance_wallpaper(self: &Rc<Self>) {
        use rand::seq::SliceRandom;
        if self.wallpaper_queue.borrow().is_empty() {
            let Some(folder) = self.config.borrow().background_folder.clone() else {
                self.background_picture.set_visible(false);
                return;
            };
            let mut images = scan_wallpaper_folder(&folder);
            images.shuffle(&mut rand::rng());
            *self.wallpaper_queue.borrow_mut() = images;
        }
        let Some(path) = self.wallpaper_queue.borrow_mut().pop() else {
            self.background_picture.set_visible(false);
            return;
        };
        self.background_picture.set_filename(Some(&path));
        self.background_picture.set_visible(true);
    }

    /// (Re)starts background handling from the current config: random
    /// rotation through `background_folder` if set, else the static
    /// `background_image`, else no background. Called at startup and
    /// whenever Settings are saved, so changes apply immediately.
    fn restart_wallpaper_rotation(self: &Rc<Self>) {
        if let Some(source) = self.wallpaper_timer.take() {
            source.remove();
        }
        self.wallpaper_queue.borrow_mut().clear();

        let config = self.config.borrow();
        if config.background_folder.is_some() {
            let interval = config.background_rotate_interval();
            drop(config);
            self.advance_wallpaper();
            let ui = Rc::clone(self);
            let source = glib::timeout_add_local(interval, move || {
                ui.advance_wallpaper();
                glib::ControlFlow::Continue
            });
            self.wallpaper_timer.set(Some(source));
        } else {
            match &config.background_image {
                Some(path) => {
                    self.background_picture.set_filename(Some(path));
                    self.background_picture.set_visible(true);
                }
                None => {
                    self.background_picture.set_visible(false);
                }
            }
        }
    }

    fn add_tab(self: &Rc<Self>, restore: Option<session::TabState>) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let page_name = format!("tab-{id}");

        let terminal = vte4::Terminal::new();
        terminal.add_css_class("term-flashy-transparent");
        terminal.set_font(Some(&pango::FontDescription::from_string(
            &self.config.borrow().font_description(),
        )));
        terminal.set_colors(
            None,
            Some(&gdk::RGBA::new(
                0.0,
                0.0,
                0.0,
                self.config.borrow().terminal_background_alpha(),
            )),
            &[],
        );
        terminal.set_scrollback_lines(self.config.borrow().scrollback_lines as _);
        let restore_dir =restore.as_ref().and_then(|r| r.cwd.as_deref());
        let restore_title = restore.as_ref().map(|r| r.title.clone());
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let envv = shell_env(&shell);
        let envv_refs: Vec<&str> = envv.iter().map(String::as_str).collect();
        terminal.spawn_async(
            PtyFlags::DEFAULT,
            restore_dir,
            &[&shell],
            &envv_refs,
            glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            gio::Cancellable::NONE,
            |_result| {},
        );

        let key_controller = EventControllerKey::new();
        let terminal_for_keys = terminal.clone();
        key_controller.connect_key_pressed(move |_controller, keyval, _keycode, state| {
            let ctrl_shift = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
            if state.contains(ctrl_shift) {
                match keyval {
                    gdk::Key::C | gdk::Key::c => {
                        terminal_for_keys.copy_clipboard_format(Format::Text);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::V | gdk::Key::v => {
                        terminal_for_keys.paste_clipboard();
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }
            if state.contains(gdk::ModifierType::CONTROL_MASK) {
                match keyval {
                    gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => {
                        adjust_font_size(&terminal_for_keys, 1.0);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::minus | gdk::Key::underscore | gdk::Key::KP_Subtract => {
                        adjust_font_size(&terminal_for_keys, -1.0);
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }
            glib::Propagation::Proceed
        });
        terminal.add_controller(key_controller);

        self.stack.add_named(&terminal, Some(&page_name));

        let label = Label::new(Some(
            &restore_title.unwrap_or_else(|| format!("Tab {}", id + 1)),
        ));
        label.set_hexpand(true);
        label.set_xalign(0.0);

        let entry = Entry::new();
        entry.set_hexpand(true);
        entry.set_visible(false);

        let finish_rename: Rc<dyn Fn(bool)> = {
            let entry_for_finish = entry.clone();
            let label_for_finish = label.clone();
            let ui_for_finish = Rc::clone(self);
            Rc::new(move |commit: bool| {
                if !entry_for_finish.get_visible() {
                    return;
                }
                if commit {
                    let text = entry_for_finish.text();
                    if !text.trim().is_empty() {
                        label_for_finish.set_text(text.trim());
                        ui_for_finish.schedule_session_save();
                    }
                }
                entry_for_finish.set_visible(false);
                label_for_finish.set_visible(true);
                ui_for_finish.active_rename.borrow_mut().take();
            })
        };

        let start_rename: Rc<dyn Fn()> = {
            let entry_for_start = entry.clone();
            let label_for_start = label.clone();
            let ui_for_start = Rc::clone(self);
            let finish_for_start = Rc::clone(&finish_rename);
            Rc::new(move || {
                entry_for_start.set_text(&label_for_start.text());
                label_for_start.set_visible(false);
                entry_for_start.set_visible(true);
                entry_for_start.grab_focus();
                entry_for_start.select_region(0, -1);
                let finish_for_stored = Rc::clone(&finish_for_start);
                *ui_for_start.active_rename.borrow_mut() = Some((
                    entry_for_start.clone(),
                    Box::new(move |commit| finish_for_stored(commit)),
                ));
            })
        };

        let click_gesture = GestureClick::new();
        let start_rename_for_click = Rc::clone(&start_rename);
        click_gesture.connect_pressed(move |gesture, n_press, _x, _y| {
            if n_press == 2 {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                start_rename_for_click();
            }
        });
        label.add_controller(click_gesture);

        let finish_for_activate = Rc::clone(&finish_rename);
        entry.connect_activate(move |_entry| finish_for_activate(true));

        let finish_for_focus = Rc::clone(&finish_rename);
        let focus_controller = gtk4::EventControllerFocus::new();
        focus_controller.connect_leave(move |_controller| finish_for_focus(true));
        entry.add_controller(focus_controller);

        let finish_for_escape = Rc::clone(&finish_rename);
        let entry_key_controller = EventControllerKey::new();
        entry_key_controller.connect_key_pressed(move |_controller, keyval, _keycode, _state| {
            if keyval == gdk::Key::Escape {
                finish_for_escape(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(entry_key_controller);

        let close_button = Button::from_icon_name("window-close-symbolic");
        close_button.set_has_frame(false);
        close_button.add_css_class("term-flashy-btn");

        let row_box = GtkBox::new(Orientation::Horizontal, 6);
        row_box.set_margin_start(8);
        row_box.set_margin_end(4);
        row_box.set_margin_top(4);
        row_box.set_margin_bottom(4);
        row_box.append(&label);
        row_box.append(&entry);
        row_box.append(&close_button);

        let row = gtk4::ListBoxRow::new();
        row.set_widget_name(&page_name);
        row.set_child(Some(&row_box));

        self.list_box.append(&row);
        self.list_box.select_row(Some(&row));

        // Right-click a tab for a small context menu: rename (same action as
        // the double-click shortcut), and forcing the orange notify
        // highlight on manually — same "notify" css class the automatic
        // triggers use, just applied even while the tab is active. Leaving
        // the tab and coming back clears it exactly like any other notify,
        // via the row-selected handler below.
        let right_click_gesture = GestureClick::new();
        right_click_gesture.set_button(gdk::BUTTON_SECONDARY);
        let row_for_menu = row.clone();
        let window_for_menu = self.window.clone();
        let start_rename_for_menu = Rc::clone(&start_rename);
        let ui_for_menu = Rc::clone(self);
        right_click_gesture.connect_pressed(move |gesture, _n_press, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);

            // Parenting the popover to the row itself would make closing it
            // return keyboard focus to that row — and GTK's ListBox
            // auto-selects whatever row gains focus, silently switching the
            // active tab (and, since row-selected clears "notify", undoing
            // the mark just set). That focus-return turned out to fire
            // across multiple idle rounds, so it couldn't be reliably
            // undone after the fact. Parenting to the window instead keeps
            // the popover outside the row's focus/selection chain entirely
            // — translate the click point into window coordinates so it
            // still opens in the same visual spot.
            let point = row_for_menu
                .compute_point(&window_for_menu, &gtk4::graphene::Point::new(x as f32, y as f32));
            let (px, py) = point.map(|p| (p.x(), p.y())).unwrap_or((x as f32, y as f32));

            let popover = gtk4::Popover::new();
            popover.set_parent(&window_for_menu);
            popover.set_has_arrow(false);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(px as i32, py as i32, 1, 1)));
            popover.connect_closed(|popover| popover.unparent());

            let menu_box = GtkBox::new(Orientation::Vertical, 0);

            let rename_button = Button::with_label("Renomear");
            rename_button.set_has_frame(false);
            let popover_for_rename = popover.clone();
            let start_rename_for_click = Rc::clone(&start_rename_for_menu);
            rename_button.connect_clicked(move |_| {
                popover_for_rename.popdown();
                start_rename_for_click();
            });
            menu_box.append(&rename_button);

            let pending_label = if row_for_menu.has_css_class("notify") {
                "Remover marcação"
            } else {
                "Marcar como pendente"
            };
            let pending_button = Button::with_label(pending_label);
            pending_button.set_has_frame(false);
            let popover_for_pending = popover.clone();
            let row_for_pending_click = row_for_menu.clone();
            let ui_for_pending = Rc::clone(&ui_for_menu);
            pending_button.connect_clicked(move |_| {
                popover_for_pending.popdown();
                // Closing the popover can trigger GTK to re-fire
                // "row-selected" for the row it was anchored near, even
                // when that row was already selected and stays selected —
                // which would otherwise immediately clear the "notify"
                // class right back off. Arm the guard so the next
                // row-selected for this exact row is skipped once.
                if row_for_pending_click.has_css_class("notify") {
                    row_for_pending_click.remove_css_class("notify");
                } else {
                    row_for_pending_click.add_css_class("notify");
                    ui_for_pending
                        .suppress_notify_clear
                        .set(Some(row_for_pending_click.clone()));
                }
                ui_for_pending.schedule_session_save();
            });
            menu_box.append(&pending_button);

            popover.set_child(Some(&menu_box));
            popover.popup();
        });
        row.add_controller(right_click_gesture);

        // Drag-and-drop tab reordering: each row can be picked up (carrying
        // its tab id as the drag payload) and dropped onto another row to
        // swap places with it.
        let drag_source = gtk4::DragSource::new();
        drag_source.set_actions(gdk::DragAction::MOVE);
        drag_source.connect_prepare(move |_source, _x, _y| {
            Some(gdk::ContentProvider::for_value(&id.to_value()))
        });
        let row_for_icon = row.clone();
        drag_source.connect_drag_begin(move |source, _drag| {
            let paintable = gtk4::WidgetPaintable::new(Some(&row_for_icon));
            source.set_icon(Some(&paintable), 0, 0);
        });
        row.add_controller(drag_source);

        let drop_target = gtk4::DropTarget::new(glib::types::Type::U32, gdk::DragAction::MOVE);
        let ui_for_drop = Rc::clone(self);
        let row_for_drop = row.clone();
        drop_target.connect_drop(move |_target, value, _x, _y| {
            let Ok(source_id) = value.get::<u32>() else {
                return false;
            };
            let Some(source_row) = ui_for_drop
                .tabs
                .borrow()
                .get(&source_id)
                .map(|entry| entry.row.clone())
            else {
                return false;
            };
            if source_row == row_for_drop {
                return false;
            }
            let target_index = row_for_drop.index();
            let source_index = source_row.index();
            ui_for_drop.list_box.remove(&source_row);
            let new_index = if source_index < target_index {
                target_index - 1
            } else {
                target_index
            };
            ui_for_drop.list_box.insert(&source_row, new_index);
            ui_for_drop.list_box.select_row(Some(&source_row));
            ui_for_drop.schedule_session_save();
            true
        });
        row.add_controller(drop_target);

        let ui_for_close = Rc::clone(self);
        close_button.connect_clicked(move |_| {
            ui_for_close.confirm_close_tab(id);
        });

        let ui_for_exit = Rc::clone(self);
        terminal.connect_child_exited(move |_terminal, _status| {
            ui_for_exit.close_tab(id);
        });

        let ui_for_bell = Rc::clone(self);
        let row_for_bell = row.clone();
        let page_name_for_bell = page_name.clone();
        let trigger_bell = self.config.borrow().trigger_bell;
        terminal.connect_bell(move |_terminal| {
            if trigger_bell {
                ui_for_bell.mark_tab_notify(&page_name_for_bell, &row_for_bell);
            }
        });

        // Shell integration (OSC 133, via /etc/profile.d/vte-2.91.sh, active
        // automatically since VTE sets $VTE_VERSION for spawned shells):
        // "vte.shell.preexec" fires right before a command runs, and
        // "vte.shell.postexec" fires when it finishes, carrying the exit
        // code. Used for two built-in triggers: non-zero exit code, and a
        // long-running command finishing.
        let command_started: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));

        let command_started_for_preexec = Rc::clone(&command_started);
        terminal.connect_termprop_changed(Some("vte.shell.preexec"), move |_terminal, _name| {
            command_started_for_preexec.set(Some(Instant::now()));
        });

        let ui_for_postexec = Rc::clone(self);
        let row_for_exit = row.clone();
        let page_name_for_exit = page_name.clone();
        let trigger_exit_code = self.config.borrow().trigger_exit_code;
        let trigger_long_command = self.config.borrow().trigger_long_command;
        let long_command_threshold = self.config.borrow().long_command_threshold();
        terminal.connect_termprop_changed(Some("vte.shell.postexec"), move |terminal, _name| {
            let exit_code = terminal.termprop_uint("vte.shell.postexec").unwrap_or(0);
            let long_running = trigger_long_command
                && command_started
                    .take()
                    .is_some_and(|start| start.elapsed() >= long_command_threshold);
            if (trigger_exit_code && exit_code != 0) || long_running {
                ui_for_postexec.mark_tab_notify(&page_name_for_exit, &row_for_exit);
            }
        });

        // OSC 7 (same shell integration as the OSC 133 termprops above)
        // keeps the tab's current directory up to date.
        let ui_for_cwd = Rc::clone(self);
        connect_cwd_changed(&terminal, move || {
            ui_for_cwd.schedule_session_save();
        });

        self.stack.set_visible_child_name(&page_name);
        terminal.grab_focus();
        self.tabs.borrow_mut().insert(
            id,
            TabEntry {
                terminal,
                row,
                label,
            },
        );
        self.schedule_session_save();
    }

    /// Debounces session writes: `current-directory-uri-changed` can fire in
    /// quick bursts (e.g. a script `cd`-ing repeatedly), so this coalesces
    /// them into a single write ~500ms after the last change, instead of
    /// hitting the disk on every event.
    fn schedule_session_save(self: &Rc<Self>) {
        if !self.config.borrow().restore_session {
            return;
        }
        if let Some(source) = self.pending_session_save.take() {
            source.remove();
        }
        let ui = Rc::clone(self);
        let source = glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
            ui.save_session_now();
            ui.pending_session_save.set(None);
            glib::ControlFlow::Break
        });
        self.pending_session_save.set(Some(source));
    }

    fn save_session_now(&self) {
        if !self.config.borrow().restore_session {
            return;
        }
        let tabs = self.tabs.borrow();
        let mut saved_tabs = Vec::new();
        let mut index = 0;
        while let Some(current_row) = self.list_box.row_at_index(index) {
            let id: Option<u32> = current_row
                .widget_name()
                .strip_prefix("tab-")
                .and_then(|s| s.parse().ok());
            if let Some(entry) = id.and_then(|id| tabs.get(&id)) {
                let cwd = terminal_cwd_uri(&entry.terminal)
                    .and_then(|uri| glib::filename_from_uri(&uri).ok())
                    .map(|(path, _hostname)| path)
                    .filter(|path| path.is_dir())
                    .map(|path| path.to_string_lossy().into_owned());
                saved_tabs.push(session::TabState {
                    title: entry.label.text().to_string(),
                    cwd,
                    notify: entry.row.has_css_class("notify"),
                });
            }
            index += 1;
        }
        let active = self
            .list_box
            .selected_row()
            .map(|row| row.index().max(0) as usize)
            .unwrap_or(0);
        session::save(&session::SessionState {
            active,
            tabs: saved_tabs,
        });
    }

    fn apply_background_settings(self: &Rc<Self>) {
        self.restart_wallpaper_rotation();
        let alpha = self.config.borrow().terminal_background_alpha();
        for entry in self.tabs.borrow().values() {
            entry
                .terminal
                .set_colors(None, Some(&gdk::RGBA::new(0.0, 0.0, 0.0, alpha)), &[]);
        }
    }

    fn apply_scrollback_settings(&self) {
        let lines = self.config.borrow().scrollback_lines;
        for entry in self.tabs.borrow().values() {
            entry.terminal.set_scrollback_lines(lines as _);
        }
    }

    fn select_adjacent_tab(self: &Rc<Self>, delta: i32) {
        let Some(current) = self.list_box.selected_row() else {
            return;
        };
        let new_index = current.index() + delta;
        if new_index < 0 {
            return;
        }
        if let Some(row) = self.list_box.row_at_index(new_index) {
            self.list_box.select_row(Some(&row));
        }
    }

    /// Asks before closing a tab, so a misclick on the close button doesn't
    /// kill a running shell. Only guards the button; a shell that exits on
    /// its own still closes its tab silently.
    fn confirm_close_tab(self: &Rc<Self>, id: u32) {
        let Some(title) = self
            .tabs
            .borrow()
            .get(&id)
            .map(|entry| entry.label.text().to_string())
        else {
            return;
        };
        let dialog = gtk4::AlertDialog::builder()
            .modal(true)
            .message(format!("Fechar a aba \"{title}\"?"))
            .detail("O que estiver rodando nela será encerrado.")
            .buttons(["Cancelar", "Fechar"])
            .default_button(0)
            .cancel_button(0)
            .build();
        let ui = Rc::clone(self);
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if matches!(result, Ok(1)) {
                ui.close_tab(id);
            }
        });
    }

    fn close_tab(self: &Rc<Self>, id: u32) {
        let entry = self.tabs.borrow_mut().remove(&id);
        if let Some(entry) = entry {
            self.stack.remove(&entry.terminal);
            self.list_box.remove(&entry.row);
        }
        self.schedule_session_save();
        if self.tabs.borrow().is_empty() {
            self.app.quit();
        } else if self.list_box.selected_row().is_none() {
            if let Some(next_row) = self.list_box.row_at_index(0) {
                self.list_box.select_row(Some(&next_row));
            }
        }
    }
}

fn open_settings_window(ui: &Rc<AppUi>, parent: &ApplicationWindow) {
    let current = ui.config.borrow();

    let window = gtk4::Window::builder()
        .title("term-flashy settings")
        .transient_for(parent)
        .modal(true)
        .default_width(320)
        .build();

    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.set_margin_top(12);
    root.set_margin_bottom(12);

    let font_family_row = GtkBox::new(Orientation::Horizontal, 8);
    font_family_row.append(&Label::new(Some("Font family")));
    let font_family_entry = Entry::new();
    font_family_entry.set_hexpand(true);
    font_family_entry.set_placeholder_text(Some("Monospace"));
    font_family_entry.set_text(current.font_family.as_deref().unwrap_or(""));
    font_family_row.append(&font_family_entry);
    root.append(&font_family_row);

    let font_size_row = GtkBox::new(Orientation::Horizontal, 8);
    font_size_row.append(&Label::new(Some("Font size")));
    let font_size_spin = gtk4::SpinButton::with_range(6.0, 72.0, 1.0);
    font_size_spin.set_value(current.font_size);
    font_size_spin.set_hexpand(true);
    font_size_row.append(&font_size_spin);
    root.append(&font_size_row);

    let bell_check = gtk4::CheckButton::with_label("Notify on terminal bell");
    bell_check.set_active(current.trigger_bell);
    root.append(&bell_check);

    let exit_code_check = gtk4::CheckButton::with_label("Notify on non-zero exit code");
    exit_code_check.set_active(current.trigger_exit_code);
    root.append(&exit_code_check);

    let long_command_check = gtk4::CheckButton::with_label("Notify on long-running command");
    long_command_check.set_active(current.trigger_long_command);
    root.append(&long_command_check);

    let threshold_row = GtkBox::new(Orientation::Horizontal, 8);
    threshold_row.append(&Label::new(Some("Long command threshold (seconds)")));
    let threshold_spin = gtk4::SpinButton::with_range(1.0, 3600.0, 1.0);
    threshold_spin.set_value(current.long_command_threshold_secs as f64);
    threshold_spin.set_hexpand(true);
    threshold_row.append(&threshold_spin);
    root.append(&threshold_row);

    let bg_image_row = GtkBox::new(Orientation::Horizontal, 8);
    bg_image_row.append(&Label::new(Some("Background image path")));
    let bg_image_entry = Entry::new();
    bg_image_entry.set_hexpand(true);
    bg_image_entry.set_placeholder_text(Some("(none)"));
    bg_image_entry.set_text(current.background_image.as_deref().unwrap_or(""));
    bg_image_row.append(&bg_image_entry);
    root.append(&bg_image_row);

    let bg_folder_label = Label::new(Some("Background folder (random rotation)"));
    bg_folder_label.set_xalign(0.0);
    root.append(&bg_folder_label);
    let bg_folder_row = GtkBox::new(Orientation::Horizontal, 8);
    let bg_folder_entry = Entry::new();
    bg_folder_entry.set_hexpand(true);
    bg_folder_entry.set_width_chars(10);
    bg_folder_entry.set_placeholder_text(Some("(none)"));
    bg_folder_entry.set_text(current.background_folder.as_deref().unwrap_or(""));
    bg_folder_row.append(&bg_folder_entry);
    let bg_folder_browse = Button::with_label("Browse…");
    let bg_folder_entry_for_browse = bg_folder_entry.clone();
    let window_for_browse = window.clone();
    bg_folder_browse.connect_clicked(move |_| {
        let dialog = gtk4::FileDialog::new();
        let entry = bg_folder_entry_for_browse.clone();
        dialog.select_folder(
            Some(&window_for_browse),
            gio::Cancellable::NONE,
            move |result| {
                if let Ok(folder) = result {
                    if let Some(path) = folder.path() {
                        entry.set_text(&path.to_string_lossy());
                    }
                }
            },
        );
    });
    bg_folder_row.append(&bg_folder_browse);
    root.append(&bg_folder_row);

    let bg_rotate_row = GtkBox::new(Orientation::Horizontal, 8);
    bg_rotate_row.append(&Label::new(Some("Rotate every (seconds)")));
    let bg_rotate_spin = gtk4::SpinButton::with_range(1.0, 86400.0, 1.0);
    bg_rotate_spin.set_value(current.background_rotate_interval_secs as f64);
    bg_rotate_spin.set_hexpand(true);
    bg_rotate_row.append(&bg_rotate_spin);
    root.append(&bg_rotate_row);

    let bg_dim_row = GtkBox::new(Orientation::Horizontal, 8);
    bg_dim_row.append(&Label::new(Some("Background dim (%)")));
    let bg_dim_spin = gtk4::SpinButton::with_range(0.0, 100.0, 5.0);
    bg_dim_spin.set_value(current.background_dim * 100.0);
    bg_dim_spin.set_hexpand(true);
    bg_dim_row.append(&bg_dim_spin);
    root.append(&bg_dim_row);

    let restore_session_check =
        gtk4::CheckButton::with_label("Restore open tabs, directories and pending marks");
    restore_session_check.set_active(current.restore_session);
    root.append(&restore_session_check);

    let scrollback_row = GtkBox::new(Orientation::Horizontal, 8);
    scrollback_row.append(&Label::new(Some("Scrollback lines")));
    let scrollback_spin = gtk4::SpinButton::with_range(100.0, 1_000_000.0, 1000.0);
    scrollback_spin.set_value(if current.scrollback_lines < 0 {
        Config::default().scrollback_lines as f64
    } else {
        current.scrollback_lines as f64
    });
    scrollback_spin.set_hexpand(true);
    scrollback_row.append(&scrollback_spin);
    root.append(&scrollback_row);

    let scrollback_unlimited_check = gtk4::CheckButton::with_label("Unlimited scrollback");
    scrollback_unlimited_check.set_active(current.scrollback_lines < 0);
    scrollback_spin.set_sensitive(current.scrollback_lines >= 0);
    let scrollback_spin_for_toggle = scrollback_spin.clone();
    scrollback_unlimited_check.connect_toggled(move |check| {
        scrollback_spin_for_toggle.set_sensitive(!check.is_active());
    });
    root.append(&scrollback_unlimited_check);

    drop(current);

    let button_row = GtkBox::new(Orientation::Horizontal, 8);
    button_row.set_halign(gtk4::Align::End);
    let cancel_button = Button::with_label("Cancel");
    let save_button = Button::with_label("Save");
    button_row.append(&cancel_button);
    button_row.append(&save_button);
    root.append(&button_row);

    window.set_child(Some(&root));

    let window_for_cancel = window.clone();
    cancel_button.connect_clicked(move |_| {
        window_for_cancel.close();
    });

    let ui_for_save = Rc::clone(ui);
    let window_for_save = window.clone();
    save_button.connect_clicked(move |_| {
        let family = font_family_entry.text();
        let bg_image = bg_image_entry.text();
        let bg_folder = bg_folder_entry.text();
        let new_config = Config {
            font_family: (!family.trim().is_empty()).then(|| family.trim().to_string()),
            font_size: font_size_spin.value(),
            trigger_bell: bell_check.is_active(),
            trigger_exit_code: exit_code_check.is_active(),
            trigger_long_command: long_command_check.is_active(),
            long_command_threshold_secs: threshold_spin.value() as u64,
            background_image: (!bg_image.trim().is_empty()).then(|| bg_image.trim().to_string()),
            background_folder: (!bg_folder.trim().is_empty())
                .then(|| bg_folder.trim().to_string()),
            background_rotate_interval_secs: bg_rotate_spin.value() as u64,
            background_dim: bg_dim_spin.value() / 100.0,
            restore_session: restore_session_check.is_active(),
            scrollback_lines: if scrollback_unlimited_check.is_active() {
                -1
            } else {
                scrollback_spin.value() as i64
            },
        };
        config::save(&new_config);
        *ui_for_save.config.borrow_mut() = new_config;
        ui_for_save.apply_background_settings();
        ui_for_save.apply_scrollback_settings();
        window_for_save.close();
    });

    window.present();
}

fn build_ui(app: &Application) {
    let stack = Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.add_css_class("term-flashy-transparent");

    let list_box = ListBox::new();
    list_box.set_selection_mode(SelectionMode::Single);
    list_box.add_css_class("term-flashy-sidebar");

    let scrolled = ScrolledWindow::new();
    scrolled.set_child(Some(&list_box));
    scrolled.set_hscrollbar_policy(PolicyType::Never);
    scrolled.set_vexpand(true);

    let add_button = Button::with_label("+ New Tab");
    add_button.set_margin_start(4);
    add_button.set_margin_end(4);
    add_button.set_margin_top(4);
    add_button.set_margin_bottom(4);
    add_button.add_css_class("term-flashy-btn");

    let settings_button = Button::with_label("Settings");
    settings_button.set_margin_start(4);
    settings_button.set_margin_end(4);
    settings_button.set_margin_bottom(4);
    settings_button.add_css_class("term-flashy-btn");

    let sidebar = GtkBox::new(Orientation::Vertical, 0);
    sidebar.set_width_request(120);
    sidebar.add_css_class("term-flashy-sidebar");
    sidebar.append(&scrolled);
    sidebar.append(&add_button);
    sidebar.append(&settings_button);

    let background_picture = Picture::new();
    background_picture.set_content_fit(gtk4::ContentFit::Cover);
    background_picture.set_can_target(false);
    background_picture.set_visible(false);

    let content_overlay = Overlay::new();
    content_overlay.add_css_class("term-flashy-transparent");
    content_overlay.set_child(Some(&background_picture));
    content_overlay.add_overlay(&stack);

    let main_paned = Paned::new(Orientation::Horizontal);
    main_paned.set_start_child(Some(&sidebar));
    main_paned.set_end_child(Some(&content_overlay));
    main_paned.set_resize_start_child(false);
    main_paned.set_resize_end_child(true);
    main_paned.set_shrink_start_child(true);
    main_paned.set_shrink_end_child(false);
    main_paned.set_position(load_sidebar_width());

    let window = ApplicationWindow::builder()
        .application(app)
        .title("term-flashy")
        .icon_name("utilities-terminal")
        .default_width(1000)
        .default_height(650)
        .child(&main_paned)
        .build();

    let paned_for_shutdown = main_paned.clone();
    app.connect_shutdown(move |_app| {
        save_sidebar_width(paned_for_shutdown.position());
    });

    let ui = Rc::new(AppUi {
        app: app.clone(),
        window: window.clone(),
        stack,
        list_box: list_box.clone(),
        background_picture: background_picture.clone(),
        tabs: RefCell::new(HashMap::new()),
        next_id: Cell::new(0),
        active_rename: RefCell::new(None),
        config: RefCell::new(config::load()),
        pending_session_save: Cell::new(None),
        wallpaper_queue: RefCell::new(Vec::new()),
        wallpaper_timer: Cell::new(None),
        suppress_notify_clear: Cell::new(None),
    });
    ui.restart_wallpaper_rotation();

    // Flush the session synchronously on a clean shutdown: a pending
    // debounced timeout from schedule_session_save() won't get a chance to
    // fire after quit(). A crash (kill -9, no shutdown signal) instead
    // relies on the debounced writes already on disk from the last ~500ms
    // of activity.
    let ui_for_session_shutdown = Rc::clone(&ui);
    app.connect_shutdown(move |_app| {
        if let Some(source) = ui_for_session_shutdown.pending_session_save.take() {
            source.remove();
        }
        ui_for_session_shutdown.save_session_now();
    });

    let ui_for_selection = Rc::clone(&ui);
    list_box.connect_row_selected(move |_, row| {
        if let Some(row) = row {
            let guard = ui_for_selection.suppress_notify_clear.take();
            let suppressed = guard.as_ref() == Some(row);
            if suppressed {
                // Consumed: this was the spurious reselect right after
                // marking this row pending, not a real navigation away
                // and back. Leave "notify" alone.
            } else {
                ui_for_selection.suppress_notify_clear.set(guard);
                row.remove_css_class("notify");
            }
            ui_for_selection
                .stack
                .set_visible_child_name(&row.widget_name());
            let stack_for_focus = ui_for_selection.stack.clone();
            glib::idle_add_local_once(move || {
                if let Some(child) = stack_for_focus.visible_child() {
                    child.grab_focus();
                }
            });
            ui_for_selection.schedule_session_save();
        }
    });

    let ui_for_add = Rc::clone(&ui);
    add_button.connect_clicked(move |_| {
        ui_for_add.add_tab(None);
    });

    let ui_for_settings = Rc::clone(&ui);
    let window_for_settings = window.clone();
    settings_button.connect_clicked(move |_| {
        open_settings_window(&ui_for_settings, &window_for_settings);
    });

    let outside_click_gesture = GestureClick::new();
    outside_click_gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let ui_for_outside_click = Rc::clone(&ui);
    let list_box_for_outside_click = list_box.clone();
    outside_click_gesture.connect_pressed(move |_gesture, _n_press, x, y| {
        let Some(active_entry) = ui_for_outside_click
            .active_rename
            .borrow()
            .as_ref()
            .map(|(entry, _)| entry.clone())
        else {
            return;
        };
        let hit = list_box_for_outside_click.pick(x, y, gtk4::PickFlags::DEFAULT);
        let is_inside_entry = hit
            .map(|w| {
                w == active_entry.clone().upcast::<gtk4::Widget>() || w.is_ancestor(&active_entry)
            })
            .unwrap_or(false);
        if !is_inside_entry {
            let finish = ui_for_outside_click
                .active_rename
                .borrow_mut()
                .take()
                .map(|(_, f)| f);
            if let Some(finish) = finish {
                finish(true);
            }
        }
    });
    list_box.add_controller(outside_click_gesture);

    let window_key_controller = EventControllerKey::new();
    window_key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let ui_for_shortcut = Rc::clone(&ui);
    window_key_controller.connect_key_pressed(move |_controller, keyval, _keycode, state| {
        let ctrl_shift = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
        if state.contains(ctrl_shift) {
            match keyval {
                gdk::Key::T | gdk::Key::t => {
                    ui_for_shortcut.add_tab(None);
                    return glib::Propagation::Stop;
                }
                gdk::Key::Up => {
                    ui_for_shortcut.select_adjacent_tab(-1);
                    return glib::Propagation::Stop;
                }
                gdk::Key::Down => {
                    ui_for_shortcut.select_adjacent_tab(1);
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
        }
        glib::Propagation::Proceed
    });
    window.add_controller(window_key_controller);

    let saved_session = if ui.config.borrow().restore_session {
        session::load()
    } else {
        session::SessionState::default()
    };
    if saved_session.tabs.is_empty() {
        ui.add_tab(None);
    } else {
        let pending: Vec<bool> = saved_session.tabs.iter().map(|tab| tab.notify).collect();
        for tab in saved_session.tabs {
            ui.add_tab(Some(tab));
        }
        let active_row = ui
            .list_box
            .row_at_index(saved_session.active as i32)
            .or_else(|| ui.list_box.row_at_index(0));
        ui.list_box.select_row(active_row.as_ref());
        // Only after the startup selection: adding a row selects it, and
        // row-selected strips "notify".
        for (index, _) in pending.iter().enumerate().filter(|(_, mark)| **mark) {
            if let Some(row) = ui.list_box.row_at_index(index as i32) {
                row.add_css_class("notify");
            }
        }
    }

    window.present();
}

const HELP_TEXT: &str = include_str!("../HELP.txt");

fn main() {
    let mut args = std::env::args().skip(1);
    if args.any(|arg| arg == "--help" || arg == "-h") {
        print!("{HELP_TEXT}");
        return;
    }

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(SIDEBAR_CSS);
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().expect("no default display"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
    app.connect_activate(build_ui);
    app.run();
}
