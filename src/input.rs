//! 输入注入：把 WebSocket 消息翻译成 Windows SendInput 事件。
//! 支持鼠标 / 滚轮 / 键盘 / 组合键 / Unicode 文本（中文与移动端软键盘）。
//!
//! ## 防止「远程端按键永久粘滞」
//! 浏览器在窗口失焦、焦点切到输入框、系统快捷键抢焦点、网络断开重连时，
//! 经常不会派发 keyup。因此本模块采用三层防护：
//! 1. 每个会话维护 `InputState`，记录本会话按下的所有键 / 修饰键 / 鼠标键；
//! 2. 客户端心跳上报当前真实按下的键集合，服务端做差集立即释放多余按键；
//! 3. `Drop` 时在连接结束（含异常断开、服务停止、重连）自动抬起全部按键，
//!    并附带看门狗兜底，避免任何一条路径遗漏导致 Ctrl 一直按着。

use crate::protocol::{ClientMessage, PressedKeys};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// 单个 WebSocket 会话的输入状态。每个连接独占一份，断开时自动清理。
#[derive(Default)]
pub struct InputState {
    /// 当前被认为处于按下状态的修饰键
    modifiers: HashSet<String>,
    /// 本会话按下、尚未确认松开的虚拟键（含修饰键），值为按下/最后确认时间
    keys_down: PressedKeys,
    /// 本会话按下、尚未确认松开的鼠标键编号
    buttons_down: HashSet<i32>,
}

impl InputState {
    pub fn new() -> Self {
        Self::default()
    }

    fn note_down(&mut self, vk: u16) {
        self.keys_down.insert(vk, Instant::now());
    }

    fn note_up(&mut self, vk: u16) {
        self.keys_down.remove(&vk);
    }

    /// 抬起本会话按下的**所有**键与鼠标键，清空状态。
    /// 对未真正按下的键发 KEYUP 是幂等且无害的，宁可多抬一次也绝不卡住。
    pub fn release_all(&mut self) {
        for vk in self.keys_down.keys() {
            imp::key_up(*vk);
        }
        self.keys_down.clear();
        for m in &self.modifiers {
            if let Some(vk) = modifier_vk(m) {
                imp::key_up(vk);
            }
        }
        self.modifiers.clear();
        for button in &self.buttons_down {
            imp::mouse_button(*button, false);
        }
        self.buttons_down.clear();
    }

    /// 看门狗：释放超过 `max_age` 未被任何心跳确认过的按键。
    /// 客户端持续按住时会随心跳刷新时间戳，因此不会误伤正常长按。
    pub fn release_stale(&mut self, max_age: Duration) {
        let stale: Vec<u16> = self
            .keys_down
            .iter()
            .filter(|(_, t)| t.elapsed() > max_age)
            .map(|(vk, _)| *vk)
            .collect();
        for vk in stale {
            imp::key_up(vk);
            self.keys_down.remove(&vk);
            if let Some(name) = modifier_name(vk) {
                self.modifiers.remove(name);
            }
        }
    }

    /// 心跳对账：以客户端上报的「真实按下集合」为准，释放服务端多出来的键。
    pub fn sync_pressed(&mut self, msg: &ClientMessage) {
        let now = Instant::now();
        let mut desired = HashSet::new();
        for k in &msg.keys {
            if let Some(vk) = resolve_vk(&k.key, &k.code) {
                desired.insert(vk);
            }
        }

        // 客户端已松开、但服务端仍认为按着 → 立刻抬起
        let stale: Vec<u16> = self
            .keys_down
            .keys()
            .filter(|vk| !desired.contains(vk))
            .copied()
            .collect();
        for vk in stale {
            imp::key_up(vk);
            self.keys_down.remove(&vk);
        }
        // 客户端仍按着 → 刷新时间戳（若 keydown 曾丢包则补按下）
        for vk in desired {
            if !self.keys_down.contains_key(&vk) {
                imp::key_down(vk);
            }
            self.keys_down.insert(vk, now);
        }

        // 鼠标键同理：只释放客户端已确认松开的按键
        let desired_buttons: HashSet<i32> = msg.buttons.iter().copied().collect();
        let stale_buttons: Vec<i32> = self
            .buttons_down
            .iter()
            .filter(|b| !desired_buttons.contains(b))
            .copied()
            .collect();
        for b in stale_buttons {
            imp::mouse_button(b, false);
            self.buttons_down.remove(&b);
        }

        // 修饰键语义与按键快照对齐（双保险）
        let mods: Vec<String> = msg
            .keys
            .iter()
            .filter_map(|k| modifier_key_name(&k.key).map(|s| s.to_string()))
            .collect();
        sync_modifiers(self, &mods);
    }
}

