//! The main window. Created on demand from the tray and destroyed on close, so no GL context or
//! window exists while AudioBridge sits in the tray. Repaints on status changes; the only
//! animation (the level bars) runs while PC audio is streaming and the window is open.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use audiobridge_core::session::{ConnState, PathKind, Status};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Layout,
    Margin, Mesh, Painter, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui,
    UiBuilder,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::core::{w, BOOL, HSTRING};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::audio::DeviceSummary;
use crate::cable_install::{self, InstallState};
use crate::icon;
use crate::settings::Settings;
use crate::shared::shared;

const BG: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x14);
const CARD: Color32 = Color32::from_rgb(0x13, 0x16, 0x21);
const CARD_HOVER: Color32 = Color32::from_rgb(0x1A, 0x1E, 0x2C);
const CARD_STROKE: Color32 = Color32::from_rgb(0x23, 0x28, 0x38);
const TEXT: Color32 = Color32::from_rgb(0xEE, 0xF0, 0xF6);
const MUTED: Color32 = Color32::from_rgb(0x8A, 0x91, 0xA8);
/// Brand gradient (matches the Android launcher icon).
const INDIGO: Color32 = Color32::from_rgb(0x5B, 0x4B, 0xFF);
const TEAL: Color32 = Color32::from_rgb(0x19, 0xC3, 0xD0);
const GREEN: Color32 = Color32::from_rgb(0x4A, 0xDE, 0x80);
const AMBER: Color32 = Color32::from_rgb(0xF5, 0xB5, 0x44);
const RED: Color32 = Color32::from_rgb(0xF8, 0x71, 0x71);
const LINK: Color32 = Color32::from_rgb(0x7D, 0xD8, 0xE2);
const TOGGLE_OFF: Color32 = Color32::from_rgb(0x2E, 0x33, 0x45);
const WARN_BG: Color32 = Color32::from_rgb(0x24, 0x1D, 0x10);
const WARN_STROKE: Color32 = Color32::from_rgb(0x5A, 0x45, 0x15);

const VB_CABLE_URL: &str = "https://vb-audio.com/Cable/";
const REPO_URL: &str = "https://github.com/Lyten02/AudioBridge";

const PAD: f32 = 16.0;
const GAP: f32 = 12.0;
const TILE_H: f32 = 148.0;

/// Opens the window and blocks until it is closed (the app keeps running in the tray).
pub fn run_window() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AudioBridge")
            .with_inner_size([400.0, 680.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_icon(Arc::new(egui::IconData { rgba: icon::render(64, false), width: 64, height: 64 })),
        renderer: eframe::Renderer::Glow,
        centered: true,
        run_and_return: true,
        ..Default::default()
    };
    let result = eframe::run_native(
        "AudioBridge",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
                _ => 0,
            };
            dark_title_bar(hwnd);
            shared().register_window(hwnd, cc.egui_ctx.clone());
            Ok(Box::new(UiApp::default()))
        }),
    );
    shared().unregister_window();
    if let Err(e) = result {
        tracing::error!("window failed: {e}");
    }
}

fn setup_style(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let dir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("Fonts");
    let mut proportional = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Ok(bytes) = std::fs::read(dir.join("segoeui.ttf")) {
        fonts.font_data.insert("segoe".into(), Arc::new(FontData::from_owned(bytes)));
        proportional.insert(0, "segoe".into());
    }
    let mut semibold = proportional.clone();
    if let Ok(bytes) = std::fs::read(dir.join("seguisb.ttf")) {
        fonts.font_data.insert("segoe-sb".into(), Arc::new(FontData::from_owned(bytes)));
        semibold.insert(0, "segoe-sb".into());
    }
    fonts.families.insert(FontFamily::Proportional, proportional);
    fonts.families.insert(FontFamily::Name("semibold".into()), semibold);
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = LINK;
    v.widgets.inactive.weak_bg_fill = TOGGLE_OFF;
    v.widgets.hovered.weak_bg_fill = CARD_STROKE;
    v.widgets.inactive.corner_radius = CornerRadius::same(10);
    v.widgets.hovered.corner_radius = CornerRadius::same(10);
    v.widgets.active.corner_radius = CornerRadius::same(10);
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(14.0, 7.0);
    });
}

