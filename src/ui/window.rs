//! Main application window: status rows for the doctor checks, graph
//! repair/test buttons, the virtual mic VU meter and a placeholder for the
//! upcoming clip grid (next slice).
//!
//! Threading rule (see [`super::worker`]): nothing blocking happens here —
//! the window only sends [`WorkerCmd`]s to the audio worker and renders
//! [`GuiMsg`]s arriving over glib channels.

use async_channel::Receiver;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, Frame, Image, Label, LevelBar, Orientation};
use libadwaita as adw;
use libadwaita::prelude::*;

use super::worker::{GuiMsg, Worker, WorkerCmd};
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

/// Builds the window, wires the worker channels and presents it. The
/// returned window is owned by the application once presented; the caller
/// may drop the handle.
pub(crate) fn build(
    app: &adw::Application,
    ui_rx: Receiver<GuiMsg>,
    meter_rx: Receiver<f32>,
    worker: &Worker,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Soundboard")
        .default_width(720)
        .default_height(560)
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
    buttons.append(&btn_doctor);
    buttons.append(&btn_repair);
    buttons.append(&btn_test);

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

    // Placeholder for the next slice (clip grid).
    let clips_placeholder = Label::builder()
        .label("Клипы (следующий этап)")
        .halign(gtk4::Align::Start)
        .sensitive(false)
        .build();

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
    content.append(&clips_placeholder);
    toolbar.set_content(Some(&content));
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

    // Doctor/reconcile/meter/error messages from the worker. The receiver
    // loops run on the main loop (gtk4-rs book pattern with async-channel).
    let (summary_rx, checks_rx, window_rx) = (summary.clone(), checks.clone(), window.clone());
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
                }
                GuiMsg::GraphUp(report) => {
                    set_summary(
                        &summary_rx,
                        reconcile_summary_line(&report),
                        Some(report.warnings.is_empty()),
                    );
                }
                GuiMsg::Error(text) => {
                    set_summary(&summary_rx, &format!("Ошибка: {text}"), Some(false));
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
    // (meter stopped, playbacks stopped); the graph stays up for Zoom.
    {
        let worker = worker.clone();
        window.connect_close_request(move |_window| {
            worker.send(WorkerCmd::Shutdown);
            glib::Propagation::Stop
        });
    }

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
}
