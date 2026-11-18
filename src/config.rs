//! 配置读写、密码哈希、开机自启、本机访问地址。
//!
//! 刻意保持与旧 Python 版 `config.ini` 完全兼容（同名键、true/false 字符串、
//! `pbkdf2_sha256$迭代次数$salt$hash` 格式），方便直接升级而无需重新配置。

use std::fs;
use std::io::{self, Write};
use std::net::{ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};

use base64::Engine;
use rand::RngCore;

pub const APP_NAME: &str = "WebDesk";
pub const CONFIG_FILENAME: &str = "config.ini";
pub const LOG_FILENAME: &str = "webdesk.log";
pub const PBKDF2_ITERATIONS: u32 = 120_000;
const HASH_PREFIX: &str = "pbkdf2_sha256";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

// ──────────────────────────────────────────────────────────────────────────────
// 配置结构
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    /// PBKDF2 哈希；空字符串表示未设置密码
    pub password_hash: String,
    /// 兼容旧版明文密码（读取旧配置后建议清空）
    pub password_plain: String,
    pub fps: u32,
    pub tile_size: u32,
    pub quality: u8,
    pub autostart: bool,
    pub minimize_to_tray: bool,
    pub start_minimized: bool,
    pub open_browser: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 8765,
            password_hash: String::new(),
            password_plain: String::new(),
            fps: 15,
            tile_size: 64,
            quality: 75,
            autostart: false,
            minimize_to_tray: true,
            start_minimized: false,
            open_browser: false,
        }
    }
}

impl Config {
    pub fn auth_required(&self) -> bool {
        !self.password_hash.is_empty() || !self.password_plain.is_empty()
    }

    /// 这些键改动需要重启监听；其余（密码等）可热更新。
    pub fn needs_restart(&self, other: &Config) -> bool {
        self.port != other.port
            || self.fps != other.fps
            || self.tile_size != other.tile_size
            || self.quality != other.quality
    }

    pub fn clamped(mut self) -> Self {
        self.fps = self.fps.clamp(1, 60);
        self.tile_size = self.tile_size.clamp(16, 256);
        self.quality = self.quality.clamp(10, 95);
        self
    }

    /// 从 ini 文本解析（忽略注释与未知键，缺失项用默认值补齐）。
    pub fn from_ini(text: &str) -> Self {
        let mut c = Config::default();
        let mut section = String::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_ascii_lowercase();
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            match (section.as_str(), key.as_str()) {
                ("server", "port") => c.port = val.parse::<u16>().unwrap_or(c.port),
                ("server", "password_hash") => c.password_hash = val,
                ("server", "password") => c.password_plain = val,
                ("performance", "fps") => c.fps = val.parse().unwrap_or(c.fps),
                ("performance", "tile_size") => c.tile_size = val.parse().unwrap_or(c.tile_size),
                ("performance", "quality") => c.quality = val.parse().unwrap_or(c.quality),
                ("general", "autostart") => c.autostart = parse_bool(&val, c.autostart),
                ("general", "minimize_to_tray") => {
                    c.minimize_to_tray = parse_bool(&val, c.minimize_to_tray)
                }
                ("general", "start_minimized") => {
                    c.start_minimized = parse_bool(&val, c.start_minimized)
                }
                ("general", "open_browser") => c.open_browser = parse_bool(&val, c.open_browser),
                // "host" / "ipv4" / "ipv6" 为旧版遗留键：现在固定双栈监听，忽略即可
                _ => {}
            }
        }
        c.clamped()
    }

    pub fn load(path: Option<&Path>) -> Self {
        let p = path.map(Path::to_path_buf).unwrap_or_else(|| config_path(false));
        match fs::read_to_string(&p) {
            Ok(text) => Config::from_ini(&text),
            Err(_) => Config::default(),
        }
    }

    pub fn to_ini(&self) -> String {
        format!(
            "# WebDesk 配置文件 - 也可在图形界面中修改\n\
             [server]\n\
             # 监听端口 (1-65535)\n\
             port = {port}\n\
             # 访问密码哈希（PBKDF2-SHA256，留空表示不需要密码）\n\
             password_hash = {hash}\n\
             # 兼容旧版明文密码，建议清空并改用上面的哈希\n\
             password = {plain}\n\
             \n\
             [performance]\n\
             fps = {fps}\n\
             tile_size = {tile}\n\
             quality = {quality}\n\
             \n\
             [general]\n\
             # 开机自动启动\n\
             autostart = {autostart}\n\
             # 关闭窗口时最小化到系统托盘\n\
             minimize_to_tray = {tray}\n\
             # 启动后直接最小化到托盘\n\
             start_minimized = {startmin}\n\
             # 服务启动后自动打开浏览器\n\
             open_browser = {browser}\n",
            port = self.port,
            hash = self.password_hash,
            plain = self.password_plain,
            fps = self.fps,
            tile = self.tile_size,
            quality = self.quality,
            autostart = b(self.autostart),
            tray = b(self.minimize_to_tray),
            startmin = b(self.start_minimized),
            browser = b(self.open_browser),
        )
    }

    /// 原子写入；程序目录只读时自动回退到用户数据目录。
    pub fn save(&self, path: Option<&Path>) -> io::Result<PathBuf> {
        let text = self.to_ini();
        let target = path.map(Path::to_path_buf).unwrap_or_else(|| config_path(true));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        match write_atomic(&target, &text) {
            Ok(()) => Ok(target),
            Err(e) => {
                let fallback = user_data_dir().join(CONFIG_FILENAME);
                if fallback == target {
                    return Err(e);
                }
                fs::create_dir_all(fallback.parent().unwrap_or_else(|| Path::new(".")))?;
                write_atomic(&fallback, &text)?;
                Ok(fallback)
            }
        }
    }
}

