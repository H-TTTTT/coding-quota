#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use coding_quota::model::{ProviderId, ProviderReport, Snapshot};
use coding_quota::render::{ago_cn, compact_until_cn, label_cn, title_cn};
use coding_quota::{credentials, fetch};
use eframe::egui;
#[cfg(windows)]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::mpsc;
use std::time::Duration;

const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const QUOTA_VALUE_WIDTH: f32 = 86.0;
/// 额度标签行（左侧标签 + 右侧重置时间）统一字号，与「剩余 xx%」一致。
/// 注意不能用 `.small().monospace()`：两者都是设置 text_style，后者会覆盖前者。
const LABEL_ROW_SIZE: f32 = 11.0;

enum Cmd {
    Refresh,
}

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_decorations(false)
        .with_transparent(true)
        .with_taskbar(false)
        .with_resizable(true)
        .with_inner_size([340.0, 740.0])
        .with_min_inner_size([280.0, 200.0])
        .with_window_level(egui::WindowLevel::AlwaysOnBottom);
    // 恢复上次关闭时的窗口位置
    if let Some((x, y)) = load_window_pos() {
        viewport = viewport.with_position([x, y]);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "编程额度",
        options,
        Box::new(|cc| {
            load_chinese_font(&cc.egui_ctx);
            let mut visuals = egui::Visuals::light();
            visuals.panel_fill = egui::Color32::TRANSPARENT;
            visuals.window_fill = egui::Color32::TRANSPARENT;
            visuals.extreme_bg_color = egui::Color32::TRANSPARENT;
            visuals.faint_bg_color = egui::Color32::TRANSPARENT;
            visuals.code_bg_color = egui::Color32::TRANSPARENT;
            visuals.widgets.noninteractive.bg_fill = egui::Color32::TRANSPARENT;
            visuals.widgets.inactive.bg_fill = egui::Color32::from_white_alpha(18);
            visuals.widgets.hovered.bg_fill = egui::Color32::from_white_alpha(36);
            visuals.widgets.active.bg_fill = egui::Color32::from_white_alpha(48);
            visuals.widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            visuals.override_text_color = Some(egui::Color32::from_rgb(242, 242, 242));
            let radius = egui::CornerRadius::same(6);
            visuals.widgets.noninteractive.corner_radius = radius;
            visuals.widgets.inactive.corner_radius = radius;
            visuals.widgets.hovered.corner_radius = radius;
            visuals.widgets.active.corner_radius = radius;
            visuals.widgets.open.corner_radius = radius;
            cc.egui_ctx.set_visuals(visuals);
            Ok(Box::new(DesktopApp::new()))
        }),
    )
}

/// egui 自带字体不含中文，从系统目录加载微软雅黑（失败则尝试等线、黑体）。
fn load_chinese_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\Deng.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("zh".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "zh".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "zh".into());
            ctx.set_fonts(fonts);
            return;
        }
    }
}

/// 窗口位置持久化：%APPDATA%\coding-quota\window_pos.txt，纯文本 "x y"。
fn pos_file_path() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        std::path::PathBuf::from(appdata)
            .join("coding-quota")
            .join("window_pos.txt"),
    )
}

fn load_window_pos() -> Option<(f32, f32)> {
    let text = std::fs::read_to_string(pos_file_path()?).ok()?;
    let mut parts = text.split_whitespace();
    let x: f32 = parts.next()?.parse().ok()?;
    let y: f32 = parts.next()?.parse().ok()?;
    // 粗略过滤明显非法的坐标（屏外保护由 update 里的相交检查兜底）
    if x.abs() > 10_000.0 || y.abs() > 10_000.0 {
        return None;
    }
    Some((x, y))
}

fn save_window_pos(x: i32, y: i32) {
    let Some(path) = pos_file_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{x} {y}"));
}

