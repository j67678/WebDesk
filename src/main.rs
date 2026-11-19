//! WebDesk — Rust 重写的远程桌面服务端 + egui 桌面 GUI。
//!
//! 启动方式：
//!   webdesk                    图形界面（服务自动启动并常驻运行）
//!   webdesk --headless          无界面运行（Ctrl+C 退出）
//!   webdesk --minimized         启动即最小化到托盘
//!   webdesk --port 9000         临时覆盖端口

#![cfg_attr(windows, windows_subsystem = "windows")]

/// Windows 下附加到父进程的控制台，让 `--help` / `--headless` 等
/// 命令行场景能正常输出，同时双击 GUI 模式不会弹出黑窗口。
#[cfg(windows)]
fn attach_console_if_needed() {
    use windows_sys::Win32::System::Console::AttachConsole;
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // AttachConsole 返回非零表示成功（有父控制台可用）；
    // 失败（零）意味着是双击启动，没有控制台，静默忽略即可。
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_console_if_needed() {}

mod capture;
mod config;
mod firewall;
mod globals;
mod gui;
mod input;
mod logger;
mod protocol;
mod server;
mod single_instance;
mod tray;
use std::path::PathBuf;

use config::Config;
use server::ServerController;

#[derive(Debug, Default)]
struct Args {
    minimized: bool,
    headless: bool,
    port: Option<u16>,
    config_path: Option<PathBuf>,
    password: Option<String>,
    open_browser: bool,
    verbose: bool,
    help: bool,
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--minimized" | "--start-minimized" => a.minimized = true,
            "--no-gui" | "--headless" => a.headless = true,
            "--open-browser" => a.open_browser = true,
            "--verbose" | "-v" => a.verbose = true,
            "--help" | "-h" => a.help = true,
            "--port" => {
                if let Some(v) = it.next() {
                    a.port = v.parse().ok();
                }
            }
            "--config" => {
                if let Some(v) = it.next() {
                    a.config_path = Some(PathBuf::from(v));
                }
            }
            "--password" => {
                if let Some(v) = it.next() {
                    a.password = Some(v);
                }
            }
            _ => log_warn!("忽略未知参数: {arg}"),
        }
    }
    a
}

fn print_help() {
    println!(
        "WebDesk 2.0 (Rust)\n\
         \n\
         用法: webdesk [选项]\n\
         \n\
           --minimized          启动后直接最小化到系统托盘\n\
           --headless           无界面模式（等价于 --no-gui），Ctrl+C 退出\n\
           --port <N>           临时覆盖监听端口\n\
           --config <PATH>      指定 config.ini 路径\n\
           --password <PWD>     临时指定访问密码（不写入配置文件）\n\
           --open-browser       服务启动后打开浏览器\n\
           --verbose            输出调试日志\n\
           -h, --help           显示帮助\n"
    );
}

fn load_config(args: &Args) -> Config {
    let mut cfg = Config::load(args.config_path.as_deref());
    if let Some(port) = args.port {
        cfg.port = port;
    }
    if let Some(pwd) = &args.password {
        cfg.password_hash = config::hash_password(pwd);
        cfg.password_plain.clear();
    }
    cfg.clamped()
}

/// 让 GetSystemMetrics / GDI / DXGI 使用同一套物理像素坐标。
/// 否则非 DPI 感知进程在 150% 缩放下会拿到 1280x720（逻辑），
/// 而 DXGI 输出是 1920x1080（物理），导致画面截断。
#[cfg(windows)]
fn enable_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[cfg(not(windows))]
fn enable_dpi_awareness() {}

fn main() {
    attach_console_if_needed();
    enable_dpi_awareness();
    let args = parse_args();
    if args.help {
        print_help();
        return;
    }
    globals::set_verbose(args.verbose);
    logger::init(Some(config::log_path()));
    log_info!("WebDesk 2.0.0 (Rust) 启动 | 日志: {}", logger::path_string());

    // 单实例：已有实例在运行时唤起它的主窗口，避免重复监听端口
    let _instance = match single_instance::acquire() {
        Some(guard) => guard,
        None => {
            if !args.headless && single_instance::activate_existing_window() {
                log_info!("检测到 WebDesk 已在运行，已唤起其主窗口，本次启动退出");
            } else {
                log_warn!("检测到 WebDesk 已在运行（无界面模式或窗口尚未就绪），本次启动退出");
            }
            return;
        }
    };

    if args.headless {
        run_headless(args);
        return;
    }

    let opts = gui::GuiOptions {
        minimized: args.minimized,
        port_override: args.port,
        config_path: args.config_path.clone(),
    };
    // headless 的临时密码在 GUI 模式下通过配置加载；命令行 --password 时优先应用
    if let Some(pwd) = &args.password {
        let mut cfg = Config::load(args.config_path.as_deref());
        cfg.password_hash = config::hash_password(pwd);
        cfg.password_plain.clear();
        let _ = cfg.save(args.config_path.as_deref());
    }
    if let Err(e) = gui::run(opts) {
        log_error!("图形界面启动失败: {e}");
        eprintln!("图形界面启动失败: {e}");
        std::process::exit(1);
    }
    log_info!("WebDesk 已退出");
}

fn run_headless(args: Args) {
    let cfg = load_config(&args);
    log_info!(
        "无界面模式 | 端口 {} | 密码={}",
        cfg.port,
        if cfg.auth_required() { "启用" } else { "关闭" }
    );
    for url in config::local_addresses(cfg.port) {
        log_info!("   访问地址: {url}");
    }
    let controller = ServerController::new();
    controller.start(&cfg);
    if args.open_browser {
        let _ = config::open_with_system(&format!("http://127.0.0.1:{}/", cfg.port));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("无法创建 Ctrl+C 监听运行时");
    rt.block_on(gui::wait_for_ctrl_c(&controller));
    log_info!("无界面模式结束");
}
