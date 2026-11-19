//! HTTP + WebSocket 服务端：axum/tokio，IPv4 / IPv6 双独立监听。
//!
//! - 同一端口同时托管 Web 客户端页面与 WebSocket（`/ws`，根路径握手自动重写）
//! - 访问密码：PBKDF2 校验放 blocking 线程，失败指数退避，未认证不下发任何画面
//! - 服务随程序启动并常驻运行，配置变更时自动重启；密码支持热更新

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::{mpsc, watch, RwLock};
use webrtc::peer_connection::RTCPeerConnectionState;

use crate::capture::{virtual_screen_size, ScreenCapturer, SharedFrame};
use crate::config::{self, Config};
use crate::encoder::{EncodedFrame, VideoEncoder};
use crate::input::{self, InputState};
use crate::rtc;
use crate::log_debug;
use crate::log_error;
use crate::log_info;
use crate::log_warn;
use crate::protocol::{auth_backoff_ms, ClientMessage, MAX_AUTH_FAILURES};

/// 同时在线的 WebSocket 客户端上限。每个连接都独占一份全屏缓冲与截屏线程，
/// 不设上限时异常/恶意重连会把内存与线程耗尽。
const MAX_CLIENTS: usize = 6;
/// 客户端无任何消息的空闲上限（客户端心跳周期为 1 秒）
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// 单条消息写入超时：客户端不读 TCP 时不能让写任务永久挂起
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// 按键看门狗：超过该时长未被心跳确认的按键自动抬起
const KEY_STICKY_TIMEOUT: Duration = Duration::from_secs(20);
const WATCHDOG_TICK: Duration = Duration::from_secs(2);
/// 出站帧队列容量。有界队列 + 丢弃策略，避免慢客户端拖垮内存。
const OUT_QUEUE_CAP: usize = 8;

/// Mutex 被污染时不连锁 panic（否则一条连接的异常会毒死整个服务状态）。
fn lock_status(status: &Arc<Mutex<Status>>) -> std::sync::MutexGuard<'_, Status> {
    status.lock().unwrap_or_else(|e| e.into_inner())
}

// ──────────────────────────────────────────────────────────────────────────────
// 对外状态与控制
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateKind {
    Stopped,
    Starting,
    Running,
    Error,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub kind: StateKind,
    pub message: String,
    pub urls: Vec<String>,
    pub port: u16,
    pub clients: usize,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            kind: StateKind::Stopped,
            message: "服务未启动".into(),
            urls: Vec::new(),
            port: 8765,
            clients: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub port: u16,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub password_hash: String,
    pub password_plain: String,
}

impl From<&Config> for ServerConfig {
    fn from(c: &Config) -> Self {
        Self {
            port: c.port,
            fps: c.fps,
            bitrate_kbps: c.bitrate_kbps,
            min_bitrate_kbps: c.min_bitrate_kbps,
            password_hash: c.password_hash.clone(),
            password_plain: c.password_plain.clone(),
        }
    }
}

#[derive(Clone, Default)]
struct Credentials {
    hash: String,
    plain: String,
}

impl Credentials {
    fn required(&self) -> bool {
        !self.hash.is_empty() || !self.plain.is_empty()
    }
}

enum Command {
    Start(ServerConfig),
    Stop,
    SetPassword { hash: String, plain: String },
    Shutdown,
}

/// 出站控制消息：画面已改走 WebRTC，WS 只承载 JSON 信令/输入/心跳。
enum OutMsg {
    Text(String),
}

/// GUI / headless 共用的服务控制器：内部独占一个 tokio runtime 线程。
pub struct ServerController {
    cmd_tx: mpsc::UnboundedSender<Command>,
    status: Arc<Mutex<Status>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ServerController {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let status_for_thread = status.clone();
        let thread = std::thread::Builder::new()
            .name("webdesk-server".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(3)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        log_error!("无法创建 tokio runtime: {e}");
                        set_error(&status_for_thread, format!("无法创建运行时: {e}"));
                        return;
                    }
                };
                rt.block_on(command_loop(cmd_rx, status_for_thread));
            })
            .expect("启动服务线程失败");
        Self {
            cmd_tx,
            status,
            thread: Mutex::new(Some(thread)),
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn start(&self, cfg: &Config) {
        let _ = self.cmd_tx.send(Command::Start(ServerConfig::from(cfg)));
    }

    pub fn restart(&self, cfg: &Config) {
        let _ = self.cmd_tx.send(Command::Stop);
        let _ = self.cmd_tx.send(Command::Start(ServerConfig::from(cfg)));
    }

    /// 修改访问密码：无需重启监听，立即对新认证生效。
    pub fn set_password(&self, hash: String, plain: String) {
        let _ = self.cmd_tx.send(Command::SetPassword { hash, plain });
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(handle) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }

}

