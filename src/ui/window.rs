//! Main application window: a three-page `gtk::Stack` («Саундборд»,
//! «Настройки», «Дебаг») switched from the header bar, a window-level
//! failure banner, the virtual mic VU meter, the clip grid and the
//! playback controls (volume, stop-all, behavior toggles, doctor checks).
//!
//! Threading rule (see [`super::worker`]): nothing blocking happens here —
//! the window only sends [`WorkerCmd`]s to the audio worker and renders
//! [`GuiMsg`]s arriving over glib channels. The clip grid scans `clips_dir`
//! on the UI thread deliberately: `Clips::scan` is a single small readdir,
//! no audio APIs involved.
//!
//! Config flow: toggles and the volume slider mutate the shared config
//! under the lock (in-memory, instant for the worker), then either send
//! `SaveConfig` + `EnsureGraph` (mic_mix/monitoring change loopbacks, so
//! the graph must reconcile) or `SaveConfig` only (interrupt_mode). The
//! slider sends `SaveVolume` per tick — the worker persists the config with
//! a ~300 ms trailing debounce, because hotkey-spawned `soundboard play`
//! processes read the config volume and must agree with the slider.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use async_channel::Receiver;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, FlowBox, Frame, Image, Label, LevelBar, Orientation, Scale,
    ScrolledWindow, Stack, StackSwitcher,
};
use libadwaita as adw;
use libadwaita::prelude::*;

use super::kglobalaccel;
use super::tray;
use super::worker::{GuiMsg, SharedConfig, Worker, WorkerCmd, lock_config};
use crate::clips::Clips;
use crate::config::Config;
use crate::graph::{CONF_HINT, DoctorReport, Report};
use crate::hotkeys::{self, Registration};

/// `gtk::Stack` page names (also used for programmatic switches).
const PAGE_BOARD: &str = "board";
const PAGE_SETTINGS: &str = "settings";
const PAGE_DEBUG: &str = "debug";

/// Summary line for a doctor report.
pub(crate) fn doctor_summary_line(report: &DoctorReport) -> &'static str {
    if report.all_passed() {
        "Граф: OK"
    } else {
        "Граф: проблемы"
    }
}

/// Summary line after a reconcile run.
pub(crate) fn reconcile_summary_line(report: &Report) -> &'static str {
    if report.warnings.is_empty() {
        "Граф: OK"
    } else {
        "Граф: проблемы"
    }
}

/// Status icon per doctor check (themed symbolic icons).
pub(crate) fn check_icon_name(passed: bool) -> &'static str {
    if passed {
        "emblem-ok-symbolic"
    } else {
        "dialog-error-symbolic"
    }
}

/// Style class matching [`check_icon_name`].
pub(crate) fn check_css_class(passed: bool) -> &'static str {
    if passed { "success" } else { "error" }
}

/// Raw meter peak to a `LevelBar` value (clamped into 0.0..=1.0).
pub(crate) fn meter_level(value: f32) -> f64 {
    f64::from(value).clamp(0.0, 1.0)
}

/// Maps the 0..=100 slider position to the config volume factor
/// (0.0..=1.0, clamped — the scale cannot exceed the range, but stay total).
pub(crate) fn slider_to_volume_factor(percent: f64) -> f32 {
    (percent / 100.0).clamp(0.0, 1.0) as f32
}

/// Maps the config volume factor (0.0..=1.0, clamped against hand-edited
/// out-of-range values) to the 0..=100 slider position.
pub(crate) fn volume_factor_to_slider(volume: f32) -> f64 {
    f64::from(volume).clamp(0.0, 1.0) * 100.0
}

/// Placeholder text for an empty (or absent) clips directory.
pub(crate) fn clips_placeholder_text(dir: &Path) -> String {
    format!(
        "Папка клипов пуста: {} — положите mp3/ogg/wav/flac",
        dir.display()
    )
}

/// True when a clip matches the live search `query`: case-insensitive
/// substring on the clip name OR the category name; an empty (or blank)
/// query matches everything.
pub(crate) fn clip_matches_query(name: &str, category: &str, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty() || name.to_lowercase().contains(&q) || category.to_lowercase().contains(&q)
}

/// True when `name` is in the playing set (drives the button highlight).
pub(crate) fn clip_is_playing(playing: &[String], name: &str) -> bool {
    playing.iter().any(|n| n == name)
}

/// Russian state line for a desktop-file registration state.
pub(crate) fn hotkey_state_line(state: Registration) -> &'static str {
    match state {
        Registration::Current => "Хоткеи зарегистрированы",
        Registration::None => "Хоткеи не зарегистрированы",
        Registration::Stale => "Хоткеи устарели — набор клипов или клавиши изменились",
    }
}

/// Label of the register/refresh button for a registration state.
pub(crate) fn enable_button_label(state: Registration) -> &'static str {
    match state {
        Registration::None => "Включить хоткеи",
        Registration::Current | Registration::Stale => "Обновить хоткеи",
    }
}

/// Line shown when there are no clips to bind keys to.
pub(crate) const NO_CLIPS_LINE: &str = "Клипы не найдены — привязывать нечего";

/// User applications directory for the hotkey desktop files (the same path
/// the CLI `hotkeys --write` writes to: `XDG_DATA_HOME/applications`).
fn applications_dir() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|dir| dir.join("applications"))
}

/// The «Глобальные хоткеи» section of the settings page: the clip → key →
/// command table, the registration state line (read-only scan of the
/// applications directory) and the enable/refresh/disable buttons. The
/// blocking parts (desktop file writes, `kbuildsycoca6`) run on
/// `gio::spawn_blocking`; `refresh` itself only does a small readdir plus a
/// few tiny file reads — the same UI-thread precedent as the clip grid scan.
struct HotkeysSection {
    container: GtkBox,
    table: Label,
    state: Label,
    register_btn: Button,
    disable_btn: Button,
}