/// Windows 窗口特效：真透明 + 浅白染色 + DWM 圆角。
/// 不再叠加 SetWindowRgn：GDI 区域和 DWM 圆角叠在一起会在边缘留下白边。
#[cfg(windows)]
mod win32 {
    use core::ffi::c_void;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct Rect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }

    #[link(name = "user32")]
    extern "system" {
        pub fn GetWindowLongPtrW(hwnd: *mut c_void, index: i32) -> isize;
        pub fn SetWindowLongPtrW(hwnd: *mut c_void, index: i32, new_value: isize) -> isize;
        pub fn GetCursorPos(point: *mut Point) -> i32;
        pub fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
        pub fn SetWindowPos(
            hwnd: *mut c_void,
            after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
    }

    #[link(name = "dwmapi")]
    extern "system" {
        pub fn DwmSetWindowAttribute(
            hwnd: *mut c_void,
            attr: u32,
            value: *const u32,
            size: u32,
        ) -> i32;
    }

    pub unsafe fn apply_noactivate(hwnd: *mut c_void) {
        const GWL_EXSTYLE: i32 = -20;
        const WS_EX_NOACTIVATE: isize = 0x0800_0000;
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if style & WS_EX_NOACTIVATE == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_NOACTIVATE);
        }
    }

    pub unsafe fn apply_glass(hwnd: *mut c_void) {
        const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
        const DWMWCP_ROUND: u32 = 2;
        let corner = DWMWCP_ROUND;
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner, 4);
    }

}

#[cfg(windows)]
fn apply_window_chrome(frame: &eframe::Frame, glass_applied: &mut bool) {
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win) = handle.as_raw() else {
        return;
    };
    let hwnd = win.hwnd.get() as *mut core::ffi::c_void;
    unsafe {
        win32::apply_noactivate(hwnd);
        if !*glass_applied {
            win32::apply_glass(hwnd);
            *glass_applied = true;
        }
    }
}

#[cfg(not(windows))]
fn apply_window_chrome(_frame: &eframe::Frame, _glass_applied: &mut bool) {}

#[cfg(windows)]
fn hwnd_of(frame: &eframe::Frame) -> Option<*mut core::ffi::c_void> {
    let handle = frame.window_handle().ok()?;
    let RawWindowHandle::Win32(win) = handle.as_raw() else {
        return None;
    };
    Some(win.hwnd.get() as *mut core::ffi::c_void)
}

#[cfg(windows)]
fn drag_offset_begin(frame: &eframe::Frame) -> Option<(i32, i32)> {
    let hwnd = hwnd_of(frame)?;
    unsafe {
        let mut cursor = win32::Point::default();
        let mut rect = win32::Rect::default();
        if win32::GetCursorPos(&mut cursor) == 0 || win32::GetWindowRect(hwnd, &mut rect) == 0 {
            return None;
        }
        Some((cursor.x - rect.left, cursor.y - rect.top))
    }
}

