# WebDesk

WebDesk 是一个用 Rust 实现的远程桌面服务端：本机运行一个桌面程序，浏览器打开网页即可实时查看并控制这台电脑。

## 特性

- WebRTC 视频传输：H.264 编码 + RTP/SRTP，浏览器原生抖动缓冲、NACK 与拥塞控制
- 仅 IP 直连：ICE 只收集 host candidate（无 STUN/TURN），面向局域网/可信网络
- 弱网自适应：客户端依据 `getStats()` 丢包率动态建议码率，并请求 IDR 关键帧快速恢复
- 清晰度底线：码率有服务端强制下限（`min_bitrate_kbps`，默认 1500），弱网再差也不会压到字体模糊
- 一次编码多端共享：全服务单捕获线程 + 单编码线程，多个观看者复用同一份 H.264 帧
- 浏览器访问：控制端无需安装任何软件
- 密码保护：PBKDF2-SHA256 加盐哈希，未认证连接不建立 WebRTC、不允许注入输入
- 完整输入：鼠标、滚轮、键盘、组合键、中文与 emoji 文本注入（仍走 WebSocket）
- 桌面 GUI：服务控制、密码管理、性能调节、运行日志与系统托盘
- 单文件部署：`cargo build --release` 产出单个 `webdesk.exe`
- 单实例运行：重复启动不会重复监听端口，而是唤起已运行实例的主界面
- 无 WebView 依赖：GUI 使用 egui/glow（OpenGL）渲染

## 系统要求

- 服务端：Windows 10/11（屏幕捕获与输入注入目前仅实现 Windows 后端）
- 构建环境：Rust 1.85 或更高版本（`rusty_h264-encoder` 要求）
- 客户端：支持 WebRTC / H.264 的现代浏览器（Chrome 90+、Edge 90+、Firefox 90+、Safari 15+）

## 构建与运行

```bash
cargo build --release
```

产物位于 `target\release\webdesk.exe`，双击即以图形界面启动并自动开启服务。

命令行选项：

| 选项 | 说明 |
|------|------|
| `--minimized` | 启动后直接最小化到系统托盘 |
| `--headless` | 无界面模式，`Ctrl+C` 退出 |
| `--port <N>` | 临时覆盖监听端口 |
| `--config <PATH>` | 指定 `config.ini` 路径 |
| `--password <PWD>` | 临时指定访问密码（不写入配置文件） |
| `--verbose` | 输出调试日志 |
| `--no-elevate` | 不自动请求管理员权限（将无法操作提权窗口） |
| `-h, --help` | 显示帮助 |

## 配置

配置文件为程序目录或用户数据目录下的 `config.ini`，也可直接在 GUI 中修改并保存。

```ini
[server]
port = 8765
# PBKDF2-SHA256 哈希，留空表示不需要密码
password_hash =

[performance]
fps = 30
# 旧版 JPEG 差分遗留项，WebRTC 迁移后不再生效
tile_size = 64
quality = 75
# H.264 目标码率 kbps（300-20000）
bitrate_kbps = 2500
# 弱网码率下限 kbps；过低会导致字体模糊，服务端强制不下探
min_bitrate_kbps = 1500

[general]
autostart = false
minimize_to_tray = true
start_minimized = false
open_browser = false
```

| 配置项 | 说明 | 默认值 |
|--------|------|--------|
| `port` | 监听端口 | `8765` |
| `password_hash` | 访问密码哈希，留空表示无密码 | 空 |
| `password` | 旧版明文密码，兼容读取，建议改用 `password_hash` | 空 |
| `fps` | 最大帧率 / 编码节拍（1-60）；局域网拖动窗口建议 30 | `30` |
| `tile_size` | 旧版遗留项，已失效 | `64` |
| `quality` | 旧版遗留项，已失效 | `75` |
| `bitrate_kbps` | H.264 目标码率（300-20000），弱网下可自动下调 | `2500` |
| `min_bitrate_kbps` | 弱网码率下限（300-20000），低于此值字体模糊；服务端强制生效；若高于目标码率会自动提升目标码率 | `1500` |
| `autostart` | 开机自动启动（当前用户） | `false` |
| `minimize_to_tray` | 关闭窗口时最小化到托盘 | `true` |
| `start_minimized` | 启动后直接最小化到托盘 | `false` |

旧版配置中的 `host`、`ipv4`、`ipv6` 键会被忽略：服务始终双栈监听。

## 使用

