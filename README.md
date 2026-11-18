# WebDesk

WebDesk 是一个用 Rust 实现的远程桌面服务端：本机运行一个桌面程序，浏览器打开网页即可实时查看并控制这台电脑。

## 特性

- 差分传输：只发送发生变化的瓦片区域，降低带宽占用
- 浏览器访问：控制端无需安装任何软件
- 密码保护：PBKDF2-SHA256 加盐哈希，未认证连接不下发画面、不允许注入输入
- 完整输入：鼠标、滚轮、键盘、组合键、中文与 emoji 文本注入
- 桌面 GUI：服务控制、密码管理、性能调节、运行日志与系统托盘
- 单文件部署：`cargo build --release` 产出单个 `webdesk.exe`
- 无 WebView 依赖：GUI 使用 egui/glow（OpenGL）渲染

## 系统要求

- 服务端：Windows 10/11（屏幕捕获与输入注入目前仅实现 Windows 后端）
- 构建环境：Rust 1.75 或更高版本
- 客户端：支持 WebSocket 与 Canvas 的现代浏览器

## 构建与运行

```bash
cargo build --release
```

产物位于 `target\release\webdesk.exe`，双击即以图形界面启动并自动开启服务。

命令行选项：

| 选项 | 说明 |
|------|------|
| `--minimized` | 启动后直接最小化到系统托盘 |
| `--no-start` | 启动 GUI 但不自动启动服务 |
| `--headless` | 无界面模式，`Ctrl+C` 退出 |
| `--port <N>` | 临时覆盖监听端口 |
| `--config <PATH>` | 指定 `config.ini` 路径 |
| `--password <PWD>` | 临时指定访问密码（不写入配置文件） |
| `--open-browser` | 服务启动后打开浏览器 |
| `--verbose` | 输出调试日志 |
| `-h, --help` | 显示帮助 |

## 配置

配置文件为程序目录或用户数据目录下的 `config.ini`，也可直接在 GUI 中修改并保存。

```ini
[server]
port = 8765
# PBKDF2-SHA256 哈希，留空表示不需要密码
password_hash =

[performance]
fps = 15
tile_size = 64
quality = 75

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
| `fps` | 最大帧率（1-60） | `15` |
| `tile_size` | 差分瓦片大小，像素（16-256） | `64` |
| `quality` | JPEG 质量（10-95） | `75` |
| `autostart` | 开机自动启动（当前用户） | `false` |
| `minimize_to_tray` | 关闭窗口时最小化到托盘 | `true` |
| `start_minimized` | 启动后直接最小化到托盘 | `false` |
| `open_browser` | 服务启动后自动打开浏览器 | `false` |

旧版配置中的 `host`、`ipv4`、`ipv6` 键会被忽略：服务始终双栈监听。

## 使用

1. 启动 `webdesk.exe`，通过 GUI 或 `config.ini` 设置端口与密码
2. 浏览器访问界面中显示的地址（IPv6 地址需带方括号，如 `http://[::1]:8765/`）
3. 输入密码后即可看到远程画面并进行控制

Web 界面快捷键：`F11` 切换全屏，`Ctrl+V` 粘贴文本到远程桌面。

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
    ├── server.rs        # HTTP + WebSocket 服务与双栈绑定
    ├── capture.rs       # 屏幕捕获、瓦片差分、JPEG 编码
    ├── input.rs         # 键鼠输入注入与按键防粘滞
    ├── protocol.rs      # 客户端消息协议
    ├── config.rs        # 配置、密码哈希、开机自启、访问地址
    ├── logger.rs        # 内存 + 文件日志
    ├── tray.rs          # 系统托盘
    └── globals.rs       # 全局运行开关
```

## 安全建议

1. 设置强密码，避免无人值守时被随意接入
2. 仅用于局域网或可信网络，公网访问请配合 VPN
3. 画面与输入均为 HTTP/WebSocket 明文传输（`ws://`），请勿在不可信网络中承载敏感内容
4. 通过防火墙限制可访问该端口的来源地址

## 许可证

MIT。本软件仅供合法用途，使用者应遵守当地法律法规，不得用于未经授权的远程访问。