#[cfg(windows)]
fn drag_window_to_cursor(frame: &eframe::Frame, offset: (i32, i32)) {
    let Some(hwnd) = hwnd_of(frame) else {
        return;
    };
    unsafe {
        let mut cursor = win32::Point::default();
        if win32::GetCursorPos(&mut cursor) == 0 {
            return;
        }
        const SWP_NOSIZE: u32 = 0x0001;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;
        win32::SetWindowPos(
            hwnd,
            0,
            cursor.x - offset.0,
            cursor.y - offset.1,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

enum IconKind {
    Refresh,
    Close,
}

fn icon_button(ui: &mut egui::Ui, kind: IconKind, tip: &str) -> egui::Response {
    let size = egui::vec2(22.0, 22.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    if response.is_pointer_button_down_on() {
        painter.circle_filled(rect.center(), 10.0, egui::Color32::from_white_alpha(30));
    } else if response.hovered() {
        painter.circle_filled(rect.center(), 10.0, egui::Color32::from_white_alpha(18));
    }
    let color = egui::Color32::from_rgba_unmultiplied(242, 242, 242, 230);
    let stroke = egui::Stroke::new(1.5_f32, color);
    let c = rect.center();
    match kind {
        IconKind::Close => {
            let r = 4.4;
            painter.line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], stroke);
            painter.line_segment([c + egui::vec2(r, -r), c + egui::vec2(-r, r)], stroke);
        }
        IconKind::Refresh => {
            let r = 5.0;
            let start = -std::f32::consts::FRAC_PI_2 + 0.45;
            let sweep = std::f32::consts::TAU * 0.78;
            let mut points = Vec::with_capacity(22);
            for i in 0..=20 {
                let a = start + sweep * (i as f32 / 20.0);
                points.push(c + egui::vec2(a.cos() * r, a.sin() * r));
            }
            painter.add(egui::Shape::line(points, stroke));
            let end = start + sweep;
            let tip_pos = c + egui::vec2(end.cos() * r, end.sin() * r);
            let tangent = egui::vec2(-end.sin(), end.cos());
            let normal = egui::vec2(end.cos(), end.sin());
            painter.line_segment([tip_pos - tangent * 4.0 + normal * 2.1, tip_pos], stroke);
            painter.line_segment([tip_pos - tangent * 4.0 - normal * 2.1, tip_pos], stroke);
        }
    }
    response.on_hover_text(tip)
}

struct DesktopApp {
    snapshot: Option<Snapshot>,
    snap_rx: mpsc::Receiver<Snapshot>,
    cmd_tx: mpsc::Sender<Cmd>,
    was_focused: bool,
    glass_applied: bool,
    drag_offset: Option<(i32, i32)>,
    last_seen_pos: Option<(i32, i32)>,
    saved_pos: Option<(i32, i32)>,
    last_pos_save: std::time::Instant,
    pos_guard_done: bool,
}

impl DesktopApp {
    fn new() -> Self {
        let (snap_tx, snap_rx) = mpsc::channel::<Snapshot>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            loop {
                // Reload the database for every refresh. A transient UNC/SQLite
                // failure must not leave the widget permanently credential-less.
                let snapshot = match credentials::load() {
                    Ok(creds) => rt.block_on(fetch::fetch_all(&creds, None)),
                    Err(err) => {
                        let message = format!("凭据读取失败：{err}");
                        Snapshot {
                            fetched_at: chrono::Utc::now(),
                            reports: [
                                ProviderId::Codex,
                                ProviderId::Grok,
                                ProviderId::Glm,
                                ProviderId::Kimi,
                                ProviderId::Cursor,
                            ]
                            .into_iter()
                            .map(|provider| ProviderReport::err(provider, None, message.clone()))
                            .collect(),
                        }
                    }
                };
                if snap_tx.send(snapshot).is_err() {
                    return;
                }
                match cmd_rx.recv_timeout(REFRESH_INTERVAL) {
                    Ok(Cmd::Refresh) | Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Self {
            snapshot: None,
            snap_rx,
            cmd_tx,
            was_focused: false,
            glass_applied: false,
            drag_offset: None,
            last_seen_pos: None,
            saved_pos: None,
            last_pos_save: std::time::Instant::now() - Duration::from_secs(1),
            pos_guard_done: false,
        }
    }
}

impl eframe::App for DesktopApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 关闭时把最后位置落盘，供下次启动恢复
        if let Some((x, y)) = self.last_seen_pos {
            save_window_pos(x, y);
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        while let Ok(snapshot) = self.snap_rx.try_recv() {
            self.snapshot = Some(snapshot);
        }
        apply_window_chrome(frame, &mut self.glass_applied);
        if ctx.input(|input| input.viewport().minimized == Some(true)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::AlwaysOnBottom,
            ));
        }
        // 记录窗口位置：拖动中限频落盘，关闭时由 on_exit 兜底
        if let Some(outer) = ctx.input(|input| {
            let v = input.viewport();
            (v.minimized != Some(true)).then_some(v.outer_rect).flatten()
        }) {
            if !self.pos_guard_done {
                self.pos_guard_done = true;
                // 上次保存的位置可能因显示器变化落在屏外，拉回主屏左上角
                if let Some(monitor) = ctx.input(|input| input.viewport().monitor_size) {
                    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, monitor);
                    if !screen.intersects(outer) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                            100.0, 100.0,
                        )));
                    }
                }
            }
            let pos = (outer.min.x.round() as i32, outer.min.y.round() as i32);
            self.last_seen_pos = Some(pos);
            if self.saved_pos != Some(pos) {
                if self.last_pos_save.elapsed() >= Duration::from_millis(500) {
                    save_window_pos(pos.0, pos.1);
                    self.saved_pos = Some(pos);
                    self.last_pos_save = std::time::Instant::now();
                } else {
                    ctx.request_repaint_after(Duration::from_millis(300));
                }
            }
        }
        let focused = ctx.input(|input| input.viewport().focused).unwrap_or(false);
        if focused && !self.was_focused {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::AlwaysOnBottom,
            ));
        }
        self.was_focused = focused;
        if self.drag_offset.is_some() {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_secs(30));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(egui::Margin::same(10)),
            )
            .show(ctx, |ui| {
                // 接近「我的文件夹」的高透明度，再叠一层中性灰，压低壁纸干扰。
                ui.painter().rect_filled(
                    ctx.screen_rect(),
                    0.0,
                    egui::Color32::from_black_alpha(72),
                );
                let mut over_icon = false;
                let title_bar = ui
                    .horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("编程额度")
                                .strong()
                                .color(egui::Color32::from_rgb(242, 242, 242)),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            let close = icon_button(ui, IconKind::Close, "关闭");
                            if close.hovered() {
                                over_icon = true;
                            }
                            if close.clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            let refresh = icon_button(ui, IconKind::Refresh, "刷新");
                            if refresh.hovered() {
                                over_icon = true;
                            }
                            if refresh.clicked() {
                                let _ = self.cmd_tx.send(Cmd::Refresh);
                            }
                            if let Some(snapshot) = &self.snapshot {
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new(ago_cn(snapshot.fetched_at))
                                        .color(egui::Color32::from_rgb(218, 218, 218))
                                        .small(),
                                );
                            }
                        });
                    })
                    .response
                    .interact(egui::Sense::drag());
                if title_bar.drag_started() && !over_icon {
                    #[cfg(windows)]
                    {
                        self.drag_offset = drag_offset_begin(frame);
                    }
                    #[cfg(not(windows))]
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                #[cfg(windows)]
                {
                    if title_bar.dragged() {
                        if let Some(offset) = self.drag_offset {
                            drag_window_to_cursor(frame, offset);
                        }
                    } else {
                        self.drag_offset = None;
                    }
                }
                let sep_y = ui.cursor().top();
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    sep_y,
                    egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha(44)),
                );
                ui.add_space(8.0);

                match &self.snapshot {
                    None => {
                        ui.label(
                            egui::RichText::new("加载中…")
                                .color(egui::Color32::from_rgb(242, 242, 242)),
                        );
                    }
                    Some(snapshot) => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for report in &snapshot.reports {
                                    draw_report(ui, report);
                                    ui.add_space(6.0);
                                }
                            });
                    }
                }
            });
    }
}

