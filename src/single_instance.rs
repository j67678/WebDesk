//! 单实例模式：重复启动时唤起已运行实例的主窗口，避免重复监听端口。
//!
//! 通过命名互斥体判断本进程是否为唯一实例；若不是，则不再启动第二份服务，
//! 而是把已运行实例的主窗口显示到前台后退出。

/// `Local\` 前缀表示互斥体仅在当前登录会话内唯一，
/// 同一台机器上不同用户可各自运行一份。
#[cfg(windows)]
const MUTEX_NAME: &str = "Local\\WebDesk-SingleInstance";

/// 等待已运行实例创建窗口的最长时间（首个实例可能还在初始化界面）
#[cfg(windows)]
const ACTIVATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(windows)]
mod imp {
    use std::ptr;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND};
    use windows_sys::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow, ShowWindow,
        SW_RESTORE, SW_SHOW,
    };

    /// 持有互斥体句柄，进程存活期间保证自己是唯一实例。
    pub struct InstanceGuard(HANDLE);

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 成为唯一实例；返回 `None` 表示已有实例在运行。
    pub fn acquire() -> Option<InstanceGuard> {
        let name = wide(super::MUTEX_NAME);
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        // 必须紧接着 CreateMutexW 读取 GetLastError
        let already_running = !handle.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already_running {
            unsafe { CloseHandle(handle) };
            return None;
        }
        // 创建失败时返回空句柄的 guard，不阻止启动，由端口占用情况兜底提示
        Some(InstanceGuard(handle))
    }

    /// 把已运行实例的主窗口显示到前台。
    /// 对方为无界面模式（没有窗口）时等待超时后返回 `false`。
    pub fn activate_existing_window() -> bool {
        let deadline = Instant::now() + super::ACTIVATE_TIMEOUT;
        loop {
            if show_main_window() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// 立即把主窗口显示、还原并置于前台；找不到窗口（或尚未创建）时返回 `false`。
    /// 托盘线程也用它来恢复隐藏到托盘的窗口。
    pub fn show_main_window() -> bool {
        let title = wide(crate::gui::WINDOW_TITLE);
        let hwnd = unsafe { FindWindowW(ptr::null(), title.as_ptr()) };
        if hwnd.is_null() {
            return false;
        }
        unsafe {
            // 最小化到托盘的窗口是隐藏状态，先 SW_SHOW 再 SW_RESTORE
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_RESTORE);
            bring_to_front(hwnd);
        }
        true
    }

    /// `SetForegroundWindow` 受前台锁定限制，附加到当前前台线程的输入队列后更可靠。
    unsafe fn bring_to_front(hwnd: HWND) {
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, ptr::null_mut())
        };
        let current_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(current_thread, foreground_thread, 1) != 0;
        SetForegroundWindow(hwnd);
        if attached {
            AttachThreadInput(current_thread, foreground_thread, 0);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// 非 Windows 平台暂不实现单实例约束。
    pub struct InstanceGuard;

    pub fn acquire() -> Option<InstanceGuard> {
        Some(InstanceGuard)
    }

    pub fn activate_existing_window() -> bool {
        false
    }

    pub fn show_main_window() -> bool {
        false
    }
}

pub use imp::{acquire, activate_existing_window, show_main_window};
