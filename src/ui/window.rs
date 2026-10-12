//! Main application window: status rows for the doctor checks, graph
//! repair/test buttons, the virtual mic VU meter, the clip grid and the
//! playback controls (volume, stop-all, behavior toggles).
//!
//! Threading rule (see [`super::worker`]): nothing blocking happens here —
//! the window only sends [`WorkerCmd`]s to the audio worker and renders
//! [`GuiMsg`]s arriving over glib channels. The clip grid scans
//! `clips_dir` on the UI thread deliberately: `Clips::scan` is a single
//! small readdir, no audio APIs involved.
//!
//! Config flow: toggles and the volume slider mutate the shared config
//! under the lock (in-memory, instant for the worker), then either send
//! `SaveConfig` + `EnsureGraph` (mic_mix/monitoring change loopbacks, so
//! the graph must reconcile) or `SaveConfig` only (interrupt_mode). The
//! slider saves nothing per tick — persistence happens with the next
//! explicit save (any toggle or shutdown).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use async_channel::Receiver;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, FlowBox, Frame, Image, Label, LevelBar, Orientation, Scale};
use libadwaita as adw;
use libadwaita::prelude::*;

use super::worker::{GuiMsg, SharedConfig, Worker, WorkerCmd, lock_config};
use crate::clips::Clips;
use crate::config::Config;
use crate::graph::{DoctorReport, Report, ZOOM_HINT};

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

/// True when `name` is in the playing set (drives the button highlight).
pub(crate) fn clip_is_playing(playing: &[String], name: &str) -> bool {
    playing.iter().any(|n| n == name)
}

/// The clip grid section: a `FlowBox` of clip buttons plus the empty-dir
/// placeholder and scan warnings. `buttons` is shared with the worker
/// message loop so [`GuiMsg::PlayingSet`] can toggle the highlight.
struct ClipGrid {
    flow: FlowBox,
    placeholder: Label,
    warnings_label: Label,
    buttons: Rc<RefCell<HashMap<String, Button>>>,
}