fn draw_report(ui: &mut egui::Ui, report: &ProviderReport) {
    let mut title = title_cn(&report.title).to_string();
    if let Some(plan) = &report.plan {
        title.push_str(" · ");
        title.push_str(plan);
    }
    egui::Frame::new()
        .fill(egui::Color32::from_black_alpha(18))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .strong()
                        .color(egui::Color32::from_rgb(242, 242, 242)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(identity) = &report.identity {
                        ui.label(
                            egui::RichText::new(identity)
                                .color(egui::Color32::from_rgb(218, 218, 218))
                                .small(),
                        );
                    }
                });
            });
            if let Some(error) = &report.error {
                ui.label(
                    egui::RichText::new(error)
                        .color(egui::Color32::from_rgb(190, 50, 40))
                        .small(),
                );
                return;
            }
            for window in &report.windows {
                let remaining = (1.0 - window.used_fraction).clamp(0.0, 1.0) as f32;
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(label_cn(&window.label))
                            .size(LABEL_ROW_SIZE)
                            .color(egui::Color32::from_rgb(242, 242, 242)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(reset) = window.reset_at {
                            ui.label(
                                egui::RichText::new(compact_until_cn(reset))
                                    .size(LABEL_ROW_SIZE)
                                    .monospace()
                                    .color(egui::Color32::from_rgb(218, 218, 218)),
                            );
                        }
                    });
                });
                let extra = if report.provider == ProviderId::Kimi {
                    format!("剩余 {:.0}%", remaining * 100.0)
                } else {
                    match (window.used, window.limit) {
                        (Some(used), Some(limit)) => {
                            format!("剩余 {:.0}/{limit:.0}", (limit - used).max(0.0))
                        }
                        _ => format!("剩余 {:.0}%", remaining * 100.0),
                    }
                };
                ui.horizontal(|ui| {
                    let bar_width = (ui.available_width() - QUOTA_VALUE_WIDTH).max(80.0);
                    let bar = egui::ProgressBar::new(remaining)
                        .desired_width(bar_width)
                        .desired_height(13.0)
                        .fill(remaining_color(remaining));
                    ui.add(bar);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(extra)
                                .size(11.0)
                                .color(egui::Color32::from_rgb(242, 242, 242)),
                        );
                    });
                });
                ui.add_space(2.0);
            }
        });
}


fn remaining_color(remaining: f32) -> egui::Color32 {
    if remaining <= 0.10 {
        egui::Color32::from_rgb(220, 70, 60)
    } else if remaining <= 0.30 {
        egui::Color32::from_rgb(220, 180, 60)
    } else {
        egui::Color32::from_rgb(90, 180, 90)
    }
}
