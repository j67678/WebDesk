//! 系统托盘：Windows 使用原生 Shell_NotifyIcon（由 tray-icon 提供），
//! 完全不需要 WebView2 / 浏览器运行时。菜单事件通过全局 channel 由 GUI 轮询。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    OpenBrowser,
    Start,
    Stop,
    Quit,
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
    use super::{icon_rgba, TrayAction};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub struct Tray {
        _icon: TrayIcon,
        _menu: Menu,
        show: MenuItem,
        browser: MenuItem,
        start: MenuItem,
        stop: MenuItem,
        quit: MenuItem,
    }

    impl Tray {
        pub fn new() -> Option<Self> {
            let icon = Icon::from_rgba(icon_rgba(), 32, 32).ok()?;
            let menu = Menu::new();
            let show = MenuItem::new("显示主界面", true, None);
            let browser = MenuItem::new("打开浏览器", true, None);
            let start = MenuItem::new("启动服务", true, None);
            let stop = MenuItem::new("停止服务", true, None);
            let quit = MenuItem::new("退出 WebDesk", true, None);
            for item in [&show, &browser] {
                menu.append(item).ok()?;
            }
            menu.append(&PredefinedMenuItem::separator()).ok()?;
            menu.append(&start).ok()?;
            menu.append(&stop).ok()?;
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
                    crate::log_warn!("托盘图标创建失败（将退化为普通窗口）: {e}");
                    return None;
                }
            };
            Some(Self {
                _icon: tray_icon,
                _menu: menu,
                show,
                browser,
                start,
                stop,
                quit,
            })
        }

        pub fn poll(&self) -> Vec<TrayAction> {
            let mut out = Vec::new();
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                match ev {
                    TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    } => out.push(TrayAction::Show),
                    // macOS 没有 DoubleClick 事件，左键单击也可唤出窗口
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } if cfg!(target_os = "macos") => out.push(TrayAction::Show),
                    _ => {}
                }
            }
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if &ev.id == self.show.id() {
                    out.push(TrayAction::Show);
                } else if &ev.id == self.browser.id() {
                    out.push(TrayAction::OpenBrowser);
                } else if &ev.id == self.start.id() {
                    out.push(TrayAction::Start);
                } else if &ev.id == self.stop.id() {
                    out.push(TrayAction::Stop);
                } else if &ev.id == self.quit.id() {
                    out.push(TrayAction::Quit);
                }
            }
            out
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub use real::Tray;

#[cfg(not(any(windows, target_os = "macos")))]
pub struct Tray;

#[cfg(not(any(windows, target_os = "macos")))]
impl Tray {
    pub fn new() -> Option<Self> {
        None
    }
    pub fn poll(&self) -> Vec<TrayAction> {
        Vec::new()
    }
}