impl Drop for ServerController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn set_error(status: &Arc<Mutex<Status>>, message: String) {
    let mut s = lock_status(status);
    s.kind = StateKind::Error;
    s.message = message;
    s.urls.clear();
}

// ──────────────────────────────────────────────────────────────────────────────
// 服务内部状态
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    html: Arc<String>,
    creds: Arc<RwLock<Credentials>>,
    port: u16,
    screen_width: u32,
    screen_height: u32,
    status: Arc<Mutex<Status>>,
    clients: Arc<AtomicUsize>,
    /// 已认证的观看者数量；为 0 时捕获/编码线程暂停
    viewers: Arc<AtomicUsize>,
    encoded_hub: Arc<EncodedHub>,
    encoder_control: Arc<Mutex<EncoderControl>>,
    min_bitrate_kbps: u32,
    frame_interval: Duration,
    shutdown: watch::Sender<bool>,
}

struct Running {
    shutdown: watch::Sender<bool>,
    stop_flag: Arc<AtomicBool>,
    clients: Arc<AtomicUsize>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    capture_thread: Option<std::thread::JoinHandle<()>>,
    encoder_thread: Option<std::thread::JoinHandle<()>>,
}

async fn command_loop(mut rx: mpsc::UnboundedReceiver<Command>, status: Arc<Mutex<Status>>) {
    let creds = Arc::new(RwLock::new(Credentials::default()));
    let mut running: Option<Running> = None;

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Start(cfg) => {
                stop_running(&mut running).await;
                // 未设置密码时服务仍可启动，但客户端一律拒绝连接
                if cfg.password_hash.is_empty() && cfg.password_plain.is_empty() {
                    log_warn!("未设置访问密码：客户端将无法连接，请在界面中设置密码");
                }
                {
                    let mut c = creds.write().await;
                    c.hash = cfg.password_hash.clone();
                    c.plain = cfg.password_plain.clone();
                }
                set_status(
                    &status,
                    StateKind::Starting,
                    format!("正在启动服务（端口 {}）…", cfg.port),
                    cfg.port,
                    vec![],
                );
                match start_server(&cfg, creds.clone(), status.clone()).await {
                    Ok(r) => {
                        // 服务已就绪：把程序本身加入防火墙例外，避免局域网客户端被拦截
                        crate::firewall::ensure_exception();
                        let urls = config::local_addresses(cfg.port);
                        log_info!(
                            "WebDesk 已启动 | 端口 {} | FPS={} | 密码={}",
                            cfg.port,
                            cfg.fps,
                            if creds.read().await.required() { "启用" } else { "关闭" }
                        );
                        for url in &urls {
                            log_info!("   访问地址: {url}");
                        }
                        set_status(
                            &status,
                            StateKind::Running,
                            format!("端口 {} 正在监听", cfg.port),
                            cfg.port,
                            urls,
                        );
                        running = Some(r);
                    }
                    Err(e) => {
                        log_error!("服务启动失败: {e}");
                        set_error(&status, format!("启动失败: {e}"));
                    }
                }
            }
            Command::Stop => {
                stop_running(&mut running).await;
                set_status(&status, StateKind::Stopped, "服务已停止".into(), 0, vec![]);
                log_info!("服务已停止");
            }
            Command::SetPassword { hash, plain } => {
                let enabled = !hash.is_empty() || !plain.is_empty();
                *creds.write().await = Credentials { hash, plain };
                log_info!("访问密码已更新，当前状态: {}", if enabled { "启用" } else { "关闭" });
            }
            Command::Shutdown => {
                stop_running(&mut running).await;
                break;
            }
        }
    }
}

fn set_status(
    status: &Arc<Mutex<Status>>,
    kind: StateKind,
    message: String,
    port: u16,
    urls: Vec<String>,
) {
    let mut s = lock_status(status);
    s.kind = kind;
    s.message = message;
    if port > 0 {
        s.port = port;
    }
    s.urls = urls;
}

