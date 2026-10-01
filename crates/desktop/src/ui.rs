//! The main window. Created on demand from the tray and destroyed on close, so no GL context or
//! window exists while AudioBridge sits in the tray. Repaints only on status changes.

use std::path::PathBuf;
use std::sync::Arc;

use audiobridge_core::pairing::PairingInfo;
use audiobridge_core::session::{ConnState, PathKind, Status};
use eframe::egui::{
    self, pos2, vec2, Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Layout, Margin,
    Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui,
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

const BG: Color32 = Color32::from_rgb(0x0E, 0x11, 0x16);
const CARD: Color32 = Color32::from_rgb(0x17, 0x1B, 0x22);
const CARD_STROKE: Color32 = Color32::from_rgb(0x24, 0x29, 0x33);
const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE8, 0xEB);
const MUTED: Color32 = Color32::from_rgb(0x8B, 0x93, 0xA1);
const GREEN: Color32 = Color32::from_rgb(0x34, 0xD3, 0x99);
const AMBER: Color32 = Color32::from_rgb(0xF5, 0xB5, 0x44);
const RED: Color32 = Color32::from_rgb(0xF8, 0x71, 0x71);
const LINK: Color32 = Color32::from_rgb(0x6E, 0xA8, 0xFE);
const TOGGLE_OFF: Color32 = Color32::from_rgb(0x3A, 0x40, 0x4C);
const WARN_BG: Color32 = Color32::from_rgb(0x26, 0x1F, 0x12);
const WARN_STROKE: Color32 = Color32::from_rgb(0x5A, 0x45, 0x15);

const VB_CABLE_URL: &str = "https://vb-audio.com/Cable/";

/// Opens the window and blocks until it is closed (the app keeps running in the tray).
pub fn run_window() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AudioBridge")
            .with_inner_size([380.0, 600.0])
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

pub fn path_label(p: PathKind) -> &'static str {
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
            if let Some(p) = s.path {
                t.push_str(" · ");
                t.push_str(path_label(p));
            }
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

fn card<R>(ui: &mut Ui, fill: Color32, stroke: Color32, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// iOS-style switch. Returns true when clicked.
fn toggle(ui: &mut Ui, on: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(42.0, 24.0), Sense::click());
    let t = ui.ctx().animate_bool_responsive(resp.id, on);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(12), TOGGLE_OFF.lerp_to_gamma(GREEN, t));
    let x = egui::lerp((rect.left() + 12.0)..=(rect.right() - 12.0), t);
    painter.circle_filled(pos2(x, rect.center().y), 9.0, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A row: title (+ optional subtitle) on the left, switch on the right.
fn switch_row(ui: &mut Ui, title: &str, subtitle: Option<&str>, on: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).size(15.0).color(TEXT));
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

#[derive(Default)]
struct UiApp {
    qr: Option<(String, TextureHandle)>,
    show_qr: bool,
}

impl eframe::App for UiApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::from(BG).to_array()
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let sh = shared();
        let status = sh.status.borrow().clone();
        let pairing = sh.pairing.borrow().as_ref().map(PairingInfo::to_uri);
        let settings = sh.settings();
        let devices = sh.devices();
        let connected = status.state == ConnState::Connected;
        if !connected {
            self.show_qr = false;
        }

        ui.painter().rect_filled(ui.max_rect(), CornerRadius::ZERO, BG);
        egui::Frame::new().inner_margin(Margin::symmetric(18, 16)).show(ui, |ui| {
            header(ui, &status);
            ui.add_space(10.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                warnings(ui, &devices, &settings);
                if !connected || self.show_qr {
                    self.qr_card(ui, pairing.as_deref(), connected, &status);
                } else {
                    connected_cards(ui, &status, &settings, &devices);
                    ui.vertical_centered(|ui| {
                        if ui.link(RichText::new("Показать QR-код").size(13.5).color(LINK)).clicked() {
                            self.show_qr = true;
                        }
                    });
                }
                card(ui, CARD, CARD_STROKE, |ui| {
                    if switch_row(ui, "Запускать вместе с Windows", None, settings.autostart) {
                        sh.set_autostart(!settings.autostart);
                    }
                });
            });
        });
    }
}