/// 连接结束（正常关闭 / 网络断开 / 服务停止 / panic）时兜底释放所有按键。
impl Drop for InputState {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// 处理一条客户端输入消息；调用方需保证会话已认证。
pub fn handle(msg: &ClientMessage, state: &mut InputState) {
    if msg.is_mouse() {
        handle_mouse(msg, state);
        return;
    }
    if !msg.is_keyboard() {
        return;
    }
    match msg.kind.as_str() {
        "type" | "paste" | "text" => imp::type_text(&msg.text),
        "combo" => handle_combo(msg),
        "release_all" => {
            state.release_all();
            log_debug_released();
        }
        "keydown" => {
            sync_modifiers(state, &msg.modifiers);
            if let Some(vk) = resolve_vk(&msg.key, &msg.code) {
                imp::key_down(vk);
                state.note_down(vk);
            } else if is_printable_char(&msg.key) {
                imp::type_text(&msg.key);
            }
        }
        "keyup" => {
            // 无条件抬起：不能再依赖 keys_down / modifiers 判断，
            // 否则 keyup 晚到或状态不同步时会漏抬，导致远程 Ctrl 永久粘滞。
            if let Some(vk) = resolve_vk(&msg.key, &msg.code) {
                imp::key_up(vk);
                state.note_up(vk);
            }
            if let Some(name) = modifier_key_name(&msg.key) {
                state.modifiers.remove(name);
            }
            sync_modifiers(state, &msg.modifiers);
        }
        _ => {}
    }
}

fn log_debug_released() {
    crate::log_debug!("客户端请求释放全部按键");
}

fn handle_combo(msg: &ClientMessage) {
    let mods = normalized_modifiers(&msg.modifiers);
    let vks: Vec<u16> = mods.iter().filter_map(|m| modifier_vk(m)).collect();
    for vk in &vks {
        imp::key_down(*vk);
    }
    if let Some(vk) = resolve_vk(&msg.key, &msg.code) {
        imp::key_down(vk);
        imp::key_up(vk);
    } else if is_printable_char(&msg.key) {
        imp::type_text(&msg.key);
    }
    for vk in vks.iter().rev() {
        imp::key_up(*vk);
    }
}

fn handle_mouse(msg: &ClientMessage, state: &mut InputState) {
    let x = msg.x.round() as i32;
    let y = msg.y.round() as i32;
    match msg.kind.as_str() {
        "mousemove" => imp::mouse_move(x, y),
        "mousedown" => {
            // 按键前先对齐坐标：即使之前的 mousemove 丢失/被合并，
            // 也不会把点击落在旧位置（旧 Python 版同样在按下前定位光标）。
            imp::mouse_move(x, y);
            imp::mouse_button(msg.button, true);
            state.buttons_down.insert(msg.button);
        }
        "mouseup" => {
            imp::mouse_move(x, y);
            // 无条件抬起，避免漏抬导致远程鼠标左键一直按着
            imp::mouse_button(msg.button, false);
            state.buttons_down.remove(&msg.button);
        }
        "dblclick" => {
            imp::mouse_move(x, y);
            imp::mouse_button(msg.button, true);
            imp::mouse_button(msg.button, false);
            imp::mouse_button(msg.button, true);
            imp::mouse_button(msg.button, false);
            state.buttons_down.remove(&msg.button);
        }
        "wheel" => imp::mouse_wheel(msg.delta_x, msg.delta_y),
        _ => {}
    }
}

fn sync_modifiers(state: &mut InputState, target: &[String]) {
    let desired = normalized_modifiers(target);
    let current = state.modifiers.clone();
    for m in &current {
        if !desired.contains(m) {
            if let Some(vk) = modifier_vk(m) {
                imp::key_up(vk);
                state.note_up(vk);
            }
            state.modifiers.remove(m);
        }
    }
    for m in desired {
        if !state.modifiers.contains(&m) {
            if let Some(vk) = modifier_vk(&m) {
                imp::key_down(vk);
                state.note_down(vk);
            }
            state.modifiers.insert(m);
        }
    }
}

fn normalized_modifiers(mods: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for m in mods {
        if let Some(key) = modifier_key_name(m) {
            let key = key.to_string();
            if !out.contains(&key) {
                out.push(key);
            }
        }
    }
    out
}

/// JS `KeyboardEvent.key` / 内部修饰键名 → 规范化名称。
fn modifier_key_name(s: &str) -> Option<&'static str> {
    match s.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "controlleft" | "controlright" => Some("ctrl"),
        "alt" | "option" | "altleft" | "altright" | "altgr" => Some("alt"),
        "shift" | "shiftleft" | "shiftright" => Some("shift"),
        "cmd" | "meta" | "win" | "super" | "os" | "metaleft" | "metaright" => Some("cmd"),
        _ => None,
    }
}

