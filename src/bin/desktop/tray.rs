//! 托盘图标、开机自启、套餐显示设置。
//! 与 desktop.rs 里的 win32 模块保持一致：手写 FFI，不引入 windows-sys。

use coding_quota::model::ProviderId;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc;

/// 托盘菜单发给主界面的指令。显示/隐藏不在其中：窗口隐藏后 egui 收不到重绘，
/// 主循环停摆，只能由托盘线程直接调 ShowWindow。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Refresh,
    ProvidersChanged,
    Quit,
}

static WIDGET_HWND: AtomicIsize = AtomicIsize::new(0);

/// 主界面每帧告知窗口句柄，供托盘线程显示/隐藏窗口。
pub fn set_widget_hwnd(hwnd: isize) {
    WIDGET_HWND.store(hwnd, Ordering::Relaxed);
}

/// 「显示套餐」子菜单条目，顺序与主界面一致。
pub const PROVIDERS: [(ProviderId, &str); 5] = [
    (ProviderId::Codex, "OpenAI Codex"),
    (ProviderId::Grok, "xAI Grok"),
    (ProviderId::Glm, "智谱 Coding Plan"),
    (ProviderId::Kimi, "Kimi Code"),
    (ProviderId::Cursor, "Cursor"),
];

fn provider_key(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Codex => "codex",
        ProviderId::Grok => "grok",
        ProviderId::Glm => "glm",
        ProviderId::Kimi => "kimi",
        ProviderId::Cursor => "cursor",
    }
}

fn hidden_file() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        std::path::PathBuf::from(appdata)
            .join("coding-quota")
            .join("hidden_providers.txt"),
    )
}

/// 存的是被隐藏的套餐，文件缺失即全部显示，将来新增平台也默认可见。
pub fn load_hidden() -> Vec<String> {
    let Some(path) = hidden_file() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.split_whitespace().map(str::to_string).collect()
}

pub fn is_hidden(hidden: &[String], provider: ProviderId) -> bool {
    let key = provider_key(provider);
    hidden.iter().any(|item| item == key)
}

fn toggle_hidden(provider: ProviderId) {
    let Some(path) = hidden_file() else { return };
    let key = provider_key(provider);
    let mut hidden = load_hidden();
    match hidden.iter().position(|item| item == key) {
        Some(index) => {
            hidden.remove(index);
        }
        None => hidden.push(key.to_string()),
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, hidden.join("\n"));
}

#[cfg(windows)]
pub fn spawn<F>(tx: mpsc::Sender<TrayCommand>, wake: F)
where
    F: Fn() + Send + 'static,
{
    imp::spawn(tx, Box::new(wake));
}

#[cfg(not(windows))]
pub fn spawn<F>(_tx: mpsc::Sender<TrayCommand>, _wake: F)
where
    F: Fn() + Send + 'static,
{
}

#[cfg(windows)]
mod imp {
    use super::{is_hidden, load_hidden, toggle_hidden, TrayCommand, PROVIDERS, WIDGET_HWND};
    use core::ffi::c_void;
    use std::cell::RefCell;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    const WM_TRAY: u32 = 0x8000 + 1; // WM_APP + 1
    const WM_DESTROY: u32 = 0x0002;
    const WM_COMMAND: u32 = 0x0111;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_LBUTTONDBLCLK: u32 = 0x0203;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_CONTEXTMENU: u32 = 0x007B;

    const NIM_ADD: u32 = 0;
    const NIM_DELETE: u32 = 2;
    const NIF_MESSAGE: u32 = 0x01;
    const NIF_ICON: u32 = 0x02;
    const NIF_TIP: u32 = 0x04;

    const MF_STRING: u32 = 0x0000;
    const MF_CHECKED: u32 = 0x0008;
    const MF_POPUP: u32 = 0x0010;
    const MF_SEPARATOR: u32 = 0x0800;
    const TPM_RIGHTBUTTON: u32 = 0x0002;
    const TPM_RETURNCMD: u32 = 0x0100;

