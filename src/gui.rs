//! egui/eframe 桌面 GUI：服务 / 安全 / 性能 / 常规 / 日志五个分页。
//! 渲染后端为 glow(OpenGL)，Windows 下不依赖 WebView2 或任何浏览器运行时。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, ViewportCommand};
use eframe::{App, CreationContext, Frame, NativeOptions};

use crate::config::{self, Config};
use crate::server::{ServerController, StateKind, Status};
use crate::tray::{self, Tray, TrayAction};
use crate::{log_error, log_info, log_warn};

/// 主窗口标题；单实例模式按此标题查找已运行实例的窗口。
pub const WINDOW_TITLE: &str = "WebDesk · 远程桌面";

#[derive(Clone, Debug, Default)]
pub struct GuiOptions {
    pub minimized: bool,
    pub no_start: bool,
    pub port_override: Option<u16>,
    pub config_path: Option<PathBuf>,
    pub open_browser: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Service,
    Performance,
    General,
    Logs,
}

pub fn run(opts: GuiOptions) -> eframe::Result<()> {
    let icon = egui::IconData {
        rgba: tray::icon_rgba(),
        width: 32,
        height: 32,
    };
    let native = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 640.0])
            .with_min_inner_size([720.0, 480.0])
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        native,
        Box::new(move |cc| Ok(Box::new(WebDeskApp::new(cc, opts)))),
    )
}

/// egui 内置字体不含汉字，运行时加载系统中文字体（不打包字体文件）。
fn setup_fonts(ctx: &egui::Context) {
    let candidates: &[&str] = if cfg!(windows) {
        &[
            "C:\\Windows\\Fonts\\simhei.ttf",
            "C:\\Windows\\Fonts\\Deng.ttf",
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\simsun.ttc",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ]
    } else {
        &[
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        ]
    };
    let mut fonts = egui::FontDefinitions::default();
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("cjk".to_owned());
            }
            log_info!("已加载中文字体: {path}");
            break;
        }
    }
    ctx.set_fonts(fonts);
}

struct WebDeskApp {
    opts: GuiOptions,
    /// 界面上正在编辑的配置
    cfg: Config,
    /// 已应用到服务的配置
    applied: Config,
    controller: ServerController,
    tray: Option<Tray>,
    status: Status,
    tab: Tab,
    visible: bool,
    really_quit: bool,
    confirm_exit: bool,
    password_new: String,
    password_confirm: String,
    password_error: Option<String>,
    show_password: bool,
    password_warning_dismissed: bool,
    logs: Vec<String>,
    log_revision: u64,
    last_log_check: Instant,
    toast: Option<(String, Instant, bool)>,
    browser_opened: bool,
    repaint_alive: Arc<AtomicBool>,
}