/// Caption explaining what the global hotkeys do.
const HOTKEYS_CAPTION: &str = "Работают в любом приложении, даже когда окно закрыто; клип играет отдельным процессом soundboard play";

impl HotkeysSection {
    fn new() -> Self {
        let table = Label::builder()
            .label("…")
            .halign(gtk4::Align::Start)
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .build();
        // GtkLabel lost its monospace property in GTK4; the Adwaita
        // `.monospace` style class is the supported way for labels.
        table.add_css_class("monospace");
        let state = Label::builder()
            .halign(gtk4::Align::Start)
            .wrap(true)
            .build();
        let register_btn = Button::with_label("Включить хоткеи");
        register_btn.add_css_class("suggested-action");
        let disable_btn = Button::with_label("Выключить");
        disable_btn.add_css_class("destructive-action");
        let buttons = GtkBox::new(Orientation::Horizontal, 8);
        buttons.append(&register_btn);
        buttons.append(&disable_btn);
        let caption = Label::builder()
            .label(HOTKEYS_CAPTION)
            .halign(gtk4::Align::Start)
            .wrap(true)
            .build();
        caption.add_css_class("dim-label");
        let container = GtkBox::new(Orientation::Vertical, 6);
        container.append(&heading("Глобальные хоткеи"));
        container.append(&table);
        container.append(&state);
        container.append(&buttons);
        container.append(&caption);
        Self {
            container,
            table,
            state,
            register_btn,
            disable_btn,
        }
    }

    /// A shared handle to the section container for page assembly.
    fn container(&self) -> GtkBox {
        self.container.clone()
    }

    /// Recomputes the table and the registration state from the current
    /// clips and the applications directory.
    fn refresh(&self, cfg: &Config) {
        let clips = Clips::scan(&cfg.clips_dir);
        let names = clips.list();
        let entries = hotkeys::assign_keys(&names);
        self.table.set_text(&hotkeys::render_table(
            &entries,
            names.len().saturating_sub(entries.len()),
        ));
        let desired = hotkeys::build_files(&entries, &hotkeys::exec_binary());
        let scan = applications_dir()
            .ok_or_else(|| anyhow::anyhow!("каталог приложений не определён"))
            .and_then(|dir| hotkeys::read_managed_files(&dir));
        match scan {
            Ok(files) => self.apply_state(
                hotkeys::compare_registration(&files, &desired),
                !clips.is_empty(),
            ),
            Err(_) => self.show_scan_error(),
        }
    }

    /// Applies a computed registration state to the state line and buttons.
    fn apply_state(&self, state: Registration, clips_found: bool) {
        self.state.set_text(if clips_found {
            hotkey_state_line(state)
        } else {
            NO_CLIPS_LINE
        });
        self.state.remove_css_class("success");
        self.state.remove_css_class("dim-label");
        if state == Registration::Current && clips_found {
            self.state.add_css_class("success");
        } else {
            self.state.add_css_class("dim-label");
        }
        self.register_btn.set_label(enable_button_label(state));
        self.register_btn.set_sensitive(clips_found);
        self.disable_btn.set_sensitive(state != Registration::None);
    }

    /// Fallback when the applications directory cannot be read.
    fn show_scan_error(&self) {
        self.state
            .set_text("Не удалось проверить состояние хоткеев");
        self.state.remove_css_class("success");
        self.state.remove_css_class("dim-label");
        self.register_btn.set_sensitive(true);
        self.disable_btn.set_sensitive(false);
    }
}

/// Shows a short non-blocking notification at the top of the window.
fn show_toast(overlay: &adw::ToastOverlay, text: &str) {
    let toast = adw::Toast::new(text);
    toast.set_timeout(6);
    overlay.add_toast(toast);
}

/// Vertical page container with the common window margins.
fn page_box() -> GtkBox {
    let box_ = GtkBox::new(Orientation::Vertical, 12);
    box_.set_margin_top(12);
    box_.set_margin_bottom(12);
    box_.set_margin_start(12);
    box_.set_margin_end(12);
    box_
}

/// A heading label (section title).
fn heading(text: &str) -> Label {
    let label = Label::builder()
        .label(text)
        .halign(gtk4::Align::Start)
        .build();
    label.add_css_class("heading");
    label
}

/// One visible category block: a bold header label plus its own `FlowBox`
/// of clip buttons. `items` keeps (name, button) pairs in insertion order
/// so the live search filter can walk the section without a widget search.
struct CategorySection {
    category: String,
    header: Label,
    flow: FlowBox,
    items: Vec<(String, Button)>,
}

/// The clip grid section: a search field plus one [`CategorySection`] per
/// category (bold header + `FlowBox` of clip buttons), the empty-dir
/// placeholder, the «no match» line and scan warnings. `buttons` is shared
/// with the worker message loop so [`GuiMsg::PlayingSet`] can toggle the
/// highlight. The search filters live (on the UI thread — a substring check
/// over at most a hundred buttons per keystroke is cheap); the current query
/// survives «Обновить» rescans.
struct ClipGrid {
    search: gtk4::SearchEntry,
    sections_box: GtkBox,
    placeholder: Label,
    no_match: Label,
    warnings_label: Label,
    sections: RefCell<Vec<CategorySection>>,
    buttons: Rc<RefCell<HashMap<String, Button>>>,
    query: RefCell<String>,
}