1. 启动 `webdesk.exe`，通过 GUI 或 `config.ini` 设置端口与密码
2. 浏览器访问界面中显示的地址（IPv6 地址需带方括号，如 `http://[::1]:8765/`）
3. 输入密码后即可看到远程画面并进行控制

Web 界面快捷键：`F11` 切换全屏，`Ctrl+V` 粘贴文本到远程桌面。

## 操作提权窗口（任务管理器等）

Windows 的 UIPI（用户界面特权隔离）会阻止普通权限进程向更高完整性级别的窗口注入输入，
因此未提权的 WebDesk 能看到任务管理器等窗口，却点不动、也敲不进键盘。

- 程序启动时会检查自身令牌；未提权时通过 `runas` 自动重启并弹出一次 UAC 确认。
- 拒绝 UAC 或以 `--no-elevate` 启动时会降级运行：普通窗口仍可控制，提权窗口不可操作。
- 若此前已有普通权限实例在运行，请先退出它再启动，否则新进程会被单实例检查拦截并唤起旧实例。
- 即使已提权，也**无法**操作 UAC 同意对话框、锁屏界面等位于 Winlogon 安全桌面的窗口：
  `SendInput` 只能作用于当前输入桌面，且 Windows 不允许合成 `Ctrl+Alt+Delete` 安全注意序列。
  这类界面需要在目标机本地操作，或改用能注入系统桌面的方案。

提权后服务端以管理员权限监听网络，请务必设置强密码并仅在可信网络使用。

## 项目结构

```
.
├── Cargo.toml
├── config.ini
├── assets/
│   └── client.html      # 内嵌的 Web 客户端页面
└── src/
    ├── main.rs          # 入口、命令行参数
    ├── gui.rs           # egui 桌面界面
    ├── server.rs        # HTTP + WS 信令、双栈绑定、捕获/编码广播枢纽
    ├── rtc.rs           # WebRTC PeerConnection（host 直连、H.264 视频轨）
    ├── encoder.rs       # BGRA -> I420 -> H.264（码控 / 关键帧）
    ├── capture.rs       # Windows 屏幕捕获（DXGI / GDI）
    ├── input.rs         # 键鼠输入注入与按键防粘滞
    ├── privilege.rs     # 管理员权限自查与自动提权
    ├── protocol.rs      # 客户端消息协议（含 WebRTC 信令字段）
    ├── config.rs        # 配置、密码哈希、开机自启、访问地址
    ├── single_instance.rs # 单实例约束与唤起已运行实例主窗口
    ├── logger.rs        # 内存 + 文件日志
    ├── tray.rs          # 系统托盘
    └── globals.rs       # 全局运行开关
```

## 安全建议

1. 设置强密码，避免无人值守时被随意接入
2. 仅用于局域网或可信网络，公网访问请配合 VPN
3. 画面经 WebRTC SRTP 加密；信令、密码与输入仍走 WebSocket 明文（`ws://`），请仅在可信网络使用
4. 本版本仅收集 host ICE candidate（不配置 STUN/TURN），跨 NAT 场景无法连接，这是刻意约束
5. 通过防火墙限制可访问该端口的来源地址

## WebRTC 迁移说明

画面传输已从「WebSocket + JPEG 瓦片差分」迁移为：

- **视频**：Windows DXGI/GDI 捕获 → 纯 Rust H.264 编码（Annex-B / ABR 码控）→ WebRTC RTP/SRTP
- **连接**：浏览器发 offer，服务端回 answer；双方均只收集 host candidate，实现 IP 直连
- **信令**：复用原有 `/ws`，新增 `offer` / `answer` / `keyframe` / `adapt` 消息，认证与输入保持不变
- **弱网**：浏览器侧 `getStats()` 统计丢包率，超阈值请求 IDR 并下调码率；码率范围锁定为 `[min_bitrate_kbps, bitrate_kbps]`，服务端在 `adapt` 入口与编码线程各钳制一次，无丢包时也不会一路抬到高码率反复重建编码器，保证桌面字体始终可读
- **停滞自救**：客户端连续 4 秒无新增解码帧会自动请求 IDR；服务端发布帧时使用自身代次，避免捕获/编码线程启停不同步导致首帧后冻结
- **多端**：捕获与编码各仅一个线程，所有观看者共享同一份编码帧后再各自写入 RTP 轨

旧版 `tile_size` / `quality` 配置保留以兼容旧 `config.ini`，但已不再参与画面处理。

## 许可证

MIT。本软件仅供合法用途，使用者应遵守当地法律法规，不得用于未经授权的远程访问。