/// 虚拟键码反查修饰键名，供看门狗清理。
fn modifier_name(vk: u16) -> Option<&'static str> {
    for name in ["ctrl", "alt", "shift", "cmd"] {
        if modifier_vk(name) == Some(vk) {
            return Some(name);
        }
    }
    None
}

fn modifier_vk(m: &str) -> Option<u16> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT,
        };
        Some(match m {
            "ctrl" => VK_CONTROL,
            "alt" => VK_MENU,
            "shift" => VK_SHIFT,
            "cmd" => VK_LWIN,
            _ => return None,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = m;
        None
    }
}

fn is_printable_char(s: &str) -> bool {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => !c.is_control(),
        _ => false,
    }
}

/// JS `KeyboardEvent.key` / `code` → Windows 虚拟键码。
/// 仅映射 ASCII 字母数字与功能键；其余可打印字符交给 Unicode 注入。
fn resolve_vk(key: &str, code: &str) -> Option<u16> {
    #[cfg(not(windows))]
    {
        let _ = (key, code);
        return None;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
        let vk = match key {
            "Backspace" => VK_BACK,
            "Tab" => VK_TAB,
            "Enter" => VK_RETURN,
            "Escape" | "Esc" => VK_ESCAPE,
            "Delete" => VK_DELETE,
            "Insert" => VK_INSERT,
            "Home" => VK_HOME,
            "End" => VK_END,
            "PageUp" => VK_PRIOR,
            "PageDown" => VK_NEXT,
            "ArrowLeft" => VK_LEFT,
            "ArrowRight" => VK_RIGHT,
            "ArrowUp" => VK_UP,
            "ArrowDown" => VK_DOWN,
            "Control" => VK_CONTROL,
            "Alt" => VK_MENU,
            "Shift" => VK_SHIFT,
            "Meta" | "OS" => VK_LWIN,
            "CapsLock" => VK_CAPITAL,
            " " | "Space" | "Spacebar" => VK_SPACE,
            "PrintScreen" => VK_SNAPSHOT,
            "F1" => VK_F1,
            "F2" => VK_F2,
            "F3" => VK_F3,
            "F4" => VK_F4,
            "F5" => VK_F5,
            "F6" => VK_F6,
            "F7" => VK_F7,
            "F8" => VK_F8,
            "F9" => VK_F9,
            "F10" => VK_F10,
            "F11" => VK_F11,
            "F12" => VK_F12,
            _ => {
                let c = key.chars().next()?;
                if c.is_ascii_alphanumeric() && key.chars().count() == 1 {
                    c.to_ascii_uppercase() as u16
                } else if let Some(n) = fkey_from_code(code) {
                    return Some(n);
                } else {
                    return None;
                }
            }
        };
        Some(vk)
    }
}