/// Dark caption matching the window background (Windows 11; ignored where unsupported).
fn dark_title_bar(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let hwnd = HWND(hwnd as *mut _);
    let dark: BOOL = true.into();
    // COLORREF is 0x00BBGGRR.
    let caption = u32::from(BG.r()) | (u32::from(BG.g()) << 8) | (u32::from(BG.b()) << 16);
    // SAFETY: the pointers reference locals of the declared sizes for the duration of the calls.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const dark).cast(),
            size_of::<BOOL>() as u32,
        );
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, (&raw const caption).cast(), size_of::<u32>() as u32);
    }
}

fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

fn path_label(p: PathKind) -> &'static str {
    match p {
        PathKind::Lan => "Локальная сеть",
        PathKind::Tailscale => "Tailscale",
        PathKind::Direct => "Напрямую через интернет",
        PathKind::Relay => "Через relay",
    }
}

fn status_line(s: &Status) -> (String, Color32) {
    match s.state {
        ConnState::Connected => {
            let mut t = String::from("Подключено");
            if let Some(rtt) = s.rtt_ms {
                t.push_str(&format!(" · {rtt:.0} мс"));
            }
            (t, GREEN)
        }
        ConnState::WaitingForPeer => ("Ожидание телефона".into(), AMBER),
        ConnState::Starting => ("Запуск…".into(), MUTED),
        ConnState::Connecting => ("Подключение…".into(), AMBER),
        ConnState::Reconnecting => ("Переподключение…".into(), AMBER),
        ConnState::Stopped => ("Остановлено".into(), RED),
    }
}

fn open_url(url: &str) {
    // SAFETY: ShellExecuteW with valid NUL-terminated strings.
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL);
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing primitives

/// Outline of a rounded rectangle (clockwise, 9 points per corner).
fn rounded_points(rect: Rect, r: f32) -> Vec<Pos2> {
    let r = r.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let corners = [
        (pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (pos2(rect.left() + r, rect.top() + r), 180.0),
        (pos2(rect.right() - r, rect.top() + r), 270.0),
    ];
    let mut pts = Vec::with_capacity(36);
    for (c, start) in corners {
        for i in 0..=8 {
            let a = (start + 90.0 * i as f32 / 8.0_f32).to_radians();
            pts.push(pos2(c.x + r * a.cos(), c.y + r * a.sin()));
        }
    }
    pts
}

/// Rounded rectangle filled with the brand gradient (mostly left → right), with a soft edge.
fn gradient_rect(painter: &Painter, rect: Rect, r: f32, from: Color32, to: Color32) {
    let pts = rounded_points(rect, r);
    let color = |p: Pos2| {
        let t = (p.x - rect.left()) / rect.width() * 0.8 + (p.y - rect.top()) / rect.height() * 0.2;
        from.lerp_to_gamma(to, t.clamp(0.0, 1.0))
    };
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.center(), color(rect.center()));
    for p in &pts {
        mesh.colored_vertex(*p, color(*p));
    }
    let n = pts.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    painter.add(Shape::mesh(mesh));
    painter.add(Shape::closed_line(pts, Stroke::new(1.0, Color32::from_white_alpha(30))));
}

fn arc(center: Pos2, radius: f32, from_deg: f32, to_deg: f32) -> Vec<Pos2> {
    (0..=24)
        .map(|i| {
            let a = (from_deg + (to_deg - from_deg) * i as f32 / 24.0).to_radians();
            pos2(center.x + radius * a.cos(), center.y + radius * a.sin())
        })
        .collect()
}

fn icon_headphones(p: &Painter, rect: Rect, color: Color32) {
    let s = rect.width();
    let c = rect.center();
    let r = s * 0.32;
    p.line(arc(pos2(c.x, c.y + s * 0.06), r, 180.0, 360.0), Stroke::new(s * 0.085, color));
    for sx in [-1.0, 1.0] {
        let cup = Rect::from_center_size(pos2(c.x + sx * r, c.y + s * 0.2), vec2(s * 0.17, s * 0.3));
        p.rect_filled(cup, CornerRadius::same((s * 0.06) as u8), color);
    }
}

fn icon_speaker(p: &Painter, rect: Rect, color: Color32) {
    let s = rect.width();
    let c = rect.center();
    let x0 = c.x - s * 0.32;
    let box_ = Rect::from_min_max(pos2(x0, c.y - s * 0.12), pos2(x0 + s * 0.16, c.y + s * 0.12));
    p.rect_filled(box_, CornerRadius::same(1), color);
    p.add(Shape::convex_polygon(
        vec![
            pos2(box_.right() - 0.5, c.y - s * 0.12),
            pos2(c.x + s * 0.04, c.y - s * 0.3),
            pos2(c.x + s * 0.04, c.y + s * 0.3),
            pos2(box_.right() - 0.5, c.y + s * 0.12),
        ],
        color,
        Stroke::NONE,
    ));
    let w = Stroke::new(s * 0.07, color);
    p.line(arc(pos2(c.x + s * 0.04, c.y), s * 0.16, -45.0, 45.0), w);
    p.line(arc(pos2(c.x + s * 0.04, c.y), s * 0.3, -50.0, 50.0), w);
}