impl WebDeskApp {
    fn new(cc: &CreationContext<'_>, opts: GuiOptions) -> Self {
        setup_fonts(&cc.egui_ctx);

        let mut cfg = Config::load(opts.config_path.as_deref());
        if let Some(port) = opts.port_override {
            cfg.port = port;
        }
        cfg = cfg.clamped();
        // 以操作系统实际的开机自启状态为准
        cfg.autostart = config::autostart_enabled();

        let controller = ServerController::new();
        let tray = Tray::new();
        if tray.is_none() {
            log_warn!("系统托盘不可用，关闭窗口将直接退出程序");
        }

        let visible = !(opts.minimized || cfg.start_minimized);
        if !visible {
            cc.egui_ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            log_info!("已按设置启动并最小化到系统托盘");
        }

        // 窗口隐藏时 winit 可能停止重绘，用后台线程持续唤醒，保证托盘菜单可用
        let repaint_alive = Arc::new(AtomicBool::new(true));
        let wake_ctx = cc.egui_ctx.clone();
        let wake_flag = repaint_alive.clone();
        std::thread::Builder::new()
            .name("webdesk-repaint".into())
            .spawn(move || {
                while wake_flag.load(Ordering::Relaxed) {
                    wake_ctx.request_repaint();
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
            .ok();

        if opts.no_start {
            log_info!("已按 --no-start 跳过自动启动服务");
        } else {
            controller.start(&cfg);
        }

        Self {
            opts,
            applied: cfg.clone(),
            cfg,
            controller,
            tray,
            status: Status::default(),
            tab: Tab::Service,
            visible,
            really_quit: false,
            confirm_exit: false,
            password_new: String::new(),
            password_confirm: String::new(),
            password_error: None,
            show_password: false,
            password_warning_dismissed: false,
            logs: Vec::new(),
            log_revision: 0,
            last_log_check: Instant::now(),
            toast: None,
            browser_opened: false,
            repaint_alive,
        }
    }

    fn notify(&mut self, text: impl Into<String>, is_error: bool) {
        self.toast = Some((text.into(), Instant::now(), is_error));
        if is_error {
            log_warn!("{}", self.toast.as_ref().unwrap().0);
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.visible = true;
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        self.visible = false;
        log_info!("已最小化到系统托盘，服务继续在后台运行");
    }

    fn apply_settings(&mut self) {
        self.cfg = self.cfg.clone().clamped();
        if self.cfg.port == 0 {
            self.notify("端口必须在 1-65535 之间", true);
            return;
        }
        match self.cfg.save(self.opts.config_path.as_deref()) {
            Ok(path) => log_info!("配置已保存: {}", path.display()),
            Err(e) => {
                self.notify(format!("保存配置失败: {e}"), true);
                return;
            }
        }

        if self.cfg.autostart != self.applied.autostart {
            match config::set_autostart(self.cfg.autostart) {
                Ok(msg) => {
                    log_info!("{msg}");
                    self.notify(msg, false);
                }
                Err(e) => self.notify(e, true),
            }
        }

        let running = matches!(self.status.kind, StateKind::Running | StateKind::Starting);
        if running {
            if self.cfg.needs_restart(&self.applied) {
                self.controller.restart(&self.cfg);
                self.notify("设置已保存，正在重启服务…", false);
            } else {
                // 端口/性能未变：仅热更新密码，连接不中断
                self.controller.set_password(
                    self.cfg.password_hash.clone(),
                    self.cfg.password_plain.clone(),
                );
                self.notify("设置已保存并即时生效", false);
            }
        } else {
            self.notify("设置已保存，下次启动时生效", false);
        }
        self.applied = self.cfg.clone();
    }

    fn set_password(&mut self) {
        self.password_error = None;
        if self.password_new.is_empty() {
            self.password_error = Some("请输入新密码".into());
            return;
        }
        if self.password_new != self.password_confirm {
            self.password_error = Some("两次输入的密码不一致".into());
            return;
        }
        let hash = config::hash_password(&self.password_new);
        self.cfg.password_hash = hash;
        self.cfg.password_plain.clear();
        self.password_new.clear();
        self.password_confirm.clear();
        self.apply_settings();
        self.notify("访问密码已设置（PBKDF2-SHA256 加盐存储）", false);
    }

    fn open_browser(&mut self) {
        let url = format!("http://127.0.0.1:{}/", self.status.port);
        match config::open_with_system(&url) {
            Ok(()) => log_info!("已打开浏览器: {url}"),
            Err(e) => self.notify(format!("打开浏览器失败: {e}"), true),
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.really_quit = true;
        self.confirm_exit = false;
        self.repaint_alive.store(false, Ordering::Relaxed);
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
}

impl App for WebDeskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        ctx.request_repaint_after(Duration::from_millis(200));
        self.status = self.controller.status();

        // ── 托盘事件 ────────────────────────────────────────────────
        if let Some(tray) = &self.tray {
            for action in tray.poll() {
                match action {
                    TrayAction::Show => self.show_window(ctx),
                    TrayAction::Start => {
                        self.controller.start(&self.cfg);
                        self.notify("正在启动服务…", false);
                    }
                    TrayAction::Stop => {
                        self.controller.stop();
                        self.notify("正在停止服务…", false);
                    }
                    TrayAction::Quit => self.quit(ctx),
                }
            }
        }

        // ── 关闭窗口 → 最小化到托盘 ────────────────────────────────
        if ctx.input(|i| i.viewport().close_requested()) && !self.really_quit {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if self.cfg.minimize_to_tray && self.tray.is_some() {
                self.hide_window(ctx);
            } else {
                self.confirm_exit = true;
            }
        }

        // ── 服务启动后按需自动打开浏览器 ───────────────────────────
        if !self.browser_opened
            && self.status.kind == StateKind::Running
            && (self.cfg.open_browser || self.opts.open_browser)
        {
            self.browser_opened = true;
            self.open_browser();
        }

        // ── 日志刷新 ────────────────────────────────────────────────
        if self.last_log_check.elapsed() > Duration::from_millis(300) {
            self.last_log_check = Instant::now();
            let rev = crate::logger::revision();
            if rev != self.log_revision {
                self.log_revision = rev;
                self.logs = crate::logger::lines();
            }
        }

        // ── 顶部状态栏 ──────────────────────────────────────────────
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("🖥 WebDesk").size(20.0).strong().color(Color32::from_rgb(124, 158, 245)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (dot, label, color) = match self.status.kind {
                        StateKind::Running => ("●", "运行中", Color32::from_rgb(63, 185, 80)),
                        StateKind::Starting => ("●", "启动中", Color32::from_rgb(240, 173, 78)),
                        StateKind::Error => ("●", "出错", Color32::from_rgb(248, 81, 73)),
                        StateKind::Stopped => ("●", "未运行", Color32::GRAY),
                    };
                    ui.label(RichText::new(format!("{dot} {label}")).color(color).strong());
                    ui.label(RichText::new(format!("端口 {} · 客户端 {}", self.status.port, self.status.clients)).color(Color32::GRAY));
                });
            });
            ui.add_space(4.0);
        });