impl ClipGrid {
    fn new() -> Self {
        let search = gtk4::SearchEntry::new();
        search.set_placeholder_text(Some("Поиск…"));
        search.set_hexpand(true);
        let sections_box = GtkBox::new(Orientation::Vertical, 12);
        sections_box.set_visible(false);
        let placeholder = Label::builder()
            .label("Клипы: сканирование…")
            .halign(gtk4::Align::Start)
            .wrap(true)
            .sensitive(false)
            .visible(false)
            .build();
        let no_match = Label::builder()
            .label("Ничего не найдено по запросу")
            .halign(gtk4::Align::Start)
            .wrap(true)
            .sensitive(false)
            .visible(false)
            .build();
        no_match.add_css_class("dim-label");
        let warnings_label = Label::builder()
            .halign(gtk4::Align::Start)
            .wrap(true)
            .visible(false)
            .build();
        warnings_label.add_css_class("dim-label");
        ClipGrid {
            search,
            sections_box,
            placeholder,
            no_match,
            warnings_label,
            sections: RefCell::new(Vec::new()),
            buttons: Rc::new(RefCell::new(HashMap::new())),
            query: RefCell::new(String::new()),
        }
    }

    /// Widget holding the whole section (search, grid, placeholders,
    /// warnings).
    fn container(&self) -> GtkBox {
        let box_ = GtkBox::new(Orientation::Vertical, 6);
        box_.append(&self.search);
        box_.append(&self.sections_box);
        box_.append(&self.placeholder);
        box_.append(&self.no_match);
        box_.append(&self.warnings_label);
        box_
    }

    /// Applies the current search query to the rendered sections: buttons
    /// outside the filter hide, empty categories hide entirely, and a
    /// «ничего не найдено» line appears when nothing matches a non-empty
    /// query.
    fn apply_filter(&self) {
        let query = self.query.borrow().clone();
        let mut any_visible = false;
        for section in self.sections.borrow().iter() {
            let mut visible_count = 0usize;
            for (name, btn) in &section.items {
                let visible = clip_matches_query(name, &section.category, &query);
                btn.set_visible(visible);
                if visible {
                    visible_count += 1;
                }
            }
            let section_visible = visible_count > 0;
            section.header.set_visible(section_visible);
            section.flow.set_visible(section_visible);
            any_visible |= section_visible;
        }
        self.no_match
            .set_visible(!query.trim().is_empty() && !any_visible);
    }

    /// Rebuilds the grid from a fresh directory scan (see the module docs:
    /// cheap, UI-thread-safe), grouped by category, then re-applies the
    /// current search query.
    fn reload(&self, cfg: &Config, worker: &Worker) {
        let clips = Clips::scan(&cfg.clips_dir);
        while let Some(child) = self.sections_box.first_child() {
            self.sections_box.remove(&child);
        }
        self.sections.borrow_mut().clear();
        self.buttons.borrow_mut().clear();
        for group in clips.entries().chunk_by(|a, b| a.category == b.category) {
            let category = group[0].category;
            let flow = FlowBox::new();
            flow.set_selection_mode(gtk4::SelectionMode::None);
            flow.set_column_spacing(6);
            flow.set_row_spacing(6);
            flow.set_min_children_per_line(2);
            flow.set_max_children_per_line(6);
            flow.set_homogeneous(true);
            flow.set_hexpand(true);
            let mut items = Vec::new();
            for entry in group {
                let name = entry.name;
                let file_name = entry
                    .path
                    .file_name()
                    .map_or_else(|| name.to_string(), |n| n.to_string_lossy().into_owned());
                let btn = Button::with_label(name);
                let tooltip = if entry.category == crate::clips::UNCATEGORIZED {
                    file_name
                } else {
                    format!("{}/{}", entry.category, file_name)
                };
                btn.set_tooltip_text(Some(&tooltip));
                if let Some(inner) = btn.child().and_downcast::<Label>() {
                    inner.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                }
                {
                    let worker = worker.clone();
                    let clip_name = name.to_string();
                    btn.connect_clicked(move |_| {
                        worker.send(WorkerCmd::Play {
                            name: clip_name.clone(),
                        });
                    });
                }
                flow.append(&btn);
                self.buttons
                    .borrow_mut()
                    .insert(name.to_string(), btn.clone());
                items.push((name.to_string(), btn));
            }
            let section_box = GtkBox::new(Orientation::Vertical, 6);
            let header = heading(category);
            section_box.append(&header);
            section_box.append(&flow);
            self.sections_box.append(&section_box);
            self.sections.borrow_mut().push(CategorySection {
                category: category.to_string(),
                header,
                flow,
                items,
            });
        }
        let empty = clips.is_empty();
        self.sections_box.set_visible(!empty);
        self.placeholder.set_visible(empty);
        if empty {
            self.placeholder
                .set_text(&clips_placeholder_text(&cfg.clips_dir));
        }
        let warnings = clips.warnings();
        self.warnings_label.set_visible(!warnings.is_empty());
        if !warnings.is_empty() {
            self.warnings_label.set_text(&warnings.join("\n"));
        }
        self.apply_filter();
    }

    /// Highlights the buttons of currently playing clips.
    fn set_playing(&self, playing: &[String]) {
        let buttons = self.buttons.borrow();
        for (name, btn) in buttons.iter() {
            if clip_is_playing(playing, name) {
                btn.add_css_class("suggested-action");
            } else {
                btn.remove_css_class("suggested-action");
            }
        }
    }
}