fn icon_mic(p: &Painter, rect: Rect, color: Color32) {
    let s = rect.width();
    let c = rect.center();
    let capsule = Rect::from_center_size(pos2(c.x, c.y - s * 0.1), vec2(s * 0.24, s * 0.42));
    p.rect_filled(capsule, CornerRadius::same((s * 0.12) as u8), color);
    let w = Stroke::new(s * 0.07, color);
    p.line(arc(pos2(c.x, c.y - s * 0.04), s * 0.24, 0.0, 180.0), w);
    p.line(vec![pos2(c.x, c.y + s * 0.2), pos2(c.x, c.y + s * 0.34)], w);
    p.line(vec![pos2(c.x - s * 0.13, c.y + s * 0.34), pos2(c.x + s * 0.13, c.y + s * 0.34)], w);
}

fn icon_phone(p: &Painter, rect: Rect, color: Color32) {
    let s = rect.width();
    let body = Rect::from_center_size(rect.center(), vec2(s * 0.4, s * 0.66));
    p.rect_stroke(body, CornerRadius::same((s * 0.08) as u8), Stroke::new(s * 0.07, color), StrokeKind::Middle);
    p.circle_filled(pos2(body.center().x, body.bottom() - s * 0.1), s * 0.035, color);
}

fn icon_qr(p: &Painter, rect: Rect, color: Color32) {
    let s = rect.width() / 3.0;
    for (x, y) in [(0.0, 0.0), (2.0, 0.0), (0.0, 2.0)] {
        let r = Rect::from_min_size(pos2(rect.left() + x * s, rect.top() + y * s), vec2(s * 0.95, s * 0.95));
        p.rect_stroke(r, CornerRadius::same(1), Stroke::new(1.4, color), StrokeKind::Inside);
    }
    p.rect_filled(Rect::from_min_size(pos2(rect.left() + 2.0 * s, rect.top() + 2.0 * s), vec2(s * 0.6, s * 0.6)), 0.0, color);
}

/// Equalizer-like level bars; animated while `live`.
fn level_bars(p: &Painter, rect: Rect, bars: usize, live: bool, time: f64, color: Color32) {
    let w = rect.width() / (bars as f32 * 2.0 - 1.0);
    for i in 0..bars {
        let h = if live {
            let phase = time * (5.0 + i as f64 * 1.3) + i as f64 * 1.7;
            let v = 0.5 + 0.5 * phase.sin() * (time * 2.3 + i as f64).cos();
            rect.height() * (0.25 + 0.75 * v as f32)
        } else {
            w
        };
        let x = rect.left() + i as f32 * 2.0 * w;
        let r = Rect::from_min_max(pos2(x, rect.bottom() - h), pos2(x + w, rect.bottom()));
        p.rect_filled(r, CornerRadius::same((w / 2.0) as u8), color);
    }
}

/// Circle badge with the brand gradient (or grey when disabled) and a white icon.
fn badge(p: &Painter, rect: Rect, enabled: bool, icon: fn(&Painter, Rect, Color32)) {
    if enabled {
        gradient_rect(p, rect, rect.width() / 2.0, INDIGO, TEAL);
    } else {
        p.circle_filled(rect.center(), rect.width() / 2.0, TOGGLE_OFF);
    }
    icon(p, rect.shrink(rect.width() * 0.22), if enabled { Color32::WHITE } else { MUTED });
}