impl ClipGrid {
    fn new() -> Self {
        let flow = FlowBox::new();
        flow.set_selection_mode(gtk4::SelectionMode::None);
        flow.set_column_spacing(6);
        flow.set_row_spacing(6);
        flow.set_min_children_per_line(2);
        flow.set_max_children_per_line(6);
        flow.set_homogeneous(true);
        flow.set_hexpand(true);
        let placeholder = Label::builder()
            .label("Клипы: сканирование…")
            .halign(gtk4::Align::Start)
            .wrap(true)
            .sensitive(false)
            .build();
        let warnings_label = Label::builder()
            .halign(gtk4::Align::Start)
            .wrap(true)
            .visible(false)
            .build();
        warnings_label.add_css_class("dim-label");
        ClipGrid {
            flow,
            placeholder,
            warnings_label,
            buttons: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// Widget holding the whole section (grid, placeholder, warnings).
    fn container(&self) -> GtkBox {
        let box_ = GtkBox::new(Orientation::Vertical, 6);
        box_.append(&self.flow);
        box_.append(&self.placeholder);
        box_.append(&self.warnings_label);
        box_
    }

    /// Rebuilds the grid from a fresh directory scan (see the module docs:
    /// cheap, UI-thread-safe).
    fn reload(&self, cfg: &Config, worker: &Worker) {
        let clips = Clips::scan(&cfg.clips_dir);
        while let Some(child) = self.flow.first_child() {
            self.flow.remove(&child);
        }
        self.buttons.borrow_mut().clear();
        for name in clips.list() {
            let Some(path) = clips.get(name) else {
                continue;
            };
            let file_name = path
                .file_name()
                .map_or_else(|| name.to_string(), |n| n.to_string_lossy().into_owned());
            let btn = Button::with_label(name);
            btn.set_tooltip_text(Some(&file_name));
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
            self.flow.append(&btn);
            self.buttons.borrow_mut().insert(name.to_string(), btn);
        }
        let empty = clips.is_empty();
        self.flow.set_visible(!empty);
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
/// may drop the handle.
pub(crate) fn build(
    app: &adw::Application,
    ui_rx: Receiver<GuiMsg>,
    meter_rx: Receiver<f32>,
    worker: &Worker,
    cfg: &SharedConfig,
) -> adw::ApplicationWindow {
    let current = lock_config(cfg).clone();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Soundboard")
        .default_width(720)
        .default_height(640)
        .build();

    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    // Status section: summary line + one row per doctor check.
    let summary = Label::builder()
        .label("Автопроверка при запуске…")
        .halign(gtk4::Align::Start)
        .build();
    summary.add_css_class("heading");
    let checks = GtkBox::new(Orientation::Vertical, 6);
    let checks_frame = Frame::new(None);
    checks_frame.set_child(Some(&checks));
    let placeholder_checks = Label::builder()
        .label("Ожидание результатов проверки…")
        .halign(gtk4::Align::Start)
        .sensitive(false)
        .build();
    checks.append(&placeholder_checks);

    // Action buttons.
    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    let btn_doctor = Button::with_label("Проверить");
    let btn_repair = Button::with_label("Починить граф");
    btn_repair.add_css_class("suggested-action");
    let btn_test = Button::with_label("Тест");
    let btn_stop_all = Button::with_label("Стоп всё");
    btn_stop_all.add_css_class("destructive-action");
    buttons.append(&btn_doctor);
    buttons.append(&btn_repair);
    buttons.append(&btn_test);
    buttons.append(&btn_stop_all);

    // VU meter section.
    let meter_caption = Label::builder()
        .label("Виртуальный микрофон")
        .halign(gtk4::Align::Start)
        .build();
    meter_caption.add_css_class("heading");
    let level_bar = LevelBar::builder()
        .min_value(0.0)
        .max_value(1.0)
        .mode(gtk4::LevelBarMode::Continuous)
        .height_request(12)
        .hexpand(true)
        .value(0.0)
        .build();
    let zoom_hint = Label::builder()
        .label(ZOOM_HINT)
        .halign(gtk4::Align::Start)
        .wrap(true)
        .build();
    zoom_hint.add_css_class("dim-label");

    // Clips section: caption + refresh button, then the grid.
    let grid = Rc::new(ClipGrid::new());
    let clips_caption = Label::builder()
        .label("Клипы")
        .halign(gtk4::Align::Start)
        .valign(gtk4::Align::Center)
        .build();
    clips_caption.add_css_class("heading");
    let btn_refresh = Button::with_label("Обновить");
    btn_refresh.add_css_class("flat");
    let clips_header = GtkBox::new(Orientation::Horizontal, 8);
    clips_header.append(&clips_caption);
    clips_header.append(&btn_refresh);

    // Controls: volume slider.
    let volume_caption = Label::builder()
        .label("Громкость")
        .halign(gtk4::Align::Start)
        .build();
    volume_caption.add_css_class("heading");
    let volume_scale = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
    volume_scale.set_value(volume_factor_to_slider(current.volume));
    volume_scale.set_digits(0);
    volume_scale.set_hexpand(true);

    // Controls: behavior toggles.
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

    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&summary);
    content.append(&checks_frame);
    content.append(&buttons);
    content.append(&meter_caption);
    content.append(&level_bar);
    content.append(&zoom_hint);
    content.append(&clips_header);
    content.append(&grid.container());
    content.append(&volume_caption);
    content.append(&volume_scale);
    content.append(&toggles);

    // Toasts surface worker errors without blocking the UI.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&content));
    toolbar.set_content(Some(&toast_overlay));
    window.set_content(Some(&toolbar));

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
        let cfg = cfg.clone();
        let worker = worker.clone();
        btn_refresh.connect_clicked(move |_btn| {
            grid.reload(&lock_config(&cfg), &worker);
        });
    }

    // Volume slider: mutate the shared config in memory on every tick; the
    // worker uses the current value for new plays, persistence happens with
    // the next explicit save (a toggle or shutdown).
    {
        let cfg = cfg.clone();
        volume_scale.adjustment().connect_value_changed(move |adj| {
            lock_config(&cfg).volume = slider_to_volume_factor(adj.value());
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
    let (summary_rx, checks_rx, window_rx) = (summary.clone(), checks.clone(), window.clone());
    let grid_rx = grid.clone();
    let worker_rx = worker.clone();
    let cfg_rx = cfg.clone();
    let toast_rx = toast_overlay.clone();
    glib::spawn_future_local(async move {
        while let Ok(msg) = ui_rx.recv().await {
            match msg {
                GuiMsg::Status(report) => {
                    set_summary(
                        &summary_rx,
                        doctor_summary_line(&report),
                        Some(report.all_passed()),
                    );
                    rebuild_check_rows(&checks_rx, &report);
                    grid_rx.reload(&lock_config(&cfg_rx), &worker_rx);
                }
                GuiMsg::GraphUp(report) => {
                    set_summary(
                        &summary_rx,
                        reconcile_summary_line(&report),
                        Some(report.warnings.is_empty()),
                    );
                    grid_rx.reload(&lock_config(&cfg_rx), &worker_rx);
                }
                GuiMsg::PlayingSet(names) => {
                    grid_rx.set_playing(&names);
                }
                GuiMsg::Error(text) => {
                    set_summary(&summary_rx, &format!("Ошибка: {text}"), Some(false));
                    let toast = adw::Toast::new(&format!("Ошибка: {text}"));
                    toast.set_timeout(6);
                    toast_rx.add_toast(toast);
                }
                GuiMsg::ShutdownComplete => window_rx.destroy(),
            }
        }
    });

    // Meter values arrive on their own channel (~30 Hz, throttled).
    glib::spawn_future_local(async move {
        while let Ok(value) = meter_rx.recv().await {
            level_bar.set_value(meter_level(value));
        }
    });

    // Graceful shutdown: keep the window until the worker confirms cleanup
    // (config saved, meter stopped, playbacks stopped); the graph stays up
    // for Zoom.
    {
        let worker = worker.clone();
        window.connect_close_request(move |_window| {
            worker.send(WorkerCmd::Shutdown);
            glib::Propagation::Stop
        });
    }

    // Build the grid right away instead of waiting for the first worker
    // report (scan is a cheap readdir).
    grid.reload(&current, worker);

    window.present();
    window
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

/// Rebuilds the per-check status rows: icon (green/red) + name + detail.
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
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        let detail = Label::builder()
            .label(&check.detail)
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
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
    fn clip_playing_membership_is_exact() {
        let playing = vec!["airhorn".to_string(), "applause".to_string()];
        assert!(clip_is_playing(&playing, "airhorn"));
        assert!(!clip_is_playing(&playing, "air"));
        assert!(!clip_is_playing(&playing, ""));
        assert!(!clip_is_playing(&[], "airhorn"));
    }
}