fn write_atomic(target: &Path, text: &str) -> io::Result<()> {
    let tmp = target.with_extension("ini.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
    }
    fs::rename(&tmp, target)
}

fn b(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}

fn parse_bool(s: &str, default: bool) -> bool {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 路径
// ──────────────────────────────────────────────────────────────────────────────

pub fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn user_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())))
            .join(APP_NAME)
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join("Library/Application Support")
            .join(APP_NAME)
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            })
            .join("webdesk")
    }
}

/// 用户目录已有配置优先；否则用程序目录（绿色版）；写入时目录不可写则回退。
pub fn config_path(for_write: bool) -> PathBuf {
    let user_path = user_data_dir().join(CONFIG_FILENAME);
    let local_path = app_dir().join(CONFIG_FILENAME);
    if user_path.exists() {
        return user_path;
    }
    if local_path.exists() {
        if !for_write || dir_writable(&app_dir()) {
            return local_path;
        }
        return user_path;
    }
    if dir_writable(&app_dir()) {
        local_path
    } else {
        user_path
    }
}

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(".webdesk_write_probe");
    match fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn log_path() -> PathBuf {
    user_data_dir().join(LOG_FILENAME)
}

// ──────────────────────────────────────────────────────────────────────────────
// 密码：PBKDF2-SHA256 + 随机盐，格式与 Python 版一致
// ──────────────────────────────────────────────────────────────────────────────

pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let digest = derive(password.as_bytes(), &salt, PBKDF2_ITERATIONS);
    format!(
        "{HASH_PREFIX}${}${}${}",
        PBKDF2_ITERATIONS,
        B64.encode(salt),
        B64.encode(digest)
    )
}

/// 校验密码；支持哈希与旧版明文，比较过程为常量时间。
pub fn verify_password(candidate: &str, stored_hash: &str, legacy_plain: &str) -> bool {
    if let Some(rest) = stored_hash.strip_prefix(&format!("{HASH_PREFIX}$")) {
        let parts: Vec<&str> = rest.split('$').collect();
        if parts.len() == 3 {
            if let (Ok(iterations), Ok(salt), Ok(expected)) = (
                parts[0].parse::<u32>(),
                B64.decode(parts[1]),
                B64.decode(parts[2]),
            ) {
                if !(1..=10_000_000).contains(&iterations) || expected.is_empty() {
                    return false;
                }
                let actual = derive(candidate.as_bytes(), &salt, iterations);
                return constant_time_eq(&actual, &expected);
            }
        }
        return false;
    }
    if !legacy_plain.is_empty() {
        return constant_time_eq(candidate.as_bytes(), legacy_plain.as_bytes());
    }
    false
}

fn derive(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, iterations, &mut out);
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ──────────────────────────────────────────────────────────────────────────────
// 开机自启
// ──────────────────────────────────────────────────────────────────────────────

const WIN_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn autostart_command() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("webdesk"));
    format!("\"{}\" --minimized", exe.display())
}