        // ── 未设置密码：可关闭的提示条（不再强制弹窗）──────────────
        if !self.cfg.auth_required() && !self.password_warning_dismissed {
            egui::TopBottomPanel::top("password_warning").show(ctx, |ui| {
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("⚠ 为保证安全，请设置访问密码！")
                            .color(Color32::from_rgb(248, 81, 73))
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("✕").on_hover_text("关闭提示").clicked() {
                            self.password_warning_dismissed = true;
                        }
                    });
                });
                ui.add_space(5.0);
            });
        }

        // ── 底部工具栏 ──────────────────────────────────────────────
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("💾 保存并应用").clicked() {
                    self.apply_settings();
                }
                let running = matches!(self.status.kind, StateKind::Running | StateKind::Starting);
                if ui.add_enabled(!running, egui::Button::new("▶ 启动")).clicked() {
                    self.controller.start(&self.cfg);
                    self.notify("正在启动服务…", false);
                }
                if ui.add_enabled(running, egui::Button::new("■ 停止")).clicked() {
                    self.controller.stop();
                    self.notify("正在停止服务…", false);
                }
                if ui.button("🔄 重启").clicked() {
                    self.controller.restart(&self.cfg);
                    self.notify("正在重启服务…", false);
                }
                if ui.button("⬇ 最小化到托盘").clicked() && self.tray.is_some() {
                    self.hide_window(ctx);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("退出").clicked() {
                        self.confirm_exit = true;
                    }
                    if let Some((text, at, is_error)) = &self.toast {
                        if at.elapsed() < Duration::from_secs(4) {
                            let color = if *is_error {
                                Color32::from_rgb(248, 81, 73)
                            } else {
                                Color32::from_rgb(63, 185, 80)
                            };
                            ui.label(RichText::new(text).color(color));
                        }
                    }
                });
            });
            ui.add_space(4.0);
        });

        // ── 主体 ────────────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Service, "服务");
                ui.selectable_value(&mut self.tab, Tab::Performance, "性能");
                ui.selectable_value(&mut self.tab, Tab::General, "常规");
                ui.selectable_value(&mut self.tab, Tab::Logs, "日志");
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.tab {
                    Tab::Service => self.service_tab(ui),
                    Tab::Performance => self.performance_tab(ui),
                    Tab::General => self.general_tab(ui),
                    Tab::Logs => self.logs_tab(ui),
                });
        });

        // ── 退出确认 ────────────────────────────────────────────────
        if self.confirm_exit {
            egui::Window::new("确认退出")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label("退出后远程桌面服务将停止，确定退出 WebDesk 吗？");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("确定退出").clicked() {
                            self.quit(ctx);
                        }
                        if ui.button("取消").clicked() {
                            self.confirm_exit = false;
                        }
                    });
                });
        }
    }
}