fn card<R>(ui: &mut Ui, fill: Color32, stroke: Color32, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(CornerRadius::same(16))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Switch. Returns true when clicked.
fn toggle(ui: &mut Ui, on: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(44.0, 26.0), Sense::click());
    let t = ui.ctx().animate_bool_responsive(resp.id, on);
    let painter = ui.painter();
    if t > 0.0 {
        gradient_rect(painter, rect, 13.0, TOGGLE_OFF.lerp_to_gamma(INDIGO, t), TOGGLE_OFF.lerp_to_gamma(TEAL, t));
    } else {
        painter.rect_filled(rect, CornerRadius::same(13), TOGGLE_OFF);
    }
    let x = egui::lerp((rect.left() + 13.0)..=(rect.right() - 13.0), t);
    painter.circle_filled(pos2(x, rect.center().y), 10.0, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A row: title (+ optional subtitle) on the left, switch on the right.
fn switch_row(ui: &mut Ui, title: &str, subtitle: Option<&str>, on: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            // leave room for the switch, so long subtitles wrap instead of pushing it out
            ui.set_max_width((ui.available_width() - 56.0).max(0.0));
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).size(14.5).color(TEXT));
            if let Some(sub) = subtitle {
                ui.label(RichText::new(sub).size(12.5).color(MUTED));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            clicked = toggle(ui, on);
        });
    });
    clicked
}

/// Full-width secondary button with an optional leading icon.
fn wide_button(ui: &mut Ui, text: &str, icon: Option<fn(&Painter, Rect, Color32)>) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::click());
    let painter = ui.painter();
    let fill = if resp.hovered() { CARD_HOVER } else { CARD };
    painter.rect(rect, CornerRadius::same(12), fill, Stroke::new(1.0, CARD_STROKE), StrokeKind::Inside);
    let galley = painter.layout_no_wrap(text.to_owned(), FontId::proportional(14.0), TEXT);
    let icon_w = if icon.is_some() { 22.0 } else { 0.0 };
    let x = rect.center().x - (galley.size().x + icon_w) / 2.0;
    if let Some(icon) = icon {
        icon(painter, Rect::from_center_size(pos2(x + 7.0, rect.center().y), vec2(14.0, 14.0)), LINK);
    }
    painter.galley(pos2(x + icon_w, rect.center().y - galley.size().y / 2.0), galley, TEXT);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

// ---------------------------------------------------------------------------------------------
// Window

#[derive(Default)]
struct UiApp {
    qr: Option<(String, TextureHandle)>,
    show_qr: bool,
    volume_edit: VolumeEdit,
}

impl eframe::App for UiApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::from(BG).to_array()
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let sh = shared();
        let status = sh.status();
        let pairing = sh.pairing_uri();
        let settings = sh.settings();
        let devices = sh.devices();
        let connected = status.state == ConnState::Connected;
        if !connected {
            self.show_qr = false;
        }
        let live = connected && status.pc_audio.active;
        if live {
            ui.ctx().request_repaint_after(Duration::from_millis(40));
        }

        ui.painter().rect_filled(ui.max_rect(), CornerRadius::ZERO, BG);
        egui::Frame::new().inner_margin(Margin::same(PAD as i8)).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = GAP;
                hero(ui, &status, live, settings.service_enabled);
                let label = if settings.service_enabled { "Выключить AudioBridge" } else { "Включить AudioBridge" };
                if wide_button(ui, label, None) {
                    sh.set_enabled(!settings.service_enabled);
                }
                if !settings.service_enabled {
                    card(ui, CARD, CARD_STROKE, |ui| {
                        ui.label("Выключено. Телефон отключён; настройки сохранены.");
                        if let Some(error) = &status.last_error {
                            ui.colored_label(RED, error);
                        }
                    });
                } else if !connected || self.show_qr {
                    warnings(ui, &devices, &settings);
                    self.qr_card(ui, pairing.as_deref(), connected, &status);
                } else {
                    warnings(ui, &devices, &settings);
                    connected_cards(ui, &status, &settings, &devices, &mut self.volume_edit);
                    if wide_button(ui, "Показать QR-код", Some(icon_qr)) {
                        self.show_qr = true;
                    }
                }
                card(ui, CARD, CARD_STROKE, |ui| {
                    if switch_row(ui, "Запускать вместе с Windows", Some("Работает в фоне из трея"), settings.autostart) {
                        sh.set_autostart(!settings.autostart);
                    }
                });
                footer(ui);
            });
        });
    }
}