#[cfg(windows)]
pub fn autostart_enabled() -> bool {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey_with_flags(WIN_RUN_KEY, KEY_READ)
        .ok()
        .and_then(|k| k.get_value::<String, _>(APP_NAME).ok())
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn set_autostart(enabled: bool) -> Result<String, String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(WIN_RUN_KEY, KEY_READ | KEY_SET_VALUE)
        .map_err(|e| format!("注册表操作失败: {e}"))?;
    if enabled {
        let cmd = autostart_command();
        key.set_value(APP_NAME, &cmd)
            .map_err(|e| format!("写入启动项失败: {e}"))?;
        Ok("已写入 Windows 当前用户启动项（免管理员权限）".into())
    } else {
        match key.delete_value(APP_NAME) {
            Ok(()) => Ok("已移除 Windows 启动项".into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Ok("启动项原本就不存在，无需修改".into())
            }
            Err(e) => Err(format!("删除启动项失败: {e}")),
        }
    }
}

#[cfg(target_os = "macos")]
fn autostart_file() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join("Library/LaunchAgents/cn.webdesk.server.plist")
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn autostart_file() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
        });
    base.join("autostart/webdesk.desktop")
}

#[cfg(not(windows))]
pub fn autostart_enabled() -> bool {
    autostart_file().exists()
}

#[cfg(not(windows))]
pub fn set_autostart(enabled: bool) -> Result<String, String> {
    let path = autostart_file();
    if !enabled {
        if path.exists() {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        return Ok("已移除开机自启项".into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = if cfg!(target_os = "macos") {
        let exe = std::env::current_exe().unwrap_or_default();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n\
             <key>Label</key><string>cn.webdesk.server</string>\n\
             <key>ProgramArguments</key><array><string>{}</string><string>--minimized</string></array>\n\
             <key>RunAtLoad</key><true/>\n</dict></plist>\n",
            exe.display()
        )
    } else {
        format!(
            "[Desktop Entry]\nType=Application\nName=WebDesk\nExec={}\n\
             X-GNOME-Autostart-enabled=true\nNoDisplay=false\n",
            autostart_command()
        )
    };
    fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(format!("已写入 {}", path.display()))
}

// ──────────────────────────────────────────────────────────────────────────────
// 本机访问地址
// ──────────────────────────────────────────────────────────────────────────────

fn outbound_ip(family_v6: bool) -> Option<String> {
    let sock = if family_v6 {
        UdpSocket::bind("[::]:0").ok()?
    } else {
        UdpSocket::bind("0.0.0.0:0").ok()?
    };
    let peer = if family_v6 {
        "[2001:4860:4860::8888]:80"
    } else {
        "8.8.8.8:80"
    };
    // UDP connect 只选择出口路由，不会真正发包
    sock.connect(peer).ok()?;
    let mut addr = sock.local_addr().ok()?.ip().to_string();
    if let Some((ip, _scope)) = addr.split_once('%') {
        addr = ip.to_string();
    }
    Some(addr)
}

/// HTTP 访问地址列表（IPv6 使用方括号），供 GUI 展示与复制。
pub fn local_addresses(port: u16) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    let push = |addr: String, urls: &mut Vec<String>| {
        if urls.len() >= 12 {
            return;
        }
        let host = if addr.contains(':') {
            format!("[{addr}]")
        } else {
            addr.clone()
        };
        let url = format!("http://{host}:{port}/");
        if !urls.contains(&url) {
            urls.push(url);
        }
    };

    push("127.0.0.1".into(), &mut urls);
    push("::1".into(), &mut urls);

    let mut candidates: Vec<String> = Vec::new();
    let hostname = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    if !hostname.is_empty() {
        if let Ok(iter) = (hostname.as_str(), 0).to_socket_addrs() {
            for sa in iter {
                let ip = sa.ip().to_string().split('%').next().unwrap_or("").to_string();
                candidates.push(ip);
            }
        }
    }
    if let Some(ip) = outbound_ip(false) {
        candidates.push(ip);
    }
    if let Some(ip) = outbound_ip(true) {
        candidates.push(ip);
    }
    for addr in candidates {
        if addr.starts_with("127.") || addr == "::1" || addr == "0.0.0.0" || addr == "::" {
            continue;
        }
        push(addr, &mut urls);
    }
    urls
}

/// 打开 URL 或目录（Windows 用 start，Linux 用 xdg-open，macOS 用 open）。
pub fn open_with_system(target: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", target])
            .spawn()
            .map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(target)
            .spawn()
            .map(|_| ())
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(target)
            .spawn()
            .map(|_| ())
    }
}
