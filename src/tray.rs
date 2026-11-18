//! 系统托盘：Windows 使用原生 Shell_NotifyIcon（由 tray-icon 提供），
//! 完全不需要 WebView2 / 浏览器运行时。
//!
//! 托盘事件由独立线程轮询，而不是在 GUI 的 `update()` 里轮询：
//! 窗口隐藏到托盘后 eframe 收不到重绘事件，`update()` 会暂停，
//! 此时放在界面里的轮询将导致托盘菜单（尤其“显示主界面”）完全无响应。

use std::sync::atomic::{AtomicBool, Ordering};

/// 托盘“退出 WebDesk”被点击后置位，由 GUI 在恢复运行的那一帧完成优雅退出。
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 取出并清除“退出”请求。
pub fn take_quit_request() -> bool {
    QUIT_REQUESTED.swap(false, Ordering::SeqCst)
}

/// 生成 32×32 RGBA 图标：蓝色圆角底 + 白色显示器。
pub fn icon_rgba() -> Vec<u8> {
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let i = (y * size + x) * 4;
            let round = (x < 3 && y < 3 && (x + y) < 3)
                || (x >= size - 3 && y < 3 && (size - 1 - x + y) < 3)
                || (x < 3 && y >= size - 3 && (x + size - 1 - y) < 3)
                || (x >= size - 3 && y >= size - 3 && ((size - 1 - x) + (size - 1 - y)) < 3);
            if round {
                continue;
            }
            let in_screen = (6..26).contains(&x) && (7..23).contains(&y);
            let in_stand = (14..18).contains(&x) && (23..27).contains(&y);
            let in_base = (10..22).contains(&x) && (27..29).contains(&y);
            if in_screen {
                // 屏幕内边框留 1px 蓝底
                let inner = (7..25).contains(&x) && (8..22).contains(&y);
                if inner {
                    rgba[i] = 235;
                    rgba[i + 1] = 243;
                    rgba[i + 2] = 255;
                } else {
                    rgba[i] = 255;
                    rgba[i + 1] = 255;
                    rgba[i + 2] = 255;
                }
            } else if in_stand || in_base {
                rgba[i] = 255;
                rgba[i + 1] = 255;
                rgba[i + 2] = 255;
            } else {
                rgba[i] = 31;
                rgba[i + 1] = 111;
                rgba[i + 2] = 235;
            }
            rgba[i + 3] = 255;
        }
    }
    rgba
}

#[cfg(any(windows, target_os = "macos"))]
mod real {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use eframe::egui::{Context, ViewportCommand};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::{icon_rgba, QUIT_REQUESTED};
    use crate::{log_info, log_warn};

    pub struct Tray {
        _icon: TrayIcon,
        _menu: Menu,
    }

    impl Tray {
        /// `ctx` 用于通过 egui 命令恢复窗口。服务常驻运行，托盘只负责
        /// 唤出窗口与退出程序。
        pub fn new(ctx: Context) -> Option<Self> {
            let icon = Icon::from_rgba(icon_rgba(), 32, 32).ok()?;
            let menu = Menu::new();
            let show = MenuItem::new("显示主界面", true, None);
            let quit = MenuItem::new("退出 WebDesk", true, None);
            menu.append(&show).ok()?;
            menu.append(&PredefinedMenuItem::separator()).ok()?;
            menu.append(&quit).ok()?;

            let tray_icon = match TrayIconBuilder::new()
                .with_icon(icon)
                .with_tooltip("WebDesk · 远程桌面")
                .with_menu(Box::new(menu.clone()))
                .build()
            {
                Ok(t) => t,
                Err(e) => {
                    log_warn!("托盘图标创建失败（将退化为普通窗口）: {e}");
                    return None;
                }
            };

            let show_id = show.id().clone();
            let quit_id = quit.id().clone();
            let thread_ctx = ctx.clone();
            std::thread::Builder::new()
                .name("webdesk-tray".into())
                .spawn(move || loop {
                    while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                        let show_window = match ev {
                            TrayIconEvent::DoubleClick {
                                button: MouseButton::Left,
                                ..
                            } => true,
                            // macOS 没有 DoubleClick 事件，左键单击也可唤出窗口
                            TrayIconEvent::Click {
                                button: MouseButton::Left,
                                button_state: MouseButtonState::Up,
                                ..
                            } => cfg!(target_os = "macos"),
                            _ => false,
                        };
                        if show_window {
                            reveal(&thread_ctx);
                        }
                    }
                    while let Ok(ev) = MenuEvent::receiver().try_recv() {
                        if ev.id == show_id {
                            reveal(&thread_ctx);
                        } else if ev.id == quit_id {
                            log_info!("托盘菜单：退出 WebDesk");
                            QUIT_REQUESTED.store(true, Ordering::SeqCst);
                            // 隐藏的窗口收不到重绘事件，先恢复窗口让 GUI 主循环继续
                            reveal(&thread_ctx);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                })
                .ok();

            Some(Self {
                _icon: tray_icon,
                _menu: menu,
            })
        }
    }

    /// 把主窗口恢复显示并置于前台（隐藏或最小化状态都能还原）。
    fn reveal(ctx: &Context) {
        #[cfg(windows)]
        {
            if crate::single_instance::show_main_window() {
                return;
            }
        }
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

}

#[cfg(any(windows, target_os = "macos"))]
pub use real::Tray;

#[cfg(not(any(windows, target_os = "macos")))]
pub struct Tray;

#[cfg(not(any(windows, target_os = "macos")))]
impl Tray {
    pub fn new(_ctx: eframe::egui::Context) -> Option<Self> {
        None
    }
}