/// Gradient header: logo, name, connection state and live level bars.
fn hero(ui: &mut Ui, status: &Status, live: bool, enabled: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 104.0), Sense::hover());
    let p = ui.painter();
    gradient_rect(p, rect, 20.0, INDIGO, TEAL);

    let logo = Rect::from_min_size(pos2(rect.left() + 18.0, rect.center().y - 28.0), vec2(56.0, 56.0));
    p.rect_filled(logo, CornerRadius::same(16), Color32::from_white_alpha(38));
    icon_headphones(p, logo.shrink(12.0), Color32::WHITE);

    let x = logo.right() + 14.0;
    p.text(pos2(x, rect.center().y - 12.0), Align2::LEFT_CENTER, "AudioBridge", semibold(22.0), Color32::WHITE);
    let (text, color) = if enabled { status_line(status) } else { ("Выключено".into(), MUTED) };
    let dot = pos2(x + 5.0, rect.center().y + 15.0);
    p.circle_filled(dot, 7.0, Color32::from_black_alpha(60));
    p.circle_filled(dot, 4.0, color);
    p.text(
        pos2(x + 16.0, dot.y),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(13.5),
        Color32::from_white_alpha(235),
    );

    let time = ui.input(|i| i.time);
    let bars = Rect::from_min_size(pos2(rect.right() - 58.0, rect.center().y - 18.0), vec2(38.0, 36.0));
    level_bars(p, bars, 5, live, time, Color32::from_white_alpha(if live { 230 } else { 90 }));
}

struct TileSpec<'a> {
    title: &'a str,
    route: &'a str,
    state: &'a str,
    state_color: Color32,
    on: bool,
    active: bool,
    icon: fn(&Painter, Rect, Color32),
}

/// One stream tile (icon, title, route, state, switch). Returns true when the switch was clicked.
fn tile(ui: &mut Ui, rect: Rect, t: &TileSpec) -> bool {
    let p = ui.painter();
    let stroke = if t.active { TEAL.gamma_multiply(0.55) } else { CARD_STROKE };
    p.rect(rect, CornerRadius::same(16), CARD, Stroke::new(1.0, stroke), StrokeKind::Inside);
    let inner = rect.shrink(PAD);
    badge(p, Rect::from_min_size(inner.min, vec2(40.0, 40.0)), t.on, t.icon);
    p.text(pos2(inner.left(), inner.top() + 62.0), Align2::LEFT_CENTER, t.title, semibold(15.5), TEXT);
    p.text(pos2(inner.left(), inner.top() + 82.0), Align2::LEFT_CENTER, t.route, FontId::proportional(12.5), MUTED);
    let sy = inner.bottom() - 6.0;
    // A long state wraps (it may use half of the right padding); it grows upwards, and the dot
    // marks its first row.
    let wrap = inner.width() - 14.0 + PAD / 2.0;
    let galley = p.layout(t.state.to_owned(), FontId::proportional(12.5), t.state_color, wrap);
    let rows = galley.rows.len().max(1);
    let row_h = galley.size().y / rows as f32;
    let top = sy + row_h / 2.0 - galley.size().y + if rows > 1 { 6.0 } else { 0.0 };
    p.circle_filled(pos2(inner.left() + 4.0, top + row_h / 2.0), 3.5, t.state_color);
    p.galley(pos2(inner.left() + 14.0, top), galley, t.state_color);

    let sw = Rect::from_min_size(pos2(inner.right() - 44.0, inner.top() + 7.0), vec2(44.0, 26.0));
    // a child UI that does not move the parent's cursor (the row was allocated by the caller)
    toggle(&mut ui.new_child(UiBuilder::new().max_rect(sw)), t.on)
}