#[cfg(windows)]
fn fkey_from_code(code: &str) -> Option<u16> {
    let rest = code.strip_prefix('F')?;
    let n: u16 = rest.parse().ok()?;
    if (1..=24).contains(&n) {
        Some(0x6f + n) // VK_F1 = 0x70
    } else {
        None
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Windows SendInput 实现
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
mod imp {
    use crate::capture;
    use std::mem::size_of;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;

    fn send(inputs: &[INPUT]) {
        if inputs.is_empty() {
            return;
        }
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        };
        // 被 UIPI 拦截（目标窗口以管理员权限运行）或被安全软件禁止时，
        // SendInput 会静默返回 0；只提示一次，避免刷屏。
        if sent == 0 {
            static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                crate::log_warn!(
                    "SendInput 注入失败: {}（若目标程序以管理员权限运行，请以管理员身份运行 WebDesk）",
                    std::io::Error::last_os_error()
                );
            }
        }
    }

    fn mouse(flags: u32, data: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    pub fn mouse_move(x: i32, y: i32) {
        let (ox, oy) = capture::virtual_screen_origin();
        let ok = unsafe { SetCursorPos(x.saturating_add(ox), y.saturating_add(oy)) };
        if ok == 0 {
            crate::log_debug!(
                "SetCursorPos 失败: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    pub fn mouse_button(button: i32, down: bool) {
        let flags = match (button, down) {
            (0, true) => MOUSEEVENTF_LEFTDOWN,
            (0, false) => MOUSEEVENTF_LEFTUP,
            (1, true) => MOUSEEVENTF_MIDDLEDOWN,
            (1, false) => MOUSEEVENTF_MIDDLEUP,
            (2, true) => MOUSEEVENTF_RIGHTDOWN,
            (2, false) => MOUSEEVENTF_RIGHTUP,
            (3, true) => MOUSEEVENTF_XDOWN,
            (3, false) => MOUSEEVENTF_XUP,
            (4, true) => MOUSEEVENTF_XDOWN,
            (4, false) => MOUSEEVENTF_XUP,
            _ => return,
        };
        let data = if button >= 3 { (button - 2) as u32 } else { 0 };
        send(&[mouse(flags, data)]);
    }

    pub fn mouse_wheel(dx: f64, dy: f64) {
        const WHEEL_DELTA: i32 = 120;
        let mut batch = Vec::new();
        if dy.abs() >= 1.0 {
            let notches = (dy / 100.0).round().clamp(-10.0, 10.0) as i32;
            if notches != 0 {
                let data = (-notches * WHEEL_DELTA) as u32;
                batch.push(mouse(MOUSEEVENTF_WHEEL, data));
            }
        }
        if dx.abs() >= 1.0 {
            let notches = (dx / 100.0).round().clamp(-10.0, 10.0) as i32;
            if notches != 0 {
                batch.push(mouse(MOUSEEVENTF_HWHEEL, (notches * WHEEL_DELTA) as u32));
            }
        }
        send(&batch);
    }

    pub fn key_down(vk: u16) {
        send(&[key(vk, 0, 0)]);
    }

    pub fn key_up(vk: u16) {
        send(&[key(vk, 0, KEYEVENTF_KEYUP)]);
    }

    /// Unicode 文本注入：对中文、emoji、标点都有效（含 UTF-16 代理对）。
    pub fn type_text(text: &str) {
        let mut batch = Vec::new();
        let mut buf = [0u16; 2];
        for ch in text.chars().take(2000) {
            let units = ch.encode_utf16(&mut buf);
            for unit in units.iter() {
                batch.push(key(0, *unit, KEYEVENTF_UNICODE));
                batch.push(key(0, *unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
            if batch.len() >= 128 {
                send(&batch);
                batch.clear();
            }
        }
        send(&batch);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 非 Windows 降级实现
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(not(windows))]
mod imp {
    pub fn mouse_move(_x: i32, _y: i32) {}
    pub fn mouse_button(_button: i32, _down: bool) {}
    pub fn mouse_wheel(_dx: f64, _dy: f64) {}
    pub fn key_down(_vk: u16) {}
    pub fn key_up(_vk: u16) {}
    pub fn type_text(_text: &str) {}
}