/// Builds the window, wires the worker channels and presents it. The
/// returned window is owned by the application once presented; the caller
/// may drop the handle. `tray` is the system tray handle (empty when the
/// tray is unavailable — then closing the window really quits); `tray_rx`
/// carries the tray menu commands drained on the main loop.
pub(crate) fn build(
    app: &adw::Application,
    ui_rx: Receiver<GuiMsg>,
    meter_rx: Receiver<f32>,
    worker: &Worker,
    cfg: &SharedConfig,
    tray: Rc<RefCell<Option<tray::TrayHandle>>>,
    tray_rx: Receiver<tray::TrayCmd>,
) -> adw::ApplicationWindow {
    let current = lock_config(cfg).clone();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Soundboard")
        .default_width(720)
        .default_height(640)
        .build();

    // Window/taskbar icon: the hicolor icon installed by `just
    // install-desktop`. When it is not installed, keep the theme default —
    // a missing icon name would render as a broken image glyph.
    let has_app_icon = gtk4::gdk::Display::default()
        .map(|display| gtk4::IconTheme::for_display(&display).has_icon(super::APP_ICON))
        .unwrap_or(false);
    if has_app_icon {
        window.set_icon_name(Some(super::APP_ICON));
    }

    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    // Window-level graph health banner: revealed ONLY when the last doctor
    // status or reconcile has failures; «Открыть» jumps to the debug page.
    // The header switcher + banner are the only chrome above the stack, so
    // graph health stays visible from every page.
    let banner = adw::Banner::builder()
        .title("Граф: проблемы — см. вкладку Дебаг")
        .button_label("Открыть")
        .revealed(false)
        .build();
    toolbar.add_top_bar(&banner);

    // Summary line of the latest doctor/reconcile/error report (debug page).
    let summary = Label::builder()
        .label("Автопроверка при запуске…")
        .halign(gtk4::Align::Start)
        .build();
    summary.add_css_class("heading");

    // Doctor checks of the «Дебаг» page: merged rows (✓/✗ icon + name + the
    // grey detail line inline — there is no separate details tab anymore).
    let checks = GtkBox::new(Orientation::Vertical, 0);
    let checks_frame = Frame::new(None);
    checks_frame.set_child(Some(&checks));
    let placeholder_checks = Label::builder()
        .label("Ожидание результатов проверки…")
        .halign(gtk4::Align::Start)
        .sensitive(false)
        .build();
    checks.append(&placeholder_checks);

    // Raw reconcile warnings («Починить граф» ran with warnings), if any.
    let raw_warnings = Label::builder()
        .halign(gtk4::Align::Start)
        .wrap(true)
        .visible(false)
        .build();
    raw_warnings.add_css_class("dim-label");

    // Action buttons of the «Дебаг» page.
    let debug_buttons = GtkBox::new(Orientation::Horizontal, 8);
    let btn_doctor = Button::with_label("Проверить");
    let btn_repair = Button::with_label("Починить граф");
    btn_repair.add_css_class("suggested-action");
    let btn_test = Button::with_label("Тест");
    debug_buttons.append(&btn_doctor);
    debug_buttons.append(&btn_repair);
    debug_buttons.append(&btn_test);

    // VU meter section («Саундборд» page only).
    let meter_caption = heading("Виртуальный микрофон");
    let level_bar = LevelBar::builder()
        .min_value(0.0)
        .max_value(1.0)
        .mode(gtk4::LevelBarMode::Continuous)
        .height_request(12)
        .hexpand(true)
        .value(0.0)
        .build();
    let conf_hint = Label::builder()
        .label(CONF_HINT)
        .halign(gtk4::Align::Start)
        .wrap(true)
        .build();
    conf_hint.add_css_class("dim-label");

    // Clips section («Саундборд»): caption + refresh + stop-all + grid.
    let grid = Rc::new(ClipGrid::new());
    {
        let grid = grid.clone();
        let search = grid.search.clone();
        search.connect_search_changed(move |entry| {
            *grid.query.borrow_mut() = entry.text().to_string();
            grid.apply_filter();
        });
    }
    let btn_refresh = Button::with_label("Обновить");
    btn_refresh.add_css_class("flat");
    let btn_stop_all = Button::with_label("Стоп всё");
    btn_stop_all.add_css_class("destructive-action");
    let header_spacer = GtkBox::new(Orientation::Horizontal, 0);
    header_spacer.set_hexpand(true);
    let clips_header = GtkBox::new(Orientation::Horizontal, 8);
    clips_header.append(&heading("Клипы"));
    clips_header.append(&btn_refresh);
    clips_header.append(&header_spacer);
    clips_header.append(&btn_stop_all);

    // Controls: volume slider («Саундборд»).
    let volume_caption = heading("Громкость");
    let volume_scale = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
    volume_scale.set_value(volume_factor_to_slider(current.volume));
    volume_scale.set_digits(0);
    volume_scale.set_hexpand(true);

    // Controls: behavior toggles («Настройки»).
    let sw_mic_mix = adw::SwitchRow::builder()
        .title("Микс с микрофоном")
        .subtitle("Пропускать настоящий микрофон в виртуальный")
        .active(current.mic_mix)
        .build();
    let sw_monitoring = adw::SwitchRow::builder()
        .title("Слышать себя")
        .subtitle("Дублировать клипы в реальные колонки")
        .active(current.monitoring)
        .build();
    let sw_interrupt = adw::SwitchRow::builder()
        .title("Перезапускать клип повторным нажатием")
        .subtitle("Иначе повторный клик играет копию поверх")
        .active(current.interrupt_mode)
        .build();
    let toggles = adw::PreferencesGroup::new();
    toggles.add(&sw_mic_mix);
    toggles.add(&sw_monitoring);
    toggles.add(&sw_interrupt);

    // Clips directory row («Настройки»): display + xdg-open button.
    let dir_row = adw::ActionRow::builder()
        .title("Папка клипов")
        .subtitle(current.clips_dir.display().to_string())
        .build();
    let btn_open_dir = Button::with_label("Открыть папку");
    btn_open_dir.add_css_class("flat");
    dir_row.add_suffix(&btn_open_dir);
    let dirs_group = adw::PreferencesGroup::new();
    dirs_group.add(&dir_row);

    // Global hotkeys management («Настройки»).
    let hotkeys_section = Rc::new(HotkeysSection::new());

    // --- Page «Саундборд»: daily use -------------------------------------
    let board = page_box();
    board.append(&clips_header);
    board.append(&grid.container());
    board.append(&volume_caption);
    board.append(&volume_scale);
    board.append(&meter_caption);
    board.append(&level_bar);
    board.append(&conf_hint);

    // --- Page «Настройки» -------------------------------------------------
    let settings = page_box();
    settings.append(&toggles);
    settings.append(&hotkeys_section.container());
    settings.append(&dirs_group);

    // --- Page «Дебаг»: doctor checks merged with details ------------------
    let debug = page_box();
    debug.append(&summary);
    debug.append(&debug_buttons);
    debug.append(&checks_frame);
    debug.append(&raw_warnings);

    // The stack itself; the switcher lives in the header bar title area.
    let stack = Stack::new();
    // Pages have very different heights — do not stretch them to the tallest.
    stack.set_vhomogeneous(false);
    stack.add_titled(&board, Some(PAGE_BOARD), "Саундборд");
    stack.add_titled(&settings, Some(PAGE_SETTINGS), "Настройки");
    stack.add_titled(&debug, Some(PAGE_DEBUG), "Дебаг");
    let switcher = StackSwitcher::new();
    switcher.set_stack(Some(&stack));
    header.set_title_widget(Some(&switcher));

    // Pages may overflow the window: scroll vertically instead of clipping.
    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .child(&stack)
        .build();
    scroll.set_vexpand(true);

    // Toasts surface worker errors without blocking the UI.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&scroll));
    toolbar.set_content(Some(&toast_overlay));
    window.set_content(Some(&toolbar));

    // Banner «Открыть» → the debug page.
    {
        let stack = stack.clone();
        banner.connect_button_clicked(move |_banner| {
            stack.set_visible_child_name(PAGE_DEBUG);
        });
    }

    // Buttons: send commands; immediate text feedback on the summary line.
    {
        let summary = summary.clone();
        let worker = worker.clone();
        btn_doctor.connect_clicked(move |_btn| {
            set_summary(&summary, "Проверка…", None);
            worker.send(WorkerCmd::Doctor);
        });
    }
    {
        let summary = summary.clone();
        let worker = worker.clone();
        btn_repair.connect_clicked(move |_btn| {
            set_summary(&summary, "Чиню граф…", None);
            worker.send(WorkerCmd::EnsureGraph);
        });
    }
    {
        let worker = worker.clone();
        btn_test.connect_clicked(move |_btn| {
            worker.send(WorkerCmd::PlayTest);
        });
    }
    {
        let worker = worker.clone();
        btn_stop_all.connect_clicked(move |_btn| {
            worker.send(WorkerCmd::Stop { name: None });
        });
    }
    {
        let grid = grid.clone();
        let hotkeys = hotkeys_section.clone();
        let cfg = cfg.clone();
        let worker = worker.clone();
        btn_refresh.connect_clicked(move |_btn| {
            let current = lock_config(&cfg);
            grid.reload(&current, &worker);
            hotkeys.refresh(&current);
        });
    }

    // «Открыть папку»: xdg-open with an argv array (no shell); the child is
    // reaped on a throwaway thread so the UI thread never blocks on wait().
    {
        let cfg = cfg.clone();
        let toast_overlay = toast_overlay.clone();
        btn_open_dir.connect_clicked(move |_btn| {
            let dir = lock_config(&cfg).clips_dir.clone();
            match std::process::Command::new("xdg-open").arg(&dir).spawn() {
                Ok(child) => {
                    std::thread::spawn(move || {
                        let mut child = child;
                        let _ = child.wait();
                    });
                }
                Err(e) => {
                    show_toast(&toast_overlay, &format!("Не удалось открыть папку: {e}"));
                }
            }
        });
    }

    // «Включить/Обновить хоткеи»: the exact CLI `hotkeys --write` code path
    // (assign_keys → build_files → write_desktop_files: idempotent, backups,
    // foreign files never touched), then kbuildsycoca6 so Plasma picks the
    // files up live. Both block: they run on gio::spawn_blocking.
    {
        let section = hotkeys_section.clone();
        let cfg = cfg.clone();
        let toast_overlay = toast_overlay.clone();
        hotkeys_section.register_btn.connect_clicked(move |_btn| {
            let cfg = lock_config(&cfg).clone();
            let clips = Clips::scan(&cfg.clips_dir);
            let entries = hotkeys::assign_keys(&clips.list());
            if entries.is_empty() {
                show_toast(&toast_overlay, NO_CLIPS_LINE);
                return;
            }
            let Some(dir) = applications_dir() else {
                show_toast(&toast_overlay, "Не удалось определить каталог приложений");
                return;
            };
            let files = hotkeys::build_files(&entries, &hotkeys::exec_binary());
            // Write, then make the RUNNING Plasma re-read the files: the
            // daemon never reloads an existing component on its own (see
            // `kglobalaccel` module docs — the stale-Exec trap), so a plain
            // kbuildsycoca6 is not enough after a rewrite.
            let handle = gio::spawn_blocking(move || {
                let outcome = hotkeys::write_desktop_files(&dir, &files);
                let activation = outcome.is_ok().then(|| {
                    let names: Vec<String> =
                        files.iter().map(|file| file.file_name.clone()).collect();
                    match kglobalaccel::refresh_services(&dir, &names) {
                        Ok(res @ kglobalaccel::RefreshOutcome::Refreshed) => Ok(res),
                        // No Plasma daemon: refresh the cache only, so a
                        // later session picks the files up.
                        Ok(kglobalaccel::RefreshOutcome::NoDaemon) => hotkeys::refresh_ksycoca()
                            .map(|()| kglobalaccel::RefreshOutcome::NoDaemon),
                        Err(e) => {
                            // Daemon misbehaved: still refresh the cache
                            // best-effort so a re-login applies the files.
                            let _ = hotkeys::refresh_ksycoca();
                            Err(e)
                        }
                    }
                });
                (outcome, activation)
            });
            let section = section.clone();
            let toast_overlay = toast_overlay.clone();
            let cfg = cfg.clone();
            glib::spawn_future_local(async move {
                let Ok((outcome, activation)) = handle.await else {
                    show_toast(
                        &toast_overlay,
                        "Ошибка: операция хоткеев аварийно завершилась",
                    );
                    return;
                };
                match outcome {
                    Ok(o) => {
                        let mut text = format!(
                            "Хоткеи: создано {}, обновлено {}, без изменений {}",
                            o.created.len(),
                            o.updated.len(),
                            o.unchanged.len()
                        );
                        if !o.skipped.is_empty() {
                            text.push_str(&format!(", пропущено {}", o.skipped.len()));
                        }
                        show_toast(&toast_overlay, &text);
                    }
                    Err(e) => {
                        show_toast(&toast_overlay, &format!("Ошибка хоткеев: {e:#}"));
                    }
                }
                if let Some(Err(e)) = activation {
                    show_toast(
                        &toast_overlay,
                        &format!(
                            "Хоткеи записаны, но Plasma их не перечитала: {e:#} — \
                             перелогиньтесь, чтобы они заработали"
                        ),
                    );
                }
                section.refresh(&cfg);
            });
        });
    }

    // «Выключить»: remove ONLY our marker-checked desktop files, then
    // kbuildsycoca6; also off the UI thread.
    {
        let section = hotkeys_section.clone();
        let cfg = cfg.clone();
        let toast_overlay = toast_overlay.clone();
        hotkeys_section.disable_btn.connect_clicked(move |_btn| {
            let Some(dir) = applications_dir() else {
                show_toast(&toast_overlay, "Не удалось определить каталог приложений");
                return;
            };
            let handle = gio::spawn_blocking(move || {
                let removed = hotkeys::remove_desktop_files(&dir);
                let ksycoca = removed.is_ok().then(hotkeys::refresh_ksycoca);
                (removed, ksycoca)
            });
            let section = section.clone();
            let cfg = cfg.clone();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                let Ok((removed, ksycoca)) = handle.await else {
                    show_toast(
                        &toast_overlay,
                        "Ошибка: операция хоткеев аварийно завершилась",
                    );
                    return;
                };
                match removed {
                    Ok(files) if files.is_empty() => {
                        show_toast(&toast_overlay, "Наших hotkey-файлов нет");
                    }
                    Ok(files) => {
                        show_toast(
                            &toast_overlay,
                            &format!("Выключено: удалено файлов {}", files.len()),
                        );
                    }
                    Err(e) => {
                        show_toast(&toast_overlay, &format!("Ошибка хоткеев: {e:#}"));
                    }
                }
                if let Some(Err(e)) = ksycoca {
                    show_toast(
                        &toast_overlay,
                        &format!(
                            "Не удалось выполнить kbuildsycoca6 ({e:#}) — \
                             запустите вручную или перелогиньтесь"
                        ),
                    );
                }
                section.refresh(&lock_config(&cfg));
            });
        });
    }

    // Volume slider: mutate the shared config in memory on every tick; the
    // worker uses the current value for new plays and persists it with a
    // ~300 ms trailing debounce (hotkey-spawned `soundboard play` processes
    // read the config volume, so the slider must reach config.toml).
    {
        let cfg = cfg.clone();
        let worker = worker.clone();
        volume_scale.adjustment().connect_value_changed(move |adj| {
            lock_config(&cfg).volume = slider_to_volume_factor(adj.value());
            worker.send(WorkerCmd::SaveVolume);
        });
    }

    // mic_mix/monitoring change loopbacks: persist, then reconcile the
    // graph. interrupt_mode is playback policy: persist only.
    {
        let cfg = cfg.clone();
        let worker = worker.clone();
        sw_mic_mix.connect_active_notify(move |row| {
            lock_config(&cfg).mic_mix = row.is_active();
            worker.send(WorkerCmd::SaveConfig);
            worker.send(WorkerCmd::EnsureGraph);
        });
    }
    {
        let cfg = cfg.clone();
        let worker = worker.clone();
        sw_monitoring.connect_active_notify(move |row| {
            lock_config(&cfg).monitoring = row.is_active();
            worker.send(WorkerCmd::SaveConfig);
            worker.send(WorkerCmd::EnsureGraph);
        });
    }
    {
        let cfg = cfg.clone();
        let worker = worker.clone();
        sw_interrupt.connect_active_notify(move |row| {
            lock_config(&cfg).interrupt_mode = row.is_active();
            worker.send(WorkerCmd::SaveConfig);
        });
    }

    // Doctor/reconcile/meter/error messages from the worker. The receiver
    // loops run on the main loop (gtk4-rs book pattern with async-channel).
    // Both Status and GraphUp refresh the clip grid: new files become
    // buttons without a manual rescan, and the very first doctor report
    // (which arrives even when the startup reconcile failed) builds it.
    let (summary_rx, checks_rx, raw_rx, window_rx) = (
        summary.clone(),
        checks.clone(),
        raw_warnings.clone(),
        window.clone(),
    );
    let grid_rx = grid.clone();
    let banner_rx = banner.clone();
    let hotkeys_rx = hotkeys_section.clone();
    let worker_rx = worker.clone();
    let cfg_rx = cfg.clone();
    let toast_rx = toast_overlay.clone();
    let tray_rx_handle = tray.clone();
    glib::spawn_future_local(async move {
        while let Ok(msg) = ui_rx.recv().await {
            match msg {
                GuiMsg::Status(report) => {
                    // A fresh report replaces the summary unconditionally, so
                    // an earlier error line (e.g. a failed meter start) is
                    // cleared as soon as a later status is all-pass.
                    set_summary(
                        &summary_rx,
                        doctor_summary_line(&report),
                        Some(report.all_passed()),
                    );
                    rebuild_check_rows(&checks_rx, &report);
                    banner_rx.set_revealed(!report.all_passed());
                    grid_rx.reload(&lock_config(&cfg_rx), &worker_rx);
                    hotkeys_rx.refresh(&lock_config(&cfg_rx));
                    // Mirror the graph status into the tray menu line.
                    if let Some(handle) = tray_rx_handle.borrow().as_ref() {
                        let ok = report.all_passed();
                        let _ = handle.update(move |state| state.graph_ok = ok);
                    }
                }
                GuiMsg::GraphUp(report) => {
                    set_summary(
                        &summary_rx,
                        reconcile_summary_line(&report),
                        Some(report.warnings.is_empty()),
                    );
                    raw_rx.set_visible(!report.warnings.is_empty());
                    if !report.warnings.is_empty() {
                        raw_rx.set_text(&report.warnings.join("\n"));
                    }
                    banner_rx.set_revealed(!report.warnings.is_empty());
                    grid_rx.reload(&lock_config(&cfg_rx), &worker_rx);
                    hotkeys_rx.refresh(&lock_config(&cfg_rx));
                }
                GuiMsg::PlayingSet(names) => {
                    grid_rx.set_playing(&names);
                }
                GuiMsg::Error(text) => {
                    set_summary(&summary_rx, &format!("Ошибка: {text}"), Some(false));
                    show_toast(&toast_rx, &format!("Ошибка: {text}"));
                }
                GuiMsg::ShutdownComplete => {
                    // Remove the tray icon before the window goes away (the
                    // quit path may already have shut the service down).
                    if let Some(handle) = tray_rx_handle.borrow().as_ref()
                        && !handle.is_closed()
                    {
                        let _ = handle.shutdown();
                    }
                    window_rx.destroy();
                }
            }
        }
    });

    // Meter values arrive on their own channel (~30 Hz, throttled).
    glib::spawn_future_local(async move {
        while let Ok(value) = meter_rx.recv().await {
            level_bar.set_value(meter_level(value));
        }
    });

    // Window close: with a tray present, [X] only hides the window (the
    // app keeps running: graph, meter, hotkey-spawned plays); a real quit
    // is the tray «Выход» (or a signal, or closing without any tray).
    // Quitting keeps the window until the worker confirms cleanup (config
    // saved, meter stopped, playbacks stopped) — the graph stays up for the
    // conferencing app.
    let quit_requested = Rc::new(std::cell::Cell::new(false));
    {
        let worker = worker.clone();
        let quit = quit_requested.clone();
        let toast_overlay = toast_overlay.clone();
        let window_close = window.clone();
        let has_tray = tray.borrow().is_some();
        window.connect_close_request(move |_window| {
            if quit.get() || !has_tray {
                worker.send(WorkerCmd::Shutdown);
            } else {
                // The toast surfaces on the next reveal (within its 6 s
                // timeout) — toasts of a hidden window are invisible, but
                // the call must stay non-blocking and cheap.
                show_toast(&toast_overlay, "Приложение осталось в трее");
                window_close.set_visible(false);
            }
            glib::Propagation::Stop
        });
    }

    // Tray menu commands: never touch GTK from the ksni thread — this drain
    // loop runs on the main loop.
    {
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let quit = quit_requested.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = tray_rx.recv().await {
                match cmd {
                    tray::TrayCmd::ToggleWindow => {
                        if window.is_visible() {
                            show_toast(&toast_overlay, "Приложение осталось в трее");
                            window.set_visible(false);
                        } else {
                            window.present();
                        }
                    }
                    tray::TrayCmd::Quit => {
                        // Same graceful path as SIGINT/SIGTERM: close-request
                        // sends WorkerCmd::Shutdown, the worker confirms via
                        // ShutdownComplete and the window is destroyed; the
                        // tray icon goes first so it never outlives the app.
                        quit.set(true);
                        window.close();
                    }
                }
            }
        });
    }

    install_signal_close_poll(&window, quit_requested);

    // Build the grid right away instead of waiting for the first worker
    // report (scan is a cheap readdir); the hotkey section state comes from
    // the same cheap directory reads.
    grid.reload(&current, worker);
    hotkeys_section.refresh(&current);

    window.present();
    window
}