impl WebDeskApp {
    fn service_tab(&mut self, ui: &mut egui::Ui) {
        // ── 访问密码（必填，放在首页） ──────────────────────────────
        ui.heading("访问密码");
        ui.add_space(6.0);
        let enabled = self.cfg.auth_required();
        ui.horizontal(|ui| {
            ui.label("当前状态：");
            ui.label(
                RichText::new(if enabled { "🔒 已启用" } else { "🔓 未设置" })
                    .color(if enabled {
                        Color32::from_rgb(63, 185, 80)
                    } else {
                        Color32::from_rgb(248, 81, 73)
                    })
                    .strong(),
            );
        });
        if !enabled {
            ui.label(
                RichText::new("为保证安全，未设置访问密码时客户端将无法连接")
                    .color(Color32::from_rgb(248, 81, 73))
                    .strong(),
            );
        }
        ui.add_space(8.0);
        egui::Grid::new("password_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("新密码");
                ui.add(
                    egui::TextEdit::singleline(&mut self.password_new)
                        .password(!self.show_password)
                        .hint_text(if enabled { "留空表示不修改" } else { "请设置访问密码" })
                        .desired_width(260.0),
                );
                ui.end_row();
                ui.label("确认密码");
                ui.add(
                    egui::TextEdit::singleline(&mut self.password_confirm)
                        .password(!self.show_password)
                        .desired_width(260.0),
                );
                ui.end_row();
            });
        ui.checkbox(&mut self.show_password, "显示密码");
        ui.add_space(8.0);
        if ui.button("设置 / 修改密码").clicked() {
            self.set_password();
        }
        if let Some(err) = &self.password_error {
            ui.label(RichText::new(err).color(Color32::from_rgb(248, 81, 73)));
        }
        ui.add_space(8.0);
        ui.label("· 密码以 PBKDF2-SHA256（120000 次迭代 + 随机盐）哈希保存，不存明文");
        ui.label("· 未通过验证的连接不会收到任何画面，也不会被允许注入输入");
        ui.label("· 连续 8 次验证失败会断开连接，并采用指数退避抵御暴力破解");

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);

        // ── 服务设置 ────────────────────────────────────────────────
        ui.heading("服务设置");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("监听端口");
            ui.add(egui::DragValue::new(&mut self.cfg.port).range(1..=65535).speed(1.0));
            ui.label(RichText::new("1 - 65535").color(Color32::GRAY));
        });
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("访问地址");
            if ui.small_button("刷新").clicked() {
                self.status.urls = config::local_addresses(self.status.port);
            }
        });
        if self.status.urls.is_empty() {
            ui.label(RichText::new("服务启动后在此显示可用的访问地址").color(Color32::GRAY));
        }
        for url in self.status.urls.clone() {
            ui.horizontal(|ui| {
                ui.monospace(&url);
                if ui.small_button("复制").clicked() {
                    ui.output_mut(|o| o.copied_text = url.clone());
                    self.notify("地址已复制到剪贴板", false);
                }
            });
        }
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(RichText::new(&self.status.message).color(
            if self.status.kind == StateKind::Error {
                Color32::from_rgb(248, 81, 73)
            } else {
                Color32::GRAY
            },
        ));
        ui.add_space(6.0);
        ui.label(RichText::new("提示：修改端口 / 性能参数后点击「保存并应用」，服务会自动重启；修改密码则即时生效，无需重启。").color(Color32::GRAY));
    }

    fn performance_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("画面与性能");
        ui.add_space(8.0);
        ui.add(egui::Slider::new(&mut self.cfg.fps, 1..=60).text("最大帧率 FPS"));
        ui.label(RichText::new("局域网建议 20-30，广域网建议 10-15").color(Color32::GRAY));
        ui.add_space(8.0);
        ui.add(egui::Slider::new(&mut self.cfg.tile_size, 16..=256).text("差分瓦片大小（像素）"));
        ui.label(RichText::new("越小越精细，但编码开销越大；推荐 64").color(Color32::GRAY));
        ui.add_space(8.0);
        ui.add(egui::Slider::new(&mut self.cfg.quality, 10..=95).text("JPEG 压缩质量"));
        ui.label(RichText::new("越高画质越好、带宽占用越大；推荐 75").color(Color32::GRAY));
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label("传输方式：仅发送发生变化的瓦片（容差哈希），JPEG 编码后经 WebSocket 下发。");
        ui.label("画面为 HTTP 明文传输（ws://），请勿在不可信网络中承载敏感内容。");
    }

    fn general_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("常规设置");
        ui.add_space(8.0);
        ui.checkbox(&mut self.cfg.autostart, "开机自动启动（当前用户，无需管理员权限）");
        ui.checkbox(&mut self.cfg.minimize_to_tray, "关闭窗口时最小化到系统托盘（服务继续运行）");
        ui.checkbox(&mut self.cfg.start_minimized, "启动后直接最小化到托盘");
        ui.checkbox(&mut self.cfg.open_browser, "服务启动后自动打开浏览器");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("重新检测开机自启").clicked() {
                self.cfg.autostart = config::autostart_enabled();
                self.applied.autostart = self.cfg.autostart;
                self.notify(format!("系统启动项状态：{}", if self.cfg.autostart { "已启用" } else { "未启用" }), false);
            }
            if ui.button("打开配置目录").clicked() {
                let dir = config::config_path(false).parent().map(|p| p.to_path_buf()).unwrap_or_else(|| config::app_dir());
                let _ = config::open_with_system(&dir.display().to_string());
            }
            if ui.button("打开日志目录").clicked() {
                let dir = config::user_data_dir();
                let _ = config::open_with_system(&dir.display().to_string());
            }
        });
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(RichText::new(format!("配置文件：{}", config::config_path(false).display())).color(Color32::GRAY));
        ui.label(RichText::new(format!("日志文件：{}", crate::logger::path_string())).color(Color32::GRAY));
        ui.label(RichText::new(format!("程序目录：{}", config::app_dir().display())).color(Color32::GRAY));
    }

    fn logs_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("运行日志");
            if ui.small_button("清空").clicked() {
                crate::logger::clear();
                self.logs.clear();
            }
            if ui.small_button("刷新").clicked() {
                self.logs = crate::logger::lines();
            }
            ui.label(RichText::new(crate::logger::path_string()).color(Color32::GRAY));
        });
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let start = self.logs.len().saturating_sub(300);
                for line in &self.logs[start..] {
                    ui.monospace(line);
                }
            });
    }
}

impl Drop for WebDeskApp {
    fn drop(&mut self) {
        self.repaint_alive.store(false, Ordering::Relaxed);
        log_info!("WebDesk 正在退出");
    }
}

/// headless 模式使用：等待 Ctrl+C 后停止服务。
pub async fn wait_for_ctrl_c(controller: &ServerController) {
    match tokio::signal::ctrl_c().await {
        Ok(()) => log_info!("收到中断信号，正在退出"),
        Err(e) => log_error!("无法监听 Ctrl+C: {e}"),
    }
    controller.shutdown();
}
