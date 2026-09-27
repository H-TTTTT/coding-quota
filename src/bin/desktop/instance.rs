//! 单实例锁：挂件重复启动会出现两个托盘图标，两路 worker 各自刷同一批端点，
//! Devin 还要重复拉起 CLI（每轮最长 25s），缓存也在两个进程间抢着写。
//! 已有一个实例时，第二次的行为是提示后立刻退出——不碰先起来那个的窗口和托盘。

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    const ERROR_ALREADY_INSTANTIATED: u32 = 183;
    const MB_OK: u32 = 0x0000_0000;
    const MB_ICONINFORMATION: u32 = 0x0000_0040;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(
            attributes: *const c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn GetLastError() -> u32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 抢占单实例锁。锁名不带 `Global\` 前缀，因此按登录会话隔离：
    /// 远程桌面会话里各开一个挂件是允许的。句柄故意不关闭（进程退出时系统回收），
    /// 所以返回的 `*mut c_void` 只当哨兵用。
    pub fn acquire() -> bool {
        let available = std::ptr::null_mut();
        unsafe {
            let name = wide(r"Local\coding-quota-gui");
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle.is_null() {
                // 建不了锁就别拦住用户，宁可重复开一个挂件。
                return true;
            }
            if GetLastError() != ERROR_ALREADY_INSTANTIATED {
                // 原始指针不会被释放，互斥句柄自然活到进程退出（由系统回收）。
                return true;
            }
            let text = wide("额度挂件已经在运行：请到托盘图标里操作（右键菜单可退出）。\n再次启动不会重复刷新。");
            let caption = wide("编程额度");
            MessageBoxW(
                available,
                text.as_ptr(),
                caption.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
            false
        }
    }
}

/// 非 Windows 平台没有托盘重复启动的问题（开发/测试环境），一律放行。
#[cfg(not(windows))]
pub fn acquire() -> bool {
    true
}

#[cfg(windows)]
pub use win::acquire;