/// Signal shutdown flag: set by the libc handlers, polled on the main loop.
/// glib 0.22 has no unix-signal sources (`unix_signal_add_local` was
/// removed), so a handler may only touch an async-signal-safe atomic; a
/// small main-loop timeout routes the flag into the normal close-request
/// path. A repeated signal before the loop polls it is harmless; after the
/// poll source stops, the process is already exiting.
static SIGNAL_CLOSE_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// libc signal handler: async-signal-safe (a single atomic store).
extern "C" fn on_close_signal(_signum: libc::c_int) {
    SIGNAL_CLOSE_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Registers SIGINT/SIGTERM handlers and a main-loop poll that routes them
/// through the same shutdown path as the tray «Выход» (`window.close()` →
/// close-request → `WorkerCmd::Shutdown` → worker stops meter + playbacks →
/// `ShutdownComplete` → window destroyed, exit 0; the graph stays up).
/// Raw libc FFI follows the recorded unsafe ADR (same precedent as
/// `player.rs`'s `kill`).
fn install_signal_close_poll(window: &adw::ApplicationWindow, quit: Rc<std::cell::Cell<bool>>) {
    for signum in [libc::SIGINT, libc::SIGTERM] {
        // Safety: `libc::signal` only registers the handler; the handler
        // itself performs a single async-signal-safe atomic store.
        unsafe {
            libc::signal(signum, on_close_signal as *const () as libc::sighandler_t);
        }
    }
    let window = window.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
        if SIGNAL_CLOSE_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
            quit.set(true);
            window.close();
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Replaces the summary text and (optionally) its ok/error tint.
fn set_summary(label: &Label, text: &str, ok: Option<bool>) {
    label.set_text(text);
    label.remove_css_class("success");
    label.remove_css_class("error");
    if let Some(passed) = ok {
        let class = check_css_class(passed);
        label.add_css_class(class);
    }
}

/// Rebuilds the doctor check rows of the «Дебаг» page: one row per check —
/// the ✓/✗ icon, the check name and its raw detail line (module args, node
/// ids, alsa names) inline. This page IS the details view: nothing is
/// hidden behind a tab switch anymore.
fn rebuild_check_rows(checks: &GtkBox, report: &DoctorReport) {
    while let Some(child) = checks.first_child() {
        checks.remove(&child);
    }
    for check in &report.checks {
        let icon = Image::from_icon_name(check_icon_name(check.passed));
        icon.set_pixel_size(16);
        icon.add_css_class(check_css_class(check.passed));
        icon.set_valign(gtk4::Align::Start);
        let name = Label::builder()
            .label(&check.name)
            .halign(gtk4::Align::Start)
            .build();
        let detail = Label::builder()
            .label(&check.detail)
            .halign(gtk4::Align::Start)
            .wrap(true)
            .xalign(0.0)
            .sensitive(false)
            .build();
        detail.add_css_class("dim-label");
        let texts = GtkBox::new(Orientation::Vertical, 0);
        texts.append(&name);
        texts.append(&detail);
        let row = GtkBox::new(Orientation::Horizontal, 6);
        row.set_margin_start(8);
        row.set_margin_end(8);
        row.set_margin_top(4);
        row.set_margin_bottom(4);
        row.append(&icon);
        row.append(&texts);
        checks.append(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Check;

    fn report_with(passed: bool) -> DoctorReport {
        DoctorReport {
            checks: vec![Check {
                name: "check".to_string(),
                passed,
                detail: String::new(),
            }],
        }
    }

    #[test]
    fn doctor_summary_reflects_checks() {
        assert_eq!(doctor_summary_line(&report_with(true)), "Граф: OK");
        assert_eq!(doctor_summary_line(&report_with(false)), "Граф: проблемы");
    }

    #[test]
    fn reconcile_summary_flags_warnings_only() {
        let mut report = Report::default();
        assert_eq!(reconcile_summary_line(&report), "Граф: OK");
        report.warnings.push("warning".to_string());
        assert_eq!(reconcile_summary_line(&report), "Граф: проблемы");
    }

    #[test]
    fn check_icons_and_classes_pair_up() {
        assert_eq!(check_icon_name(true), "emblem-ok-symbolic");
        assert_eq!(check_css_class(true), "success");
        assert_eq!(check_icon_name(false), "dialog-error-symbolic");
        assert_eq!(check_css_class(false), "error");
    }

    #[test]
    fn meter_level_is_clamped_into_bar_range() {
        assert_eq!(meter_level(0.0), 0.0);
        assert_eq!(meter_level(0.5), 0.5);
        assert_eq!(meter_level(-0.2), 0.0);
        assert_eq!(meter_level(1.7), 1.0);
    }

    #[test]
    fn volume_slider_mapping_round_trips() {
        assert_eq!(slider_to_volume_factor(0.0), 0.0);
        assert_eq!(slider_to_volume_factor(50.0), 0.5);
        assert_eq!(slider_to_volume_factor(100.0), 1.0);
        assert_eq!(volume_factor_to_slider(0.0), 0.0);
        // 0.9 is not exact in f32, so compare against the widened f32 value.
        assert_eq!(volume_factor_to_slider(0.9), f64::from(0.9_f32) * 100.0);
        assert_eq!(volume_factor_to_slider(1.0), 100.0);
        // Round-trip through the slider keeps the documented default exact.
        let factor = slider_to_volume_factor(volume_factor_to_slider(0.9));
        assert!((factor - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn volume_slider_mapping_is_total_against_odd_input() {
        // Out-of-range factors (hand-edited config) clamp into the scale.
        assert_eq!(volume_factor_to_slider(2.5), 100.0);
        assert_eq!(volume_factor_to_slider(-1.0), 0.0);
        // Non-finite slider input maps to 0 (clamp against NaN would be
        // undefined, so the implementation must not rely on it).
        assert_eq!(slider_to_volume_factor(f64::NEG_INFINITY), 0.0);
    }

    #[test]
    fn clips_placeholder_names_the_directory() {
        let text = clips_placeholder_text(Path::new("/home/u/Music/clips"));
        assert!(text.contains("/home/u/Music/clips"), "{text}");
        assert!(text.contains("mp3/ogg/wav/flac"), "{text}");
    }

    #[test]
    fn search_filter_matches_name_or_category_case_insensitively() {
        // Empty (or blank) query matches everything.
        assert!(clip_matches_query("airhorn", "Без категории", ""));
        assert!(clip_matches_query("airhorn", "Без категории", "   "));
        // Substring on the clip name, case-insensitive.
        assert!(clip_matches_query("airhorn", "Мемы", "AIR"));
        assert!(clip_matches_query("Не Молчите", "Кринж", "молч"));
        // Substring on the category name.
        assert!(clip_matches_query("airhorn", "Мемы", "мем"));
        // Misses stay hidden.
        assert!(!clip_matches_query("airhorn", "Мемы", "bruh"));
        assert!(!clip_matches_query("airhorn", "Мемы", "xyz"));
    }

    #[test]
    fn clip_playing_membership_is_exact() {
        let playing = vec!["airhorn".to_string(), "applause".to_string()];
        assert!(clip_is_playing(&playing, "airhorn"));
        assert!(!clip_is_playing(&playing, "air"));
        assert!(!clip_is_playing(&playing, ""));
        assert!(!clip_is_playing(&[], "airhorn"));
    }

    #[test]
    fn hotkey_state_lines_cover_all_states() {
        assert_eq!(
            hotkey_state_line(Registration::Current),
            "Хоткеи зарегистрированы"
        );
        assert_eq!(
            hotkey_state_line(Registration::None),
            "Хоткеи не зарегистрированы"
        );
        assert!(hotkey_state_line(Registration::Stale).contains("устарели"));
    }

    #[test]
    fn enable_button_labels_and_sensitivity_follow_state() {
        assert_eq!(enable_button_label(Registration::None), "Включить хоткеи");
        assert_eq!(enable_button_label(Registration::Stale), "Обновить хоткеи");
        assert_eq!(
            enable_button_label(Registration::Current),
            "Обновить хоткеи"
        );
    }
}