fn connected_cards(
    ui: &mut Ui,
    status: &Status,
    settings: &Settings,
    devices: &DeviceSummary,
    edit: &mut VolumeEdit,
) {
    let sh = shared();

    // the phone
    card(ui, CARD, CARD_STROKE, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
            badge(ui.painter(), rect, true, icon_phone);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                let name = status.peer_name.as_deref().unwrap_or("Телефон");
                ui.label(RichText::new(name).font(semibold(16.5)).color(TEXT));
                let path = status.path.map_or("Подключено", path_label);
                ui.label(RichText::new(path).size(12.5).color(MUTED));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let galley = ui.painter().layout_no_wrap("В сети".into(), FontId::proportional(12.5), GREEN);
                let (pill, _) = ui.allocate_exact_size(vec2(galley.size().x + 30.0, 26.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(pill, CornerRadius::same(13), GREEN.gamma_multiply(0.12));
                p.circle_filled(pos2(pill.left() + 12.0, pill.center().y), 3.5, GREEN);
                p.galley(pos2(pill.left() + 21.0, pill.center().y - galley.size().y / 2.0), galley, GREEN);
            });
        });
    });

    // the two streams
    let (pc_state, pc_color) = if devices.default_is_cable {
        ("Пауза: вывод в CABLE", AMBER)
    } else if !settings.pc_audio_enabled {
        ("Выключено", MUTED)
    } else if status.pc_audio.active {
        ("Передаётся", GREEN)
    } else {
        ("Тишина", MUTED)
    };
    let phone = status.phone;
    let (mic_state, mic_color) = if devices.cable_render_id.is_none() {
        ("Нужен VB-CABLE", AMBER)
    } else if !settings.mic_enabled {
        ("Выключено", MUTED)
    } else if phone.is_some_and(|p| !p.mic) {
        ("Выключен на телефоне", AMBER)
    } else if phone.is_some_and(|p| !p.mic_ready) {
        ("Откройте приложение на телефоне", AMBER)
    } else if status.mic.active {
        ("Передаётся", GREEN)
    } else if status.mic_demanded {
        ("Запрошен", AMBER)
    } else {
        ("Ждёт запись", MUTED)
    };
    let width = ui.available_width();
    let (row, _) = ui.allocate_exact_size(vec2(width, TILE_H), Sense::hover());
    let half = (width - GAP) / 2.0;
    let left = Rect::from_min_size(row.min, vec2(half, TILE_H));
    let right = Rect::from_min_size(pos2(row.left() + half + GAP, row.top()), vec2(half, TILE_H));
    let pc = TileSpec {
        title: "Звук ПК",
        route: "Компьютер → телефон",
        state: pc_state,
        state_color: pc_color,
        on: settings.pc_audio_enabled,
        active: status.pc_audio.active,
        icon: icon_speaker,
    };
    if tile(ui, left, &pc) {
        sh.set_pc_audio(!settings.pc_audio_enabled);
    }
    let mic = TileSpec {
        title: "Микрофон",
        route: "Телефон → компьютер",
        state: mic_state,
        state_color: mic_color,
        on: settings.mic_enabled,
        active: status.mic.active,
        icon: icon_mic,
    };
    if tile(ui, right, &mic) {
        sh.set_mic(!settings.mic_enabled);
    }

    controls_card(ui, status, devices, edit);

    // numbers
    card(ui, CARD, CARD_STROKE, |ui| {
        // the playout buffer lives on the receiving side; here: link, outgoing rate and codec
        let kbps: f32 = [&status.pc_audio, &status.mic].into_iter().filter(|s| s.active).map(|s| s.kbps).sum();
        let cols: [(&str, String); 3] = [
            ("Пинг", status.rtt_ms.map_or("—".into(), |r| format!("{r:.0} мс"))),
            ("Поток", if kbps > 0.0 { format!("{kbps:.0} кбит/с") } else { "—".into() }),
            ("Кодек", "Opus 48 кГц".into()),
        ];
        ui.columns(3, |cols_ui| {
            for (ui, (title, value)) in cols_ui.iter_mut().zip(cols) {
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(value).font(semibold(16.0)).color(TEXT));
                    ui.label(RichText::new(title).size(12.0).color(MUTED));
                });
            }
        });
    });
}

/// Local slider values while a volume slider is held: reports lag behind the drag.
#[derive(Default)]
struct VolumeEdit {
    pc: Option<f32>,
    phone: Option<f32>,
}

/// Detailed remote controls next to the one-button tiles.
fn controls_card(ui: &mut Ui, status: &Status, devices: &DeviceSummary, edit: &mut VolumeEdit) {
    let sh = shared();
    card(ui, CARD, CARD_STROKE, |ui| {
        ui.label(RichText::new("Управление").font(semibold(15.5)).color(TEXT));

        let pc_volume = status.pc.volume;
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(RichText::new("Громкость компьютера").size(14.5).color(TEXT));
            ui.horizontal(|ui| {
                let muted = pc_volume.is_some_and(|v| v.muted);
                if mute_button(ui, pc_volume.is_some(), muted) {
                    sh.set_mute(!muted);
                }
                if let Some(level) = level_slider(ui, pc_volume.map(|v| v.level), &mut edit.pc) {
                    sh.set_volume(level);
                }
            });
        });

        match status.phone {
            Some(phone) => {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Громкость телефона").size(14.5).color(TEXT));
                        if phone.volume.is_none() {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new("нет данных").size(12.5).color(MUTED));
                            });
                        }
                    });
                    if phone.volume.is_some() {
                        ui.horizontal(|ui| {
                            if let Some(level) = level_slider(ui, phone.volume, &mut edit.phone) {
                                sh.set_phone_volume(level);
                            }
                        });
                    }
                });
                let sub = if phone.mic_ready {
                    "Переключатель микрофона на телефоне"
                } else {
                    "Откройте AudioBridge на телефоне один раз: без этого Android не даёт записывать в фоне"
                };
                if switch_row(ui, "Микрофон телефона", Some(sub), phone.mic) {
                    sh.set_phone_mic(!phone.mic);
                }
            }
            None => edit.phone = None,
        }

        let on = devices.default_capture_is_cable;
        let has_cable = devices.cable_capture_id.is_some();
        let sub = if has_cable { "CABLE Output — основной микрофон Windows" } else { "Нужен VB-CABLE" };
        if switch_row(ui, "Микрофон по умолчанию", Some(sub), on) && has_cable {
            sh.set_mic_default(!on);
        }
    });
}