async fn stop_running(running: &mut Option<Running>) {
    let Some(r) = running.take() else { return };
    r.stop_flag.store(true, Ordering::Relaxed);
    let _ = r.shutdown.send(true);
    // 先让在途 WebSocket 会话自行收尾（释放按键 / 关闭帧），最多等 600ms
    for _ in 0..12 {
        if r.clients.load(Ordering::Acquire) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for task in r.tasks {
        task.abort();
    }
    if let Some(handle) = r.capture_thread {
        let _ = handle.join();
    }
    if let Some(handle) = r.encoder_thread {
        let _ = handle.join();
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 共享捕获：单生产者 / 多客户端
// ──────────────────────────────────────────────────────────────────────────────

/// 最新共享帧的发布与等待：捕获线程写入，各客户端阻塞等待新代次。
struct FrameHub {
    state: Mutex<HubState>,
    cv: Condvar,
}

struct HubState {
    generation: u64,
    latest: Option<Arc<SharedFrame>>,
    closed: bool,
}

impl FrameHub {
    fn new() -> Self {
        Self {
            state: Mutex::new(HubState {
                generation: 0,
                latest: None,
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn publish(&self, frame: Arc<SharedFrame>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed {
            return;
        }
        st.generation += 1;
        st.latest = Some(frame);
        drop(st);
        self.cv.notify_all();
    }

    fn close(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.closed = true;
        drop(st);
        self.cv.notify_all();
    }

    /// 等待比 `last_gen` 更新的帧；超时或服务关闭返回 None。
    fn wait_newer(&self, last_gen: u64, timeout: Duration) -> Option<(u64, Arc<SharedFrame>)> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (st, _) = self
            .cv
            .wait_timeout_while(st, timeout, |s| s.generation <= last_gen && !s.closed)
            .unwrap_or_else(|e| e.into_inner());
        if st.generation > last_gen {
            st.latest.clone().map(|f| (st.generation, f))
        } else {
            None
        }
    }
}

/// 编码后的 H.264 帧广播中心：编码器单生产者，各 WebRTC 会话多消费者。
struct EncodedHub {
    state: Mutex<EncodedHubState>,
    cv: Condvar,
}

struct EncodedHubState {
    generation: u64,
    latest: Option<EncodedFrame>,
    closed: bool,
}

impl EncodedHub {
    fn new() -> Self {
        Self {
            state: Mutex::new(EncodedHubState {
                generation: 0,
                latest: None,
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn publish(&self, frame: EncodedFrame) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed {
            return;
        }
        st.generation += 1;
        st.latest = Some(frame);
        drop(st);
        self.cv.notify_all();
    }

    fn close(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.closed = true;
        drop(st);
        self.cv.notify_all();
    }

    fn wait_newer(&self, last_gen: u64, timeout: Duration) -> Option<EncodedFrame> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (st, _) = self
            .cv
            .wait_timeout_while(st, timeout, |s| s.generation <= last_gen && !s.closed)
            .unwrap_or_else(|e| e.into_inner());
        if st.generation > last_gen {
            st.latest.clone()
        } else {
            None
        }
    }
}

/// 客户端弱网反馈 -> 编码器的控制通道。
#[derive(Default)]
struct EncoderControl {
    keyframe: bool,
    bitrate_kbps: Option<u32>,
}

/// 单条捕获线程：按 fps 截屏一次、算一次全局哈希，再广播给所有客户端。
///
/// 按固定节拍捕获原始 BGRA 帧并广播给编码器；空闲（无观看者）时暂停以省电。
fn spawn_capture_thread(
    fps: u32,
    stop: Arc<AtomicBool>,
    viewers: Arc<AtomicUsize>,
    hub: Arc<FrameHub>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("webdesk-capture".into())
        .spawn(move || {
            let mut capturer = match ScreenCapturer::new() {
                Ok(c) => c,
                Err(e) => {
                    log_error!("屏幕捕获初始化失败: {e}");
                    hub.close();
                    return;
                }
            };
            let interval = Duration::from_millis((1000 / fps.max(1)).max(1) as u64);
            let (width, height) = capturer.size();
            log_debug!("共享捕获线程启动 {width}x{height} @ {fps}fps");

            let mut last_frame: Option<Arc<SharedFrame>> = None;
            let mut idle = false;
            let mut next_tick = std::time::Instant::now();
            while !stop.load(Ordering::Relaxed) {
                // 没有已认证客户端时不截屏，保持空闲零开销
                if viewers.load(Ordering::Relaxed) == 0 {
                    if !idle {
                        idle = true;
                        log_debug!("无认证客户端，捕获线程暂停");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    next_tick = std::time::Instant::now();
                    continue;
                }
                idle = false;

                match capturer.capture() {
                    Ok(Some(frame)) => {
                        let shared = Arc::new(SharedFrame {
                            width: frame.width,
                            height: frame.height,
                            data: frame.data.to_vec(),
                        });
                        last_frame = Some(shared.clone());
                        hub.publish(shared);
                    }
                    Ok(None) => {
                        // 本拍无显示器更新（DXGI 空闲）：复用上一帧，代次仍递增
                        if let Some(last) = &last_frame {
                            hub.publish(last.clone());
                        }
                    }
                    Err(e) => log_warn!("截屏失败: {e}"),
                }

                // 绝对节拍：以固定 deadline 对齐，避免逐帧累积漂移；
                // 捕获耗时超过周期时不补帧，直接进入下一拍。
                next_tick += interval;
                let now = std::time::Instant::now();
                if next_tick < now {
                    next_tick = now;
                }
                loop {
                    let now = std::time::Instant::now();
                    let Some(remain) = next_tick.checked_duration_since(now) else {
                        break;
                    };
                    if remain.is_zero() {
                        break;
                    }
                    std::thread::sleep(remain.min(Duration::from_millis(25)));
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
            hub.close();
            log_debug!("共享捕获线程已退出");
        })
        .expect("启动捕获线程失败")
}

/// 单一编码线程：原始帧 -> H.264 -> 发布给所有 WebRTC 会话。
///
/// 全服务只编码一次，多个客户端共享同一份 Annex-B 数据；弱网反馈通过
/// `EncoderControl` 在下一帧生效（关键帧 / 目标码率）。
fn spawn_encoder_thread(
    fps: u32,
    bitrate_kbps: u32,
    min_bitrate_kbps: u32,
    stop: Arc<AtomicBool>,
    viewers: Arc<AtomicUsize>,
    frame_hub: Arc<FrameHub>,
    encoded_hub: Arc<EncodedHub>,
    control: Arc<Mutex<EncoderControl>>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("webdesk-encoder".into())
        .spawn(move || {
            let mut encoder: Option<VideoEncoder> = None;
            let mut last_gen = 0u64;
            let mut idle = false;
            let mut last_log = std::time::Instant::now();
            let mut encoded_bytes = 0usize;

            while !stop.load(Ordering::Relaxed) {
                if viewers.load(Ordering::Relaxed) == 0 {
                    if !idle {
                        idle = true;
                        log_debug!("无观看者，编码线程暂停");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                idle = false;

                // 应用弱网反馈（来自客户端的 keyframe / adapt 消息）
                let (want_keyframe, new_bitrate) = {
                    let mut c = control.lock().unwrap_or_else(|e| e.into_inner());
                    let k = c.keyframe;
                    c.keyframe = false;
                    (k, c.bitrate_kbps.take())
                };
                if let Some(requested) = new_bitrate {
                    // 服务端权威下限：即使客户端上报异常低值也绝不突破清晰度底线
                    let kbps = requested.max(min_bitrate_kbps);
                    if kbps != requested {
                        log_warn!(
                            "客户端建议码率 {requested} kbps 低于下限，已钳制为 {kbps} kbps"
                        );
                    }
                    if let Some(enc) = encoder.as_mut() {
                        match enc.set_bitrate(kbps) {
                            Ok(()) => log_info!("H264 目标码率调整为 {kbps} kbps"),
                            Err(e) => log_warn!("调整码率失败: {e}"),
                        }
                    }
                }
                if want_keyframe {
                    if let Some(enc) = encoder.as_mut() {
                        enc.request_keyframe();
                    }
                }

                let Some((generation, shared)) =
                    frame_hub.wait_newer(last_gen, Duration::from_millis(100))
                else {
                    continue;
                };
                last_gen = generation;

                if encoder.is_none()
                    || encoder.as_ref().map(|e| {
                        (e.width() as u32, e.height() as u32) != (shared.width, shared.height)
                    }) == Some(true)
                {
                    match VideoEncoder::new(shared.width, shared.height, fps, bitrate_kbps) {
                        Ok(enc) => {
                            log_info!(
                                "H264 编码器启动 {}x{} @ {}fps, {}kbps",
                                shared.width,
                                shared.height,
                                fps,
                                bitrate_kbps
                            );
                            encoder = Some(enc);
                        }
                        Err(e) => {
                            log_error!("H264 编码器初始化失败: {e}");
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    }
                }

                let enc = encoder.as_mut().expect("编码器已初始化");
                match enc.encode(&shared.data, shared.width, shared.height) {
                    Ok(data) if !data.is_empty() => {
                        encoded_bytes += data.len();
                        let is_keyframe = contains_idr(&data);
                        encoded_hub.publish(EncodedFrame {
                            generation,
                            data: Arc::new(data),
                            keyframe: is_keyframe,
                        });
                    }
                    Ok(_) => {}
                    Err(e) => log_warn!("H264 编码失败: {e}"),
                }

                if last_log.elapsed() >= Duration::from_secs(5) {
                    let kbps = encoded_bytes as f64 * 8.0 / last_log.elapsed().as_secs_f64() / 1000.0;
                    log_debug!("H264 输出码率约 {:.0} kbps", kbps);
                    encoded_bytes = 0;
                    last_log = std::time::Instant::now();
                }
            }
            encoded_hub.close();
            log_debug!("H264 编码线程已退出");
        })
        .expect("启动编码线程失败")
}

/// 粗判 Annex-B 流是否含 IDR（NAL type 5），仅用于日志/统计，不影响 RTP 打包。
fn contains_idr(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 4 < data.len() {
        let start = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            i + 3
        } else if i + 4 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            i + 4
        } else {
            i += 1;
            continue;
        };
        if start < data.len() && (data[start] & 0x1f) == 5 {
            return true;
        }
        i = start;
    }
    false
}

// ──────────────────────────────────────────────────────────────────────────────
// 双栈绑定
// ──────────────────────────────────────────────────────────────────────────────

async fn start_server(
    cfg: &ServerConfig,
    creds: Arc<RwLock<Credentials>>,
    status: Arc<Mutex<Status>>,
) -> Result<Running, String> {
    let mut listeners: Vec<(TcpListener, &'static str)> = Vec::new();

    // 固定双栈：IPv4 与 IPv6 各自独立绑定同一端口，任一协议栈不可用时降级为单栈
    match bind_v4(cfg.port) {
        Ok(l) => {
            log_info!("IPv4 监听已启动: 0.0.0.0:{}", cfg.port);
            listeners.push((l, "IPv4"));
        }
        Err(e) => log_warn!("IPv4 绑定失败（已跳过）: {e}"),
    }
    match bind_v6(cfg.port) {
        Ok(l) => {
            log_info!("IPv6 监听已启动: [::]:{}", cfg.port);
            listeners.push((l, "IPv6"));
        }
        Err(e) => log_warn!("IPv6 绑定失败（已跳过）: {e}"),
    }
    if listeners.is_empty() {
        return Err(format!(
            "端口 {} 绑定失败（可能已被占用，或系统缺少对应协议栈）",
            cfg.port
        ));
    }

    let (shutdown, _) = watch::channel(false);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let clients = Arc::new(AtomicUsize::new(0));
    let viewers = Arc::new(AtomicUsize::new(0));
    let hub = Arc::new(FrameHub::new());
    let encoded_hub = Arc::new(EncodedHub::new());
    let encoder_control = Arc::new(Mutex::new(EncoderControl::default()));
    let frame_interval = Duration::from_millis((1000 / cfg.fps.max(1)).max(1) as u64);
    let html = Arc::new(render_client_html());
    let (screen_width, screen_height) = virtual_screen_size();
    if screen_width == 0 || screen_height == 0 {
        return Err("无法获取屏幕分辨率".into());
    }

    let state = AppState {
        html,
        creds,
        port: cfg.port,
        screen_width,
        screen_height,
        status,
        clients: clients.clone(),
        viewers: viewers.clone(),
        encoded_hub: encoded_hub.clone(),
        encoder_control: encoder_control.clone(),
        min_bitrate_kbps: cfg.min_bitrate_kbps,
        frame_interval,
        shutdown: shutdown.clone(),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/client.html", get(index))
        .route("/health", get(health))
        .route("/favicon.ico", get(favicon))
        .route("/ws", get(ws_handler))
        .fallback(not_found)
        .layer(middleware::from_fn(rewrite_ws))
        .with_state(state);

    // 全局唯一捕获线程 + 编码线程：全服务只编码一次，多客户端共享 H.264 帧
    let capture_thread = spawn_capture_thread(cfg.fps, stop_flag.clone(), viewers.clone(), hub.clone());
    let encoder_thread = spawn_encoder_thread(
        cfg.fps,
        cfg.bitrate_kbps,
        cfg.min_bitrate_kbps,
        stop_flag.clone(),
        viewers.clone(),
        hub.clone(),
        encoded_hub.clone(),
        encoder_control.clone(),
    );

    let mut tasks = Vec::new();
    for (listener, label) in listeners {
        let mut rx = shutdown.subscribe();
        let app = app.clone();
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            // wait_for 在值已为 true 时立即返回
            let _ = rx.wait_for(|v| *v).await;
        });
        tasks.push(tokio::spawn(async move {
            if let Err(e) = serve.await {
                log_error!("{label} HTTP 服务异常: {e}");
            }
        }));
    }

    Ok(Running {
        shutdown,
        stop_flag,
        clients,
        tasks,
        capture_thread: Some(capture_thread),
        encoder_thread: Some(encoder_thread),
    })
}

fn bind_v4(port: u16) -> std::io::Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?;
    socket.listen(128)
}

fn bind_v6(port: u16) -> std::io::Result<TcpListener> {
    use std::net::TcpListener as StdListener;
    let addr: SocketAddr = (Ipv6Addr::UNSPECIFIED, port).into();
    let sock = socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    // 显式 v6-only：与 IPv4 双 socket 双栈设计一致，也避免 Linux 默认双栈
    // 与先绑定的 IPv4 端口冲突（EADDRINUSE）
    sock.set_only_v6(true)?;
    // tokio::net::TcpListener::from_std 要求非阻塞模式；
    // 若传入阻塞 socket，accept/read 会卡死整个 tokio worker 线程
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    sock.listen(128)?;
    let std_listener = StdListener::from(sock);
    TcpListener::from_std(std_listener)
}

/// 若程序目录存在自定义 client.html 则优先使用，否则回退到内嵌页面。
fn render_client_html() -> String {
    let external = config::app_dir().join("client.html");
    std::fs::read_to_string(external)
        .unwrap_or_else(|_| include_str!("../assets/client.html").to_string())
}

// ──────────────────────────────────────────────────────────────────────────────
// HTTP
// ──────────────────────────────────────────────────────────────────────────────

/// 兼容旧客户端：直接对根路径发起 WebSocket 握手时，内部重写成 `/ws`。
async fn rewrite_ws(mut req: Request, next: Next) -> Response {
    let is_ws = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if is_ws {
        *req.uri_mut() = "/ws".parse().unwrap();
    }
    next.run(req).await
}

async fn index(State(state): State<AppState>) -> Response {
    let mut resp = Html((*state.html).clone()).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

async fn health(State(state): State<AppState>) -> Response {
    let auth_required = state.creds.read().await.required();
    let body = json!({
        "ok": true,
        "port": state.port,
        "auth_required": auth_required,
        "clients": state.clients.load(Ordering::Relaxed),
    });
    let mut resp = axum::Json(body).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "Not Found")
}

// ──────────────────────────────────────────────────────────────────────────────
// WebSocket 会话
// ──────────────────────────────────────────────────────────────────────────────

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    // 安全底线：未设置访问密码时不接受任何客户端连接
    if !state.creds.read().await.required() {
        log_warn!("未设置访问密码，已拒绝客户端连接");
        return (StatusCode::FORBIDDEN, "未设置访问密码，客户端无法连接").into_response();
    }
    // 输入消息只有几十字节；限制帧大小，避免超大帧吃掉内存
    ws.max_message_size(256 * 1024)
        .max_frame_size(256 * 1024)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

fn init_payload(state: &AppState, require_auth: bool) -> String {
    json!({
        "type": "init",
        // 未认证时不下发分辨率，避免信息泄露
        "width": if require_auth { 0 } else { state.screen_width },
        "height": if require_auth { 0 } else { state.screen_height },
        "codec": "h264",
        "transport": "webrtc-host",
        // 下发给客户端，保证两端自适应使用同一下限
        "min_bitrate_kbps": state.min_bitrate_kbps,
        "require_auth": require_auth,
    })
    .to_string()
}

/// 客户端槽位的 RAII 守卫：任何退出路径（含 early return / panic）都归还计数与状态，
/// 杜绝「连接断开但 clients 计数不减」的泄漏。
struct ClientGuard {
    clients: Arc<AtomicUsize>,
    status: Arc<Mutex<Status>>,
}

impl ClientGuard {
    fn acquire(state: &AppState) -> Option<Self> {
        let prev = state
            .clients
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_CLIENTS).then_some(n + 1)
            })
            .ok()?;
        let count = prev + 1;
        lock_status(&state.status).clients = count;
        log_info!(
            "客户端连接（当前 {count} 个 / 上限 {MAX_CLIENTS}）"
        );
        Some(Self {
            clients: state.clients.clone(),
            status: state.status.clone(),
        })
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        let left = self.clients.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
        lock_status(&self.status).clients = left;
        log_info!("客户端断开（剩余 {left} 个）");
    }
}

/// 已认证观看者的 RAII 计数：为 0 时共享捕获线程暂停截屏。
struct ViewerGuard {
    viewers: Arc<AtomicUsize>,
}

impl ViewerGuard {
    fn acquire(state: &AppState) -> Self {
        state.viewers.fetch_add(1, Ordering::SeqCst);
        Self {
            viewers: state.viewers.clone(),
        }
    }
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.viewers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 带超时的文本发送：慢客户端不能把接收循环卡死在 `tx.send().await`。
async fn send_text(tx: &mpsc::Sender<OutMsg>, text: String) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, tx.send(OutMsg::Text(text))).await,
        Ok(Ok(()))
    )
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    // 超过上限直接拒绝，避免无限连接把截屏线程与内存耗尽
    let _guard = match ClientGuard::acquire(&state) {
        Some(g) => g,
        None => {
            log_warn!("连接数已达上限 {MAX_CLIENTS}，拒绝新客户端");
            return;
        }
    };

    let require_auth = state.creds.read().await.required();
    // 认证通过后才计入观看者，捕获/编码线程只服务有人观看的会话
    let mut viewer = if require_auth {
        None
    } else {
        Some(ViewerGuard::acquire(&state))
    };
    let authenticated = Arc::new(AtomicBool::new(!require_auth));
    let conn_stop = Arc::new(AtomicBool::new(false));
    let media_stop = Arc::new(AtomicBool::new(false));

    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<OutMsg>(OUT_QUEUE_CAP);

    // 认证前后都先发 init（客户端依赖它决定弹不弹密码框）
    if !send_text(&tx, init_payload(&state, require_auth)).await {
        return;
    }

    // 控制面写任务：控制消息为 JSON 文本（画面已不再走 WS）
    let mut writer = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            let msg = Message::Text(match out {
                OutMsg::Text(text) => text,
            });
            match tokio::time::timeout(WRITE_TIMEOUT, sink.send(msg)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    log_debug!("控制消息写超时，关闭连接写端");
                    break;
                }
            }
        }
        let _ = sink.close().await;
    });

    let mut shutdown_rx = state.shutdown.subscribe();
    // WebRTC 会话与媒体发送任务
    let mut rtc_writer: Option<rtc::TrackWriter> = None;
    let mut pc_state_rx: Option<mpsc::UnboundedReceiver<RTCPeerConnectionState>> = None;
    let mut media_task: Option<tokio::task::JoinHandle<()>> = None;

    // 输入状态在连接结束时 Drop，会抬起本会话所有仍未松开的按键
    let mut input_state = InputState::new();
    let mut failures = 0u32;
    let mut watchdog = tokio::time::interval(WATCHDOG_TICK);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    macro_rules! require_auth_ok {
        () => {
            authenticated.load(Ordering::Relaxed)
        };
    }

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            // WebRTC 连接状态：失败 / 关闭时结束整个会话
            some_state = async {
                match pc_state_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(st) = some_state {
                    if matches!(st, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
                        log_warn!("WebRTC 连接状态异常: {st:?}");
                        break;
                    }
                }
            }
            // 周期性对账：客户端心跳刷新按键时间戳，超时未确认的按键自动抬起
            _ = watchdog.tick() => {
                input_state.release_stale(KEY_STICKY_TIMEOUT);
            },
            incoming = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => {
                let result = match incoming {
                    Ok(Some(r)) => r,
                    Ok(None) => break,
                    Err(_) => {
                        log_info!("客户端空闲超时，断开连接");
                        break;
                    }
                };
                let Ok(message) = result else { break };
                let text = match message {
                    Message::Text(t) => t,
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(msg) = serde_json::from_str::<ClientMessage>(&text) else { continue };

                match msg.kind.as_str() {
                    "auth" => {
                        let creds = state.creds.read().await.clone();
                        let candidate = msg.password.clone();
                        let ok = tokio::task::spawn_blocking(move || {
                            config::verify_password(&candidate, &creds.hash, &creds.plain)
                        })
                        .await
                        .unwrap_or(false);
                        if ok || !creds_required(&state).await {
                            if viewer.is_none() {
                                viewer = Some(ViewerGuard::acquire(&state));
                            }
                            authenticated.store(true, Ordering::Relaxed);
                            failures = 0;
                            let _ = send_text(&tx, json!({"type":"auth_result","success":true}).to_string()).await;
                            let _ = send_text(&tx, init_payload(&state, false)).await;
                            log_info!("客户端认证成功");
                        } else {
                            failures += 1;
                            tokio::time::sleep(Duration::from_millis(auth_backoff_ms(failures))).await;
                            let _ = send_text(&tx, json!({
                                "type":"auth_result", "success":false,
                                "message":"密码错误",
                                "attempts_left": MAX_AUTH_FAILURES.saturating_sub(failures)
                            }).to_string()).await;
                            log_warn!("客户端认证失败（{failures} 次）");
                            if failures >= MAX_AUTH_FAILURES {
                                let _ = send_text(&tx, json!({"type":"auth_locked","message":"尝试次数过多，请重新连接"}).to_string()).await;
                                break;
                            }
                        }
                    }
                    "offer" => {
                        if !require_auth_ok!() || rtc_writer.is_some() {
                            continue;
                        }
                        let sdp = msg.sdp.clone();
                        match tokio::time::timeout(Duration::from_secs(20), rtc::establish(sdp)).await {
                            Ok(Ok((s, answer))) => {
                                let rtc::RtcSession { writer, state_rx } = s;
                                let track_writer = writer.clone();
                                rtc_writer = Some(writer);
                                pc_state_rx = Some(state_rx);

                                if !send_text(&tx, json!({"type":"answer","sdp":answer}).to_string()).await {
                                    break;
                                }

                                // 编码帧 -> 有界队列 -> RTP 轨；blocking 线程只读编码中心，
                                // 不占用 tokio worker，慢客户端由队列背压 + 丢弃保护。
                                let (frame_tx, mut frame_rx) = mpsc::channel::<EncodedFrame>(4);
                                let hub = state.encoded_hub.clone();
                                let stop_pull = media_stop.clone();
                                tokio::task::spawn_blocking(move || {
                                    let mut last_gen = 0u64;
                                    while !stop_pull.load(Ordering::Relaxed) {
                                        if let Some(frame) = hub.wait_newer(last_gen, Duration::from_millis(100)) {
                                            last_gen = frame.generation;
                                            if frame_tx.blocking_send(frame).is_err() {
                                                break;
                                            }
                                        }
                                    }
                                });
                                let interval = state.frame_interval;
                                media_task = Some(tokio::spawn(async move {
                                    while let Some(frame) = frame_rx.recv().await {
                                        let data = (*frame.data).clone();
                                        if let Err(e) = track_writer.write_h264(data, interval).await {
                                            log_warn!("WebRTC 写帧失败: {e}");
                                            break;
                                        }
                                    }
                                }));
                                log_info!("WebRTC 会话已建立（host 直连）");
                            }
                            Ok(Err(e)) => {
                                log_warn!("WebRTC 建立失败: {e}");
                                let _ = send_text(&tx, json!({"type":"error","message":e}).to_string()).await;
                            }
                            Err(_) => {
                                log_warn!("WebRTC 建立超时");
                                let _ = send_text(&tx, json!({"type":"error","message":"WebRTC 建立超时"}).to_string()).await;
                            }
                        }
                    }
                    "keyframe" | "request_full" => {
                        // 兼容旧客户端 request_full：映射为请求一个 IDR 关键帧
                        if require_auth_ok!() {
                            if let Ok(mut c) = state.encoder_control.lock() {
                                c.keyframe = true;
                            }
                        }
                    }
                    "adapt" => {
                        // 弱网反馈：客户端根据 getStats() 丢包率建议目标码率。
                        // 入口即钳制下限，避免字体因码率过低而模糊（编码线程还有一道兜底）。
                        if require_auth_ok!() && msg.bitrate_kbps > 0 {
                            let kbps = msg.bitrate_kbps.max(state.min_bitrate_kbps);
                            if kbps != msg.bitrate_kbps {
                                log_debug!(
                                    "adapt 码率 {} kbps 低于下限，钳制为 {} kbps",
                                    msg.bitrate_kbps,
                                    kbps
                                );
                            }
                            if let Ok(mut c) = state.encoder_control.lock() {
                                c.bitrate_kbps = Some(kbps);
                            }
                            log_debug!("客户端建议码率 {} kbps", kbps);
                        }
                    }
                    "ping" => {
                        if require_auth_ok!() {
                            input_state.sync_pressed(&msg);
                        }
                        let _ = send_text(&tx, json!({"type":"pong"}).to_string()).await;
                    }
                    "sync_keys" | "release_all" => {
                        if require_auth_ok!() {
                            if msg.kind == "release_all" {
                                input_state.release_all();
                                log_debug!("客户端请求释放全部按键");
                            } else {
                                input_state.sync_pressed(&msg);
                            }
                        }
                    }
                    _ => {
                        if require_auth_ok!() {
                            input::handle(&msg, &mut input_state);
                        }
                    }
                }
            }
        }
    }

    conn_stop.store(true, Ordering::Relaxed);
    media_stop.store(true, Ordering::Relaxed);
    if let Some(w) = rtc_writer.as_ref() {
        w.close().await;
    }
    drop(tx);
    if let Some(task) = media_task {
        task.abort();
    }
    let _ = tokio::time::timeout(Duration::from_millis(300), &mut writer).await;
    writer.abort();
    // input_state 与 _guard 在此 Drop：抬起粘滞按键、归还连接槽位
}

async fn creds_required(state: &AppState) -> bool {
    state.creds.read().await.required()
}
