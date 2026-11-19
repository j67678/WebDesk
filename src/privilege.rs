//! 权限提升：以管理员完整性级别运行，才能向提权窗口注入输入。
//!
//! Windows 的 UIPI（User Interface Privilege Isolation）会阻止较低完整性级别的
//! 进程向较高完整性级别的窗口发送输入。任务管理器等程序默认以管理员权限运行，
//! 因此普通权限下的 WebDesk 虽然能截图，却无法用 SendInput 操作它们。
//! 启动时自查令牌；若未提权，则通过 ShellExecuteW("runas") 重启自身。

/// 当前进程是否具备管理员完整性级别。
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION {
            TokenIsElevated: 0,
        };
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut std::ffi::c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// 以管理员权限重新拉起当前可执行文件，并原样传递命令行参数。
/// 成功返回表示子进程已创建，调用方应退出当前进程。
#[cfg(windows)]
pub fn relaunch_as_admin() -> std::io::Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    let exe = std::env::current_exe()?;
    let params = std::env::args()
        .skip(1)
        .map(|arg| {
            if arg.contains(' ') || arg.contains('\t') || arg.contains('"') {
                format!("\"{}\"", arg.replace('"', "\\\""))
            } else {
                arg
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    let exe_w = wide(exe.as_os_str());
    let verb_w = wide(OsStr::new("runas"));
    let params_w = wide(OsStr::new(&params));

    let ret = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb_w.as_ptr(),
            exe_w.as_ptr(),
            params_w.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (ret as usize) <= 32 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    true
}