/// Speaker button of the PC volume row: MUTED with a slash while muted. Returns true when clicked.
fn mute_button(ui: &mut Ui, enabled: bool, muted: bool) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), sense);
    let p = ui.painter();
    if enabled && resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(8), CARD_HOVER);
    }
    let color = if !enabled {
        MUTED.gamma_multiply(0.5)
    } else if muted {
        MUTED
    } else {
        TEXT
    };
    let icon = rect.shrink(5.0);
    icon_speaker(p, icon, color);
    if muted {
        let inset = icon.width() * 0.12;
        let slash = [icon.left_top() + vec2(inset, inset), icon.right_bottom() - vec2(inset, inset)];
        p.line_segment(slash, Stroke::new(icon.width() * 0.08, color));
    }
    enabled && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A 0..=100 slider filling the rest of the row, then a right-aligned "NN %". Disabled while
/// `level` is unknown. `edit` keeps the local value while the slider is held, so lagging
/// reports don't make the handle jump. Returns the new level when its rounded value changed.
fn level_slider(ui: &mut Ui, level: Option<u8>, edit: &mut Option<f32>) -> Option<u8> {
    const VALUE_W: f32 = 44.0;
    let enabled = level.is_some();
    let mut value = edit.unwrap_or_else(|| f32::from(level.unwrap_or(0)));
    let before = value.round() as u8;
    ui.spacing_mut().slider_width = (ui.available_width() - VALUE_W - ui.spacing().item_spacing.x).max(40.0);
    let v = ui.visuals_mut();
    v.selection.bg_fill = TEAL;
    v.widgets.inactive.bg_fill = TOGGLE_OFF;
    v.widgets.inactive.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    v.widgets.hovered.bg_fill = Color32::WHITE;
    v.widgets.active.bg_fill = Color32::WHITE;
    let slider = egui::Slider::new(&mut value, 0.0..=100.0).show_value(false).trailing_fill(true);
    let resp = ui.add_enabled(enabled, slider);
    *edit = (resp.dragged() || resp.is_pointer_button_down_on()).then_some(value);
    let now = value.round() as u8;
    let (rect, _) = ui.allocate_exact_size(vec2(VALUE_W, 20.0), Sense::hover());
    let (text, color) = if enabled { (format!("{now} %"), TEXT) } else { ("—".to_owned(), MUTED) };
    ui.painter().text(rect.right_center(), Align2::RIGHT_CENTER, text, FontId::proportional(13.5), color);
    (enabled && resp.changed() && now != before).then_some(now)
}

