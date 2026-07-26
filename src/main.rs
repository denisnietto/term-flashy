mod config;

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
.term-flashy-sidebar row.notify {
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

fn mark_tab_notify(stack: &Stack, page_name: &str, row: &gtk4::ListBoxRow) {
    if stack.visible_child_name().as_deref() != Some(page_name) {
        row.add_css_class("notify");
    }
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
}

struct AppUi {
    app: Application,
    stack: Stack,
    list_box: ListBox,
    background_picture: Picture,
    tabs: RefCell<HashMap<u32, TabEntry>>,
    next_id: Cell<u32>,
    active_rename: RefCell<Option<(Entry, Box<dyn Fn(bool)>)>>,
    config: RefCell<Config>,
}

fn apply_background_image(picture: &Picture, config: &Config) {
    match &config.background_image {
        Some(path) => {
            picture.set_filename(Some(path));
            picture.set_visible(true);
        }
        None => {
            picture.set_visible(false);
        }
    }
}

impl AppUi {
    fn add_tab(self: &Rc<Self>) {
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
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        terminal.spawn_async(
            PtyFlags::DEFAULT,
            None,
            &[&shell],
            &[],
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
            glib::Propagation::Proceed
        });
        terminal.add_controller(key_controller);

        self.stack.add_named(&terminal, Some(&page_name));

        let label = Label::new(Some(&format!("Tab {}", id + 1)));
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
                    }
                }
                entry_for_finish.set_visible(false);
                label_for_finish.set_visible(true);
                ui_for_finish.active_rename.borrow_mut().take();
            })
        };

        let click_gesture = GestureClick::new();
        let entry_for_click = entry.clone();
        let label_for_click = label.clone();
        let ui_for_click = Rc::clone(self);
        let finish_for_click = Rc::clone(&finish_rename);
        click_gesture.connect_pressed(move |gesture, n_press, _x, _y| {
            if n_press == 2 {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                entry_for_click.set_text(&label_for_click.text());
                label_for_click.set_visible(false);
                entry_for_click.set_visible(true);
                entry_for_click.grab_focus();
                entry_for_click.select_region(0, -1);
                let finish_for_stored = Rc::clone(&finish_for_click);
                *ui_for_click.active_rename.borrow_mut() = Some((
                    entry_for_click.clone(),
                    Box::new(move |commit| finish_for_stored(commit)),
                ));
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

        let ui_for_close = Rc::clone(self);
        close_button.connect_clicked(move |_| {
            ui_for_close.close_tab(id);
        });

        let ui_for_exit = Rc::clone(self);
        terminal.connect_child_exited(move |_terminal, _status| {
            ui_for_exit.close_tab(id);
        });

        let stack_for_bell = self.stack.clone();
        let row_for_bell = row.clone();
        let page_name_for_bell = page_name.clone();
        let trigger_bell = self.config.borrow().trigger_bell;
        terminal.connect_bell(move |_terminal| {
            if trigger_bell {
                mark_tab_notify(&stack_for_bell, &page_name_for_bell, &row_for_bell);
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

        let stack_for_exit = self.stack.clone();
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
                mark_tab_notify(&stack_for_exit, &page_name_for_exit, &row_for_exit);
            }
        });

        self.stack.set_visible_child_name(&page_name);
        terminal.grab_focus();
        self.tabs.borrow_mut().insert(id, TabEntry { terminal, row });
    }

    fn apply_background_settings(self: &Rc<Self>) {
        let config = self.config.borrow();
        apply_background_image(&self.background_picture, &config);
        let alpha = config.terminal_background_alpha();
        for entry in self.tabs.borrow().values() {
            entry
                .terminal
                .set_colors(None, Some(&gdk::RGBA::new(0.0, 0.0, 0.0, alpha)), &[]);
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

    fn close_tab(self: &Rc<Self>, id: u32) {
        let entry = self.tabs.borrow_mut().remove(&id);
        if let Some(entry) = entry {
            self.stack.remove(&entry.terminal);
            self.list_box.remove(&entry.row);
        }
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

    let bg_dim_row = GtkBox::new(Orientation::Horizontal, 8);
    bg_dim_row.append(&Label::new(Some("Background dim (%)")));
    let bg_dim_spin = gtk4::SpinButton::with_range(0.0, 100.0, 5.0);
    bg_dim_spin.set_value(current.background_dim * 100.0);
    bg_dim_spin.set_hexpand(true);
    bg_dim_row.append(&bg_dim_spin);
    root.append(&bg_dim_row);

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
        let new_config = Config {
            font_family: (!family.trim().is_empty()).then(|| family.trim().to_string()),
            font_size: font_size_spin.value(),
            trigger_bell: bell_check.is_active(),
            trigger_exit_code: exit_code_check.is_active(),
            trigger_long_command: long_command_check.is_active(),
            long_command_threshold_secs: threshold_spin.value() as u64,
            background_image: (!bg_image.trim().is_empty()).then(|| bg_image.trim().to_string()),
            background_dim: bg_dim_spin.value() / 100.0,
        };
        config::save(&new_config);
        *ui_for_save.config.borrow_mut() = new_config;
        ui_for_save.apply_background_settings();
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
        stack,
        list_box: list_box.clone(),
        background_picture: background_picture.clone(),
        tabs: RefCell::new(HashMap::new()),
        next_id: Cell::new(0),
        active_rename: RefCell::new(None),
        config: RefCell::new(config::load()),
    });
    apply_background_image(&ui.background_picture, &ui.config.borrow());

    let ui_for_selection = Rc::clone(&ui);
    list_box.connect_row_selected(move |_, row| {
        if let Some(row) = row {
            row.remove_css_class("notify");
            ui_for_selection
                .stack
                .set_visible_child_name(&row.widget_name());
            let stack_for_focus = ui_for_selection.stack.clone();
            glib::idle_add_local_once(move || {
                if let Some(child) = stack_for_focus.visible_child() {
                    child.grab_focus();
                }
            });
        }
    });

    let ui_for_add = Rc::clone(&ui);
    add_button.connect_clicked(move |_| {
        ui_for_add.add_tab();
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
                    ui_for_shortcut.add_tab();
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

    ui.add_tab();

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
