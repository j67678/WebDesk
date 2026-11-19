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

use crate::capture::{
    compute_hashes, encode_jpeg, extract_rgb, merge_dirty_rects, tile_grid, virtual_screen_size,
    DiffDetector, DirtyRect, Frame, ScreenCapturer, SharedFrame,
};
use crate::config::{self, Config};
use crate::input::{self, InputState};
use crate::log_debug;
use crate::log_error;
use crate::log_info;
use crate::log_warn;
use crate::protocol::{auth_backoff_ms, ClientMessage, MAX_AUTH_FAILURES};

const BOOT_MARKER: &str = "/*__WEBDESK_BOOT__*/";

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
    pub tile_size: u32,
    pub quality: u8,
    pub password_hash: String,
    pub password_plain: String,
}

impl From<&Config> for ServerConfig {
    fn from(c: &Config) -> Self {
        Self {
            port: c.port,
            fps: c.fps,
            tile_size: c.tile_size,
            quality: c.quality,
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

/// 出站消息：控制消息保持 JSON 文本，画面帧走二进制以省掉 Base64 与 JSON 转义。
enum OutMsg {
    Text(String),
    Binary(Vec<u8>),
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
    tile_size: u32,
    quality: u8,
    screen_width: u32,
    screen_height: u32,
    status: Arc<Mutex<Status>>,
    clients: Arc<AtomicUsize>,
    /// 已认证的观看者数量；为 0 时捕获线程暂停
    viewers: Arc<AtomicUsize>,
    hub: Arc<FrameHub>,
    shutdown: watch::Sender<bool>,
    stop_flag: Arc<AtomicBool>,
}

struct Running {
    shutdown: watch::Sender<bool>,
    stop_flag: Arc<AtomicBool>,
    clients: Arc<AtomicUsize>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    capture_thread: Option<std::thread::JoinHandle<()>>,
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

/// 单条捕获线程：按 fps 截屏一次、算一次全局哈希，再广播给所有客户端。
///
/// 与上一帧哈希完全相同时复用同一份像素（不重复拷贝整屏），
/// 仍然递增代次，让新认证 / 请求全量刷新的客户端能立即拿到当前画面。
fn spawn_capture_thread(
    fps: u32,
    tile_size: u32,
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
            while !stop.load(Ordering::Relaxed) {
                // 没有已认证客户端时不截屏，保持空闲零开销
                if viewers.load(Ordering::Relaxed) == 0 {
                    if !idle {
                        idle = true;
                        log_debug!("无认证客户端，捕获线程暂停");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                idle = false;

                let t0 = std::time::Instant::now();
                match capturer.capture() {
                    Ok(Some(frame)) => {
                        let (cols, rows) = tile_grid(frame.width, frame.height, tile_size);
                        let hashes = compute_hashes(&frame, tile_size, cols, rows);
                        let changed = match &last_frame {
                            Some(f) => f.hashes != hashes,
                            None => true,
                        };
                        let shared = if changed {
                            Arc::new(SharedFrame {
                                width: frame.width,
                                height: frame.height,
                                cols,
                                rows,
                                data: frame.data.to_vec(),
                                hashes,
                            })
                        } else {
                            last_frame.clone().expect("首帧必然发布")
                        };
                        last_frame = Some(shared.clone());
                        hub.publish(shared);
                    }
                    Ok(None) => {
                        // 本拍没有任何显示器更新（DXGI 空闲）：复用上一帧像素与哈希，
                        // 代次仍递增，让新认证 / 请求全量刷新的客户端拿到当前画面。
                        if let Some(last) = &last_frame {
                            hub.publish(last.clone());
                        }
                    }
                    Err(e) => log_warn!("截屏失败: {e}"),
                }

                let mut remain = interval.saturating_sub(t0.elapsed());
                while remain > Duration::ZERO {
                    let step = remain.min(Duration::from_millis(25));
                    std::thread::sleep(step);
                    remain -= step;
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
    let html = Arc::new(render_client_html(cfg.port));
    let (screen_width, screen_height) = virtual_screen_size();
    if screen_width == 0 || screen_height == 0 {
        return Err("无法获取屏幕分辨率".into());
    }

    let state = AppState {
        html,
        creds,
        port: cfg.port,
        tile_size: cfg.tile_size,
        quality: cfg.quality,
        screen_width,
        screen_height,
        status,
        clients: clients.clone(),
        viewers: viewers.clone(),
        hub: hub.clone(),
        shutdown: shutdown.clone(),
        stop_flag: stop_flag.clone(),
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

    // 全局唯一捕获线程：所有客户端共享同一帧与同一份哈希
    let capture_thread = spawn_capture_thread(
        cfg.fps,
        cfg.tile_size,
        stop_flag.clone(),
        viewers.clone(),
        hub.clone(),
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

/// 把服务端启动参数注入页面；若程序目录存在自定义 client.html 则优先使用。
fn render_client_html(port: u16) -> String {
    let external = config::app_dir().join("client.html");
    let template = std::fs::read_to_string(external)
        .unwrap_or_else(|_| include_str!("../assets/client.html").to_string());
    let boot = json!({
        "port": port,
        "autoConnect": true,
        "serverInjected": true,
    })
    .to_string();
    if template.contains(BOOT_MARKER) {
        template.replace(BOOT_MARKER, &format!("window.WEBDESK_BOOT = {boot};"))
    } else {
        template.replace("value=\"8765\"", &format!("value=\"{port}\""))
    }
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
        "tile_size": state.tile_size,
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
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
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
    // 认证通过后才计入观看者，捕获线程只服务有人观看的会话
    let mut viewer = if require_auth {
        None
    } else {
        Some(ViewerGuard::acquire(&state))
    };
    let authenticated = Arc::new(AtomicBool::new(!require_auth));
    let full_refresh = Arc::new(AtomicBool::new(false));
    let conn_stop = Arc::new(AtomicBool::new(false));

    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<OutMsg>(OUT_QUEUE_CAP);

    // 认证前后都先发 init（旧客户端依赖它决定弹不弹密码框）
    if !send_text(&tx, init_payload(&state, require_auth)).await {
        return;
    }

    let mut writer = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            let msg = match out {
                OutMsg::Text(text) => Message::Text(text),
                OutMsg::Binary(bytes) => Message::Binary(bytes),
            };
            // 客户端长期不读 TCP 时主动断开，避免写任务与缓冲长期驻留
            match tokio::time::timeout(WRITE_TIMEOUT, sink.send(msg)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    log_debug!("写帧超时，关闭连接写端");
                    break;
                }
            }
        }
        let _ = sink.close().await;
    });

    // 帧消费在 blocking 线程：差集与 JPEG 编码是 CPU 密集操作，不占用异步线程。
    // 截屏与全局哈希由共享捕获线程完成，这里只等待新帧并编码本会话需要的瓦片。
    let frame_stop = conn_stop.clone();
    let server_stop = state.stop_flag.clone();
    let frame_auth = authenticated.clone();
    let frame_full = full_refresh.clone();
    let frame_tx = tx.clone();
    let frame_hub = state.hub.clone();
    let mut shutdown_rx = state.shutdown.subscribe();
    let tile_size = state.tile_size;
    let quality = state.quality;
    let mut frame_task = tokio::task::spawn_blocking(move || {
        let mut detector = DiffDetector::new();
        let mut last_gen = 0u64;
        let mut rects: Vec<DirtyRect> = Vec::new();
        let mut rgb: Vec<u8> = Vec::new();

        while !frame_stop.load(Ordering::Relaxed)
            && !server_stop.load(Ordering::Relaxed)
            && !frame_tx.is_closed()
        {
            if !frame_auth.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if frame_full.swap(false, Ordering::Relaxed) {
                detector.reset();
            }
            // 等待捕获线程发布新帧；超时只是为了周期性检查退出标志
            let Some((generation, shared)) =
                frame_hub.wait_newer(last_gen, Duration::from_millis(100))
            else {
                continue;
            };
            last_gen = generation;

            let frame = Frame {
                width: shared.width,
                height: shared.height,
                data: &shared.data,
            };
            detector.diff(&shared.hashes, shared.cols, shared.rows);
            if detector.changed().is_empty() {
                continue;
            }

            // 相邻脏瓦片合并成矩形：全屏变化时只需编码 1 块，避免数百个 JPEG 头
            rects.clear();
            merge_dirty_rects(
                detector.changed(),
                shared.cols,
                shared.rows,
                shared.width,
                shared.height,
                tile_size,
                &mut rects,
            );

            // 二进制帧：类型(1) + u16 矩形数 + u16 宽 + u16 高 + [x,y,w,h u16][len u32][jpeg]
            let mut payload = Vec::with_capacity(7 + rects.len() * 64);
            payload.extend_from_slice(&[1u8, 0, 0, 0, 0, 0, 0]);
            let mut count = 0u16;
            for rect in &rects {
                extract_rgb(&frame, rect.x, rect.y, rect.w, rect.h, &mut rgb);
                match encode_jpeg(&rgb, rect.w, rect.h, quality) {
                    Ok(jpeg) => {
                        payload.extend_from_slice(&(rect.x as u16).to_le_bytes());
                        payload.extend_from_slice(&(rect.y as u16).to_le_bytes());
                        payload.extend_from_slice(&(rect.w as u16).to_le_bytes());
                        payload.extend_from_slice(&(rect.h as u16).to_le_bytes());
                        payload.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
                        payload.extend_from_slice(&jpeg);
                        count = count.saturating_add(1);
                    }
                    Err(e) => log_warn!("JPEG 编码失败: {e}"),
                }
            }
            if count == 0 {
                continue;
            }
            payload[1..3].copy_from_slice(&count.to_le_bytes());
            payload[3..5].copy_from_slice(&(shared.width.min(u16::MAX as u32) as u16).to_le_bytes());
            payload[5..7].copy_from_slice(&(shared.height.min(u16::MAX as u32) as u16).to_le_bytes());
            match frame_tx.try_send(OutMsg::Binary(payload)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // 慢客户端：丢弃本帧并请求下一帧全量刷新，保证画面最终一致
                    frame_full.store(true, Ordering::Relaxed);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
        log_debug!("客户端帧循环已退出");
    });

    // 接收循环：认证 / 输入 / 心跳
    // InputState 在连接结束时 Drop，会抬起本会话所有仍未松开的按键，
    // 从根上避免「远程 Ctrl 一直按着」（keyup 丢失、切窗口、断线重连均覆盖）。
    let mut input_state = InputState::new();
    let mut failures = 0u32;
    let mut watchdog = tokio::time::interval(WATCHDOG_TICK);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
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
                    "request_full" => {
                        if authenticated.load(Ordering::Relaxed) {
                            full_refresh.store(true, Ordering::Relaxed);
                        }
                    }
                    "ping" => {
                        // 心跳携带客户端真实按下集合：差集释放服务端残留的按键
                        if authenticated.load(Ordering::Relaxed) {
                            input_state.sync_pressed(&msg);
                        }
                        let _ = send_text(&tx, json!({"type":"pong"}).to_string()).await;
                    }
                    "sync_keys" | "release_all" => {
                        if authenticated.load(Ordering::Relaxed) {
                            if msg.kind == "release_all" {
                                input_state.release_all();
                                log_debug!("客户端请求释放全部按键");
                            } else {
                                input_state.sync_pressed(&msg);
                            }
                        }
                    }
                    _ => {
                        if authenticated.load(Ordering::Relaxed) {
                            input::handle(&msg, &mut input_state);
                        }
                    }
                }
            }
        }
    }

    conn_stop.store(true, Ordering::Relaxed);
    drop(tx);
    // spawn_blocking 无法被 abort 取消，靠 conn_stop / 通道关闭自行退出；
    // 给它一点时间收尾，再兜底取消异步写任务，避免线程与连接泄漏。
    let _ = tokio::time::timeout(Duration::from_millis(300), &mut frame_task).await;
    frame_task.abort();
    let _ = tokio::time::timeout(Duration::from_millis(300), &mut writer).await;
    writer.abort();
    // input_state 与 _guard 在此 Drop：抬起粘滞按键、归还连接槽位
}

async fn creds_required(state: &AppState) -> bool {
    state.creds.read().await.required()
}