impl UiApp {
    fn qr_texture(&mut self, ctx: &egui::Context, uri: &str) -> Option<&TextureHandle> {
        if self.qr.as_ref().is_none_or(|(u, _)| u != uri) {
            let code = match qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("QR encoding failed: {e}");
                    return None;
                }
            };
            let n = code.width();
            let pixels: Vec<Color32> = code
                .to_colors()
                .into_iter()
                .map(|c| if c == qrcode::Color::Dark { BG } else { Color32::WHITE })
                .collect();
            let image = egui::ColorImage::new([n, n], pixels);
            let tex = ctx.load_texture("pairing-qr", image, TextureOptions::NEAREST);
            self.qr = Some((uri.to_owned(), tex));
        }
        self.qr.as_ref().map(|(_, t)| t)
    }

    fn qr_card(&mut self, ui: &mut Ui, uri: Option<&str>, connected: bool, status: &Status) {
        let ctx = ui.ctx().clone();
        card(ui, CARD, CARD_STROKE, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Подключите телефон").font(semibold(18.0)).color(TEXT));
                ui.add_space(2.0);
                let side = 228.0;
                let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
                match uri.and_then(|u| self.qr_texture(&ctx, u)) {
                    Some(tex) => {
                        let painter = ui.painter();
                        gradient_rect(painter, rect, 20.0, INDIGO, TEAL);
                        let white = rect.shrink(4.0);
                        painter.rect_filled(white, CornerRadius::same(16), Color32::WHITE);
                        let inner = white.shrink(14.0);
                        painter.image(tex.id(), inner, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                    }
                    None => {
                        ui.painter().rect_filled(rect, CornerRadius::same(18), TOGGLE_OFF.gamma_multiply(0.4));
                        ui.put(rect, egui::Spinner::new().size(28.0).color(MUTED));
                    }
                }
            });
            ui.add_space(10.0);
            let steps = [
                "Установите AudioBridge на Android",
                "Нажмите «Добавить компьютер»",
                "Наведите камеру на этот QR-код",
            ];
            for (i, step) in steps.iter().enumerate() {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
                    let p = ui.painter();
                    gradient_rect(p, r, 12.0, INDIGO, TEAL);
                    p.text(r.center(), Align2::CENTER_CENTER, (i + 1).to_string(), semibold(12.5), Color32::WHITE);
                    ui.add_space(2.0);
                    ui.label(RichText::new(*step).size(13.5).color(TEXT));
                });
            }
            ui.add_space(4.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(format!("Этот компьютер: {}", shared().pc_name)).size(12.5).color(MUTED));
                if !connected {
                    if let Some(err) = &status.last_error {
                        ui.label(RichText::new(err).size(12.0).color(RED));
                    }
                }
            });
            if connected {
                ui.add_space(4.0);
                if wide_button(ui, "Назад", None) {
                    self.show_qr = false;
                }
            }
        });
    }
}

fn footer(ui: &mut Ui) {
    ui.vertical_centered(|ui| {
        let text = format!("v{} · open source на GitHub", env!("CARGO_PKG_VERSION"));
        if ui.link(RichText::new(text).size(12.0).color(MUTED)).clicked() {
            open_url(REPO_URL);
        }
    });
}

fn warnings(ui: &mut Ui, devices: &DeviceSummary, settings: &Settings) {
    let sh = shared();
    if devices.default_is_cable {
        card(ui, WARN_BG, WARN_STROKE, |ui| {
            ui.label(RichText::new("Звук выводится в CABLE Input").font(semibold(15.0)).color(AMBER));
            ui.label(
                RichText::new(
                    "Устройство вывода по умолчанию — виртуальный кабель, поэтому звук компьютера попадёт в микрофон. Передача на телефон приостановлена.",
                )
                .size(13.0)
                .color(TEXT),
            );
            match devices.render_restore_candidate(settings.last_default_render.as_deref()) {
                Some(target) => {
                    if ui.button(RichText::new(format!("Вернуть «{}»", target.name)).size(13.5)).clicked() {
                        sh.restore_default_render(target.id.clone());
                    }
                }
                None => {
                    ui.label(
                        RichText::new("Выберите динамики в Параметрах звука Windows.").size(12.5).color(MUTED),
                    );
                }
            }
        });
    }
    if devices.cable_render_id.is_none() {
        let state = sh.cable_install_state();
        card(ui, WARN_BG, WARN_STROKE, |ui| {
            ui.label(RichText::new("VB-CABLE не установлен").font(semibold(15.0)).color(AMBER));
            ui.label(
                RichText::new("Без него микрофон телефона недоступен на компьютере. Драйвер бесплатный.")
                    .size(13.0)
                    .color(TEXT),
            );
            let progress = match &state {
                InstallState::Downloading => Some("Загрузка VB-CABLE…"),
                InstallState::Extracting => Some("Распаковка…"),
                InstallState::Installing => Some("Нажмите «Install Driver» в окне установщика"),
                InstallState::Idle | InstallState::Failed(_) => None,
            };
            match progress {
                Some(text) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(AMBER));
                        ui.label(RichText::new(text).size(13.0).color(TEXT));
                    });
                }
                None => {
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Установить VB-CABLE").size(13.5)).clicked() {
                            cable_install::start();
                        }
                        if ui.link(RichText::new("Сайт VB-Audio").size(12.5).color(LINK)).clicked() {
                            open_url(VB_CABLE_URL);
                        }
                    });
                }
            }
            if let InstallState::Failed(err) = &state {
                ui.label(RichText::new(format!("Не удалось: {err}")).size(12.0).color(RED));
            }
        });
    }
}