fn header(ui: &mut Ui, status: &Status) {
    ui.label(RichText::new("AudioBridge").font(semibold(24.0)).color(TEXT));
    let (text, color) = status_line(status);
    let galley = ui.painter().layout_no_wrap(text, FontId::proportional(13.0), color);
    let size = vec2(galley.size().x + 34.0, 26.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(13), color.gamma_multiply(0.14));
    painter.rect_stroke(rect, CornerRadius::same(13), Stroke::new(1.0, color.gamma_multiply(0.35)), StrokeKind::Inside);
    painter.circle_filled(pos2(rect.left() + 14.0, rect.center().y), 4.0, color);
    painter.galley(pos2(rect.left() + 24.0, rect.center().y - galley.size().y / 2.0), galley, color);
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
                .map(|c| if c == qrcode::Color::Dark { Color32::from_rgb(0x0E, 0x11, 0x16) } else { Color32::WHITE })
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
                ui.add_space(4.0);
                let side = 244.0;
                let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
                match uri.and_then(|u| self.qr_texture(&ctx, u)) {
                    Some(tex) => {
                        let painter = ui.painter();
                        painter.rect_filled(rect, CornerRadius::same(16), Color32::WHITE);
                        let inner = rect.shrink(16.0);
                        painter.image(tex.id(), inner, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                    }
                    None => {
                        ui.painter().rect_filled(rect, CornerRadius::same(16), TOGGLE_OFF.gamma_multiply(0.4));
                        ui.put(rect, egui::Spinner::new().size(28.0).color(MUTED));
                    }
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Отсканируйте QR-код в приложении AudioBridge на телефоне").size(15.0).color(TEXT),
                );
                ui.label(RichText::new(format!("Компьютер: {}", shared().pc_name)).size(12.5).color(MUTED));
                if !connected {
                    if let Some(err) = &status.last_error {
                        ui.label(RichText::new(err).size(12.0).color(RED));
                    }
                }
                if connected && ui.link(RichText::new("Назад").size(13.5).color(LINK)).clicked() {
                    self.show_qr = false;
                }
            });
        });
    }
}

fn connected_cards(ui: &mut Ui, status: &Status, settings: &Settings, devices: &DeviceSummary) {
    let sh = shared();
    card(ui, CARD, CARD_STROKE, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
            let painter = ui.painter();
            painter.circle_filled(rect.center(), 22.0, GREEN.gamma_multiply(0.18));
            let phone = Rect::from_center_size(rect.center(), vec2(14.0, 22.0));
            painter.rect_stroke(phone, CornerRadius::same(3), Stroke::new(2.0, GREEN), StrokeKind::Middle);
            painter.circle_filled(pos2(phone.center().x, phone.bottom() - 3.5), 1.4, GREEN);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let name = status.peer_name.as_deref().unwrap_or("Телефон");
                ui.label(RichText::new(name).font(semibold(17.0)).color(TEXT));
                let path = status.path.map_or("Подключено", path_label);
                ui.label(RichText::new(path).size(13.0).color(MUTED));
            });
        });
    });

    card(ui, CARD, CARD_STROKE, |ui| {
        let pc_sub = if devices.default_is_cable {
            "Приостановлено: вывод в CABLE Input"
        } else if status.pc_audio.active {
            "Передаётся"
        } else {
            "Тишина"
        };
        if switch_row(ui, "Звук компьютера → телефон", Some(pc_sub), settings.pc_audio_enabled) {
            sh.set_pc_audio(!settings.pc_audio_enabled);
        }
        ui.add_space(2.0);
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, CARD_STROKE));
        ui.add_space(6.0);
        let mic_sub = if devices.cable_render_id.is_none() {
            "Нужен VB-CABLE"
        } else if status.mic.active {
            "Передаётся в CABLE Output"
        } else if status.mic_demanded {
            "Запрошен приложением"
        } else {
            "Включится, когда приложение начнёт запись"
        };
        if switch_row(ui, "Микрофон телефона → компьютер", Some(mic_sub), settings.mic_enabled) {
            sh.set_mic(!settings.mic_enabled);
        }
    });

    card(ui, CARD, CARD_STROKE, |ui| {
        let buffer = [&status.pc_audio, &status.mic]
            .iter()
            .filter(|s| s.active && s.buffer_ms > 0.0)
            .map(|s| s.buffer_ms)
            .reduce(f32::max);
        let cols: [(&str, String); 3] = [
            ("Пинг", status.rtt_ms.map_or("—".into(), |r| format!("{r:.0} мс"))),
            ("Буфер", buffer.map_or("—".into(), |b| format!("{b:.0} мс"))),
            ("Канал", status.path.map_or("—", path_label).to_owned()),
        ];
        ui.columns(3, |cols_ui| {
            for (ui, (title, value)) in cols_ui.iter_mut().zip(cols) {
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(title).size(12.0).color(MUTED));
                    ui.add(egui::Label::new(RichText::new(value).size(14.0).color(TEXT)).wrap());
                });
            }
        });
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
            match devices.restore_candidate(settings.last_default_render.as_deref()) {
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