    const SW_HIDE: i32 = 0;
    const SW_SHOWNOACTIVATE: i32 = 4;
    const HWND_BOTTOM: isize = 1;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOACTIVATE: u32 = 0x0010;

    const IMAGE_ICON: u32 = 1;
    const SM_CXSMICON: i32 = 49;
    const SM_CYSMICON: i32 = 50;
    const IDI_APPLICATION: usize = 32512;

    const ID_TOGGLE: usize = 1;
    const ID_REFRESH: usize = 2;
    const ID_AUTOSTART: usize = 3;
    const ID_QUIT: usize = 4;
    const ID_PROVIDER_BASE: usize = 100;

    const HKEY_CURRENT_USER: *mut c_void = 0x8000_0001_usize as *mut c_void;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_SET_VALUE: u32 = 0x0002;
    const KEY_ENUMERATE_SUB_KEYS: u32 = 0x0008;
    const REG_SZ: u32 = 1;
    const REG_DWORD: u32 = 4;
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NOTIFY_ICON_KEY: &str = r"Control Panel\NotifyIconSettings";
    const VALUE_NAME: &str = "CodingQuota";

    #[repr(C)]
    #[derive(Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Msg {
        hwnd: *mut c_void,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        pt: Point,
    }

    #[repr(C)]
    struct Guid {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    #[repr(C)]
    struct NotifyIconData {
        cb_size: u32,
        hwnd: *mut c_void,
        id: u32,
        flags: u32,
        callback_message: u32,
        icon: *mut c_void,
        tip: [u16; 128],
        state: u32,
        state_mask: u32,
        info: [u16; 256],
        version: u32,
        info_title: [u16; 64],
        info_flags: u32,
        guid_item: Guid,
        balloon_icon: *mut c_void,
    }

    type WndProcFn = unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize;

    #[repr(C)]
    struct WndClassExW {
        cb_size: u32,
        style: u32,
        wnd_proc: Option<WndProcFn>,
        cls_extra: i32,
        wnd_extra: i32,
        instance: *mut c_void,
        icon: *mut c_void,
        cursor: *mut c_void,
        background: *mut c_void,
        menu_name: *const u16,
        class_name: *const u16,
        icon_sm: *mut c_void,
    }

    #[repr(C)]
    struct BitmapInfoHeader {
        bi_size: u32,
        bi_width: i32,
        bi_height: i32,
        bi_planes: u16,
        bi_bit_count: u16,
        bi_compression: u32,
        bi_size_image: u32,
        bi_x_pels_per_meter: i32,
        bi_y_pels_per_meter: i32,
        bi_clr_used: u32,
        bi_clr_important: u32,
    }

    #[repr(C)]
    struct IconInfo {
        is_icon: i32,
        x_hotspot: u32,
        y_hotspot: u32,
        mask: *mut c_void,
        color: *mut c_void,
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterClassExW(class: *const WndClassExW) -> u16;
        fn CreateWindowExW(
            ex_style: u32,
            class_name: *const u16,
            window_name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: *mut c_void,
            menu: *mut c_void,
            instance: *mut c_void,
            param: *mut c_void,
        ) -> *mut c_void;
        fn DefWindowProcW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize;
        fn DestroyWindow(hwnd: *mut c_void) -> i32;
        fn PostQuitMessage(code: i32);
        fn GetMessageW(msg: *mut Msg, hwnd: *mut c_void, min: u32, max: u32) -> i32;
        fn TranslateMessage(msg: *const Msg) -> i32;
        fn DispatchMessageW(msg: *const Msg) -> isize;
        fn CreatePopupMenu() -> *mut c_void;
        fn AppendMenuW(menu: *mut c_void, flags: u32, id: usize, item: *const u16) -> i32;
        fn DestroyMenu(menu: *mut c_void) -> i32;
        fn TrackPopupMenu(
            menu: *mut c_void,
            flags: u32,
            x: i32,
            y: i32,
            reserved: i32,
            hwnd: *mut c_void,
            rect: *const c_void,
        ) -> i32;
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
        fn GetCursorPos(point: *mut Point) -> i32;
        fn GetSystemMetrics(index: i32) -> i32;
        fn LoadImageW(
            instance: *mut c_void,
            name: *const u16,
            kind: u32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> *mut c_void;
        fn LoadIconW(instance: *mut c_void, name: *const u16) -> *mut c_void;
        fn ShowWindow(hwnd: *mut c_void, command: i32) -> i32;
        fn IsWindowVisible(hwnd: *mut c_void) -> i32;
        fn SetWindowPos(
            hwnd: *mut c_void,
            after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn CreateIconIndirect(info: *mut IconInfo) -> *mut c_void;
        fn DestroyIcon(icon: *mut c_void) -> i32;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn Shell_NotifyIconW(message: u32, data: *mut NotifyIconData) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(
            key: *mut c_void,
            subkey: *const u16,
            options: u32,
            desired: u32,
            result: *mut *mut c_void,
        ) -> i32;
        fn RegCloseKey(key: *mut c_void) -> i32;
        fn RegEnumKeyExW(
            key: *mut c_void,
            index: u32,
            name: *mut u16,
            name_len: *mut u32,
            reserved: *mut u32,
            class: *mut u16,
            class_len: *mut u32,
            last_write: *mut c_void,
        ) -> i32;
        fn RegQueryValueExW(
            key: *mut c_void,
            name: *const u16,
            reserved: *mut u32,
            kind: *mut u32,
            data: *mut u8,
            size: *mut u32,
        ) -> i32;
        fn RegSetValueExW(
            key: *mut c_void,
            name: *const u16,
            reserved: u32,
            kind: u32,
            data: *const u8,
            size: u32,
        ) -> i32;
        fn RegDeleteValueW(key: *mut c_void, name: *const u16) -> i32;
    }

    struct Shared {
        tx: mpsc::Sender<TrayCommand>,
        wake: Box<dyn Fn() + Send>,
    }

    thread_local! {
        static SHARED: RefCell<Option<Shared>> = const { RefCell::new(None) };
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn spawn(tx: mpsc::Sender<TrayCommand>, wake: Box<dyn Fn() + Send>) {
        std::thread::spawn(move || unsafe { run(tx, wake) });
    }

    unsafe fn run(tx: mpsc::Sender<TrayCommand>, wake: Box<dyn Fn() + Send>) {
        SHARED.with(|cell| *cell.borrow_mut() = Some(Shared { tx, wake }));

        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("CodingQuotaTray");
        let class = WndClassExW {
            cb_size: std::mem::size_of::<WndClassExW>() as u32,
            style: 0,
            wnd_proc: Some(wnd_proc),
            cls_extra: 0,
            wnd_extra: 0,
            instance,
            icon: std::ptr::null_mut(),
            cursor: std::ptr::null_mut(),
            background: std::ptr::null_mut(),
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
            icon_sm: std::ptr::null_mut(),
        };
        if RegisterClassExW(&class) == 0 {
            return;
        }

        // 不调用 ShowWindow，这个窗口只用来接收托盘回调消息。
        let title = wide("编程额度");
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            return;
        }
        if !add_icon(hwnd, instance) {
            DestroyWindow(hwnd);
            return;
        }
        promote_icon();

        let mut msg: Msg = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        remove_icon(hwnd);
    }

    unsafe fn add_icon(hwnd: *mut c_void, instance: *mut c_void) -> bool {
        let mut data: NotifyIconData = std::mem::zeroed();
        data.cb_size = std::mem::size_of::<NotifyIconData>() as u32;
        data.hwnd = hwnd;
        data.flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.callback_message = WM_TRAY;
        data.icon = app_icon(instance);
        let tip = wide("编程额度");
        let len = tip.len().min(data.tip.len());
        data.tip[..len].copy_from_slice(&tip[..len]);
        let added = Shell_NotifyIconW(NIM_ADD, &mut data) != 0;
        // NIM_ADD 之后系统持有自己的拷贝，句柄可以立即释放
        if !data.icon.is_null() {
            DestroyIcon(data.icon);
        }
        added
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn CreateDIBSection(
            dc: *mut c_void,
            header: *const BitmapInfoHeader,
            usage: u32,
            bits: *mut *mut c_void,
            file: *mut c_void,
            offset: u32,
        ) -> *mut c_void;
        fn CreateBitmap(
            width: i32,
            height: i32,
            planes: u32,
            bpp: u32,
            bits: *const c_void,
        ) -> *mut c_void;
        fn DeleteObject(object: *mut c_void) -> i32;
    }

    unsafe fn remove_icon(hwnd: *mut c_void) {
        let mut data: NotifyIconData = std::mem::zeroed();
        data.cb_size = std::mem::size_of::<NotifyIconData>() as u32;
        data.hwnd = hwnd;
        data.id = 1;
        Shell_NotifyIconW(NIM_DELETE, &mut data);
    }

    /// 托盘图标：优先运行时逐像素绘制，失败再回退到嵌入的 .ico 资源。
    unsafe fn app_icon(instance: *mut c_void) -> *mut c_void {
        let icon = build_rounded_ring_icon();
        if !icon.is_null() {
            return icon;
        }
        let id = 1_usize as *const u16;
        let icon = LoadImageW(
            instance,
            id,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            0,
        );
        if !icon.is_null() {
            return icon;
        }
        let icon = LoadIconW(instance, id);
        if !icon.is_null() {
            return icon;
        }
        LoadIconW(std::ptr::null_mut(), IDI_APPLICATION as *const u16)
    }

    /// 运行时按系统小图标尺寸逐像素画一枚圆润的「进度环」图标：
    /// 圆角方形底 + 半透明轨道 + 绿色 3/4 圆头弧（与小组件进度条同系），
    /// 4× 超采样抗锯齿，比 .ico 整体缩放更锐利。
    unsafe fn build_rounded_ring_icon() -> *mut c_void {
        let mut size = GetSystemMetrics(SM_CXSMICON);
        if size <= 0 {
            size = 16;
        }
        let size = size.min(64) as usize;
        icon_from_bgra(size, &draw_ring_pixels(size))
    }

    fn draw_ring_pixels(size: usize) -> Vec<u8> {
        const SS: usize = 4;
        let (w, h) = (size as f32, size as f32);
        let (cx, cy) = (w / 2.0, h / 2.0);
        let half = w / 2.0;
        let corner = w * 0.30;
        let ring_r = w * 0.345;
        let ring_t = w * 0.13;
        let half_t = ring_t / 2.0;
        let start = -std::f32::consts::FRAC_PI_2; // 顶部
        let sweep = std::f32::consts::TAU * 0.75;
        let end = start + sweep;
        let cap1 = (cx + start.cos() * ring_r, cy + start.sin() * ring_r);
        let cap2 = (cx + end.cos() * ring_r, cy + end.sin() * ring_r);

        let bg_top = (46.0_f32, 56.0, 76.0); // #2E384C
        let bg_bottom = (22.0_f32, 27.0, 38.0); // #161B26
        let track = (255.0_f32, 255.0, 255.0);
        let track_a = 0.30;
        let arc = (112.0_f32, 219.0, 146.0); // #70DB92

        let mut pixels = vec![0u8; size * size * 4];
        for py in 0..size {
            for px in 0..size {
                let (mut acc_b, mut acc_g, mut acc_r, mut acc_a) =
                    (0f32, 0f32, 0f32, 0f32);
                for sy in 0..SS {
                    for sx in 0..SS {
                        let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                        let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                        // 圆角矩形 SDF：外距 + 内距 - 圆角半径
                        let qx = (x - cx).abs() - (half - corner);
                        let qy = (y - cy).abs() - (half - corner);
                        let outside = (qx.max(0.0)).hypot(qy.max(0.0));
                        let dist = outside + qx.max(qy).min(0.0) - corner;
                        let bg_cov = (0.5 - dist).clamp(0.0, 1.0);
                        let t = (y / h).clamp(0.0, 1.0);
                        // 本子样本局部预乘值：先背景，再环；最后才累加。
                        // 不能在累加器上直接合成，否则破坏求和不变量。
                        let (br, bg, bb) = (
                            bg_top.0 + (bg_bottom.0 - bg_top.0) * t,
                            bg_top.1 + (bg_bottom.1 - bg_top.1) * t,
                            bg_top.2 + (bg_bottom.2 - bg_top.2) * t,
                        );
                        let (mut sr, mut sg, mut sb, mut sa) =
                            (br * bg_cov, bg * bg_cov, bb * bg_cov, bg_cov);

                        let d = ((x - cx) * (x - cx) + (y - cy) * (y - cy)).sqrt();
                        let ring_cov = (half_t - (d - ring_r).abs()).clamp(0.0, 1.0);
                        if ring_cov > 0.0 {
                            let mut ang = (y - cy).atan2(x - cx) - start;
                            while ang < 0.0 {
                                ang += std::f32::consts::TAU;
                            }
                            let in_cap1 = ((x - cap1.0) * (x - cap1.0)
                                + (y - cap1.1) * (y - cap1.1))
                                .sqrt()
                                <= half_t;
                            let in_cap2 = ((x - cap2.0) * (x - cap2.0)
                                + (y - cap2.1) * (y - cap2.1))
                                .sqrt()
                                <= half_t;
                            let (cr, cg, cb, alpha) = if ang <= sweep || in_cap1 || in_cap2 {
                                (arc.0, arc.1, arc.2, ring_cov)
                            } else {
                                (track.0, track.1, track.2, ring_cov * track_a)
                            };
                            let k = 1.0 - alpha;
                            sr = cr * alpha + sr * k;
                            sg = cg * alpha + sg * k;
                            sb = cb * alpha + sb * k;
                            sa = alpha + sa * k;
                        }
                        acc_r += sr;
                        acc_g += sg;
                        acc_b += sb;
                        acc_a += sa;
                    }
                }
                let n = (SS * SS) as f32;
                let base = (py * size + px) * 4;
                // 预乘颜色直接给 CreateIconIndirect；PNG 预览时再解除预乘
                pixels[base] = (acc_b / n).round().clamp(0.0, 255.0) as u8;
                pixels[base + 1] = (acc_g / n).round().clamp(0.0, 255.0) as u8;
                pixels[base + 2] = (acc_r / n).round().clamp(0.0, 255.0) as u8;
                // 覆盖率是 0..1，写入字节前要放大到 0..255
                pixels[base + 3] = (acc_a / n * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
        pixels
    }

    /// 32bpp ARGB DIB → HICON。位图用负高度自上而下，与像素数组顺序一致。
    unsafe fn icon_from_bgra(size: usize, bgra: &[u8]) -> *mut c_void {
        let header = BitmapInfoHeader {
            bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            bi_width: size as i32,
            bi_height: -(size as i32),
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: 0,
            bi_size_image: 0,
            bi_x_pels_per_meter: 0,
            bi_y_pels_per_meter: 0,
            bi_clr_used: 0,
            bi_clr_important: 0,
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let color = CreateDIBSection(
            std::ptr::null_mut(),
            &header,
            0,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if color.is_null() || bits.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());

        // 全零掩码 + 32bpp alpha，Windows 会按 alpha 混合
        let stride = ((size + 15) / 16) * 2;
        let mask_bits = vec![0u8; stride * size];
        let mask = CreateBitmap(
            size as i32,
            size as i32,
            1,
            1,
            mask_bits.as_ptr() as *const c_void,
        );
        if mask.is_null() {
            DeleteObject(color);
            return std::ptr::null_mut();
        }

        let mut info = IconInfo {
            is_icon: 1,
            x_hotspot: 0,
            y_hotspot: 0,
            mask,
            color,
        };
        let icon = CreateIconIndirect(&mut info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }

    /// 隐藏后 egui 主循环停摆，所以显示/隐藏全部由托盘线程自己完成。
    unsafe fn toggle_window() {
        let hwnd = WIDGET_HWND.load(Ordering::Relaxed) as *mut c_void;
        if hwnd.is_null() {
            return;
        }
        if IsWindowVisible(hwnd) != 0 {
            ShowWindow(hwnd, SW_HIDE);
        } else {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            // 重新压回底层，保持「贴在其他窗口下面」的行为
            SetWindowPos(
                hwnd,
                HWND_BOTTOM,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    /// 同理，隐藏状态下 eframe 走不完退出流程，先把窗口显示回来再让主循环收尾，
    /// 这样窗口位置仍会由 on_exit 落盘。
    unsafe fn request_quit() {
        let hwnd = WIDGET_HWND.load(Ordering::Relaxed) as *mut c_void;
        if !hwnd.is_null() && IsWindowVisible(hwnd) == 0 {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        dispatch(TrayCommand::Quit);
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: *mut c_void,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        match msg {
            WM_TRAY => {
                match (lparam as u32) & 0xFFFF {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => toggle_window(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    _ => {}
                }
                0
            }
            WM_COMMAND => {
                handle_command(wparam & 0xFFFF);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn show_menu(hwnd: *mut c_void) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let hidden = load_hidden();
        append(menu, MF_STRING, ID_TOGGLE, "显示 / 隐藏窗口");
        append(menu, MF_STRING, ID_REFRESH, "刷新额度");
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());

        let autostart = if autostart_enabled() {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        append(menu, autostart, ID_AUTOSTART, "开机自启");

        let submenu = CreatePopupMenu();
        if !submenu.is_null() {
            for (index, (provider, label)) in PROVIDERS.iter().enumerate() {
                let mut flags = MF_STRING;
                if !is_hidden(&hidden, *provider) {
                    flags |= MF_CHECKED;
                }
                append(submenu, flags, ID_PROVIDER_BASE + index, label);
            }
            let label = wide("显示套餐");
            AppendMenuW(menu, MF_STRING | MF_POPUP, submenu as usize, label.as_ptr());
        }

        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        append(menu, MF_STRING, ID_QUIT, "退出");

        let mut cursor = Point::default();
        GetCursorPos(&mut cursor);
        // 托盘菜单必须先把宿主窗口提到前台，否则点击外部不会关闭菜单。
        SetForegroundWindow(hwnd);
        let selected = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            cursor.x,
            cursor.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        DestroyMenu(menu);
        if selected > 0 {
            handle_command(selected as usize);
        }
    }

    unsafe fn append(menu: *mut c_void, flags: u32, id: usize, text: &str) {
        let text = wide(text);
        AppendMenuW(menu, flags, id, text.as_ptr());
    }

    fn handle_command(id: usize) {
        match id {
            ID_TOGGLE => unsafe { toggle_window() },
            ID_REFRESH => dispatch(TrayCommand::Refresh),
            ID_QUIT => unsafe { request_quit() },
            ID_AUTOSTART => {
                set_autostart(!autostart_enabled());
            }
            id if (ID_PROVIDER_BASE..ID_PROVIDER_BASE + PROVIDERS.len()).contains(&id) => {
                toggle_hidden(PROVIDERS[id - ID_PROVIDER_BASE].0);
                dispatch(TrayCommand::ProvidersChanged);
            }
            _ => {}
        }
    }

    fn dispatch(command: TrayCommand) {
        SHARED.with(|cell| {
            if let Some(shared) = cell.borrow().as_ref() {
                if shared.tx.send(command).is_ok() {
                    (shared.wake)();
                }
            }
        });
    }

    pub fn autostart_enabled() -> bool {
        unsafe {
            let subkey = wide(RUN_KEY);
            let mut key: *mut c_void = std::ptr::null_mut();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut key,
            ) != 0
            {
                return false;
            }
            let name = wide(VALUE_NAME);
            let status = RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            RegCloseKey(key);
            status == 0
        }
    }

    fn set_autostart(enable: bool) -> bool {
        let Ok(executable) = std::env::current_exe() else {
            return false;
        };
        unsafe {
            let subkey = wide(RUN_KEY);
            let mut key: *mut c_void = std::ptr::null_mut();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                &mut key,
            ) != 0
            {
                return false;
            }
            let name = wide(VALUE_NAME);
            let status = if enable {
                let command = wide(&format!("\"{}\"", executable.display()));
                RegSetValueExW(
                    key,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    command.as_ptr() as *const u8,
                    (command.len() * 2) as u32,
                )
            } else {
                RegDeleteValueW(key, name.as_ptr())
            };
            RegCloseKey(key);
            status == 0
        }
    }

    /// Win11 默认把新托盘图标收进「隐藏的图标」弹窗。首次运行把它提到任务栏上；
    /// 若用户之后手动隐藏（Explorer 会写 IsPromoted=0），这里不再覆盖。
    /// Explorer 要等图标注册后才建这个子键，所以放后台线程重试。
    fn promote_icon() {
        std::thread::spawn(|| {
            let Ok(executable) = std::env::current_exe() else {
                return;
            };
            let wanted = executable.to_string_lossy().to_lowercase();
            for _ in 0..12 {
                if unsafe { try_promote(&wanted) } {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        });
    }

    /// 返回是否已找到本程序对应的图标子键（找到即不必再重试）。
    unsafe fn try_promote(wanted: &str) -> bool {
        let parent_name = wide(NOTIFY_ICON_KEY);
        let mut parent: *mut c_void = std::ptr::null_mut();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            parent_name.as_ptr(),
            0,
            KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE,
            &mut parent,
        ) != 0
        {
            return false;
        }
        let mut found = false;
        let mut index = 0_u32;
        loop {
            let mut name = [0_u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(
                parent,
                index,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) != 0
            {
                break;
            }
            index += 1;
            let sub_name: Vec<u16> = name[..len as usize]
                .iter()
                .copied()
                .chain(std::iter::once(0))
                .collect();
            let mut sub: *mut c_void = std::ptr::null_mut();
            if RegOpenKeyExW(
                parent,
                sub_name.as_ptr(),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                &mut sub,
            ) != 0
            {
                continue;
            }
            let matches = read_string(sub, "ExecutablePath")
                .map(|path| path.to_lowercase() == wanted)
                .unwrap_or(false);
            if matches {
                found = true;
                if !has_value(sub, "IsPromoted") {
                    let one: u32 = 1;
                    let value = wide("IsPromoted");
                    RegSetValueExW(
                        sub,
                        value.as_ptr(),
                        0,
                        REG_DWORD,
                        &one as *const u32 as *const u8,
                        4,
                    );
                }
            }
            RegCloseKey(sub);
            if found {
                break;
            }
        }
        RegCloseKey(parent);
        found
    }

    unsafe fn read_string(key: *mut c_void, name: &str) -> Option<String> {
        let name = wide(name);
        let mut buffer = [0_u16; 1024];
        let mut size = (buffer.len() * 2) as u32;
        if RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            buffer.as_mut_ptr() as *mut u8,
            &mut size,
        ) != 0
        {
            return None;
        }
        let chars = (size as usize / 2).min(buffer.len());
        let text: Vec<u16> = buffer[..chars]
            .iter()
            .copied()
            .take_while(|unit| *unit != 0)
            .collect();
        Some(String::from_utf16_lossy(&text))
    }

    unsafe fn has_value(key: *mut c_void, name: &str) -> bool {
        let name = wide(name);
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) == 0
    }
}
