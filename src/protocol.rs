//! WebSocket 协议消息（与旧 Python 版及 Web 客户端保持兼容）。
//!
//! 除键鼠事件外，客户端会通过心跳上报「当前真实按下的键 / 鼠标键」集合，
//! 服务端据此做差集释放，避免 keyup 丢失导致远程端按键永久粘滞。

use serde::Deserialize;
use std::collections::HashMap;
use std::time::Instant;

/// 客户端上报的单个「仍处于按下状态」的键。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct KeyState {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub code: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientMessage {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub button: i32,
    #[serde(default, alias = "deltaX")]
    pub delta_x: f64,
    #[serde(default, alias = "deltaY")]
    pub delta_y: f64,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub password: String,
    /// 移动端虚拟键盘 / 组合键携带的修饰键
    #[serde(default)]
    pub modifiers: Vec<String>,
    /// 心跳对账：客户端此刻仍按下的键（含修饰键）
    #[serde(default)]
    pub keys: Vec<KeyState>,
    /// 心跳对账：客户端此刻仍按下的鼠标键编号
    #[serde(default)]
    pub buttons: Vec<i32>,
    /// WebRTC offer SDP（type=offer）
    #[serde(default)]
    pub sdp: String,
    /// 弱网自适应：客户端建议的目标码率（kbps，type=adapt）
    #[serde(default)]
    pub bitrate_kbps: u32,
}

impl ClientMessage {
    pub fn is_mouse(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "mousemove" | "mousedown" | "mouseup" | "wheel" | "dblclick"
        )
    }

    pub fn is_keyboard(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "keydown" | "keyup" | "type" | "paste" | "text" | "combo" | "release_all"
        )
    }
}

/// 认证失败限制：连续 8 次错误断开，每次指数退避（上限 6 秒）。
pub const MAX_AUTH_FAILURES: u32 = 8;

pub fn auth_backoff_ms(failures: u32) -> u64 {
    let shifted = 400u64.saturating_mul(1u64 << failures.min(4));
    shifted.min(6_000)
}

/// 服务端认为「客户端仍按住」的虚拟键，附带最后一次确认时间。
pub type PressedKeys = HashMap<u16, Instant>;
