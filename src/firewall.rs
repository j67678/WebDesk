//! Windows 防火墙例外：把当前程序本身加入「允许入站」规则，避免局域网客户端被拦截。
//!
//! 添加防火墙规则需要管理员权限：成功写入日志；权限不足只告警，不影响本机访问。
//! 非 Windows 平台为空实现。

use std::sync::atomic::{AtomicBool, Ordering};

/// 进程内只尝试一次：服务重启、改端口都无需重复配置（规则按程序放行全部端口）。
static ENSURED: AtomicBool = AtomicBool::new(false);

pub fn ensure_exception() {
    if ENSURED.swap(true, Ordering::Relaxed) {
        return;
    }
    #[cfg(windows)]
    {
        // netsh 是阻塞调用，放到独立线程，避免占用 GUI / tokio 线程
        let _ = std::thread::Builder::new()
            .name("webdesk-firewall".into())
            .spawn(|| match add_rule() {
                Ok(()) => {}
                Err(e) => crate::log_warn!("{e}"),
            });
    }
}

#[cfg(windows)]
/// 规则名刻意独立：系统首次弹窗生成的规则名是 exe 名（如 webdesk），
/// 按名字删除会误删其它副本的规则，这里用独立名称避免互相干扰。
const RULE_NAME: &str = "WebDesk-Server";

#[cfg(windows)]
fn netsh_exe() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    std::path::PathBuf::from(root)
        .join("System32")
        .join("netsh.exe")
}

#[cfg(windows)]
fn run_netsh(args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW：GUI 模式下执行 netsh 不闪黑框
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(netsh_exe())
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
}

#[cfg(windows)]
fn add_rule() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("无法获取程序路径，跳过防火墙例外配置: {e}"))?;
    let exe_str = exe.to_string_lossy().to_string();
    let exe_lower = exe_str.to_lowercase();

    // 已有入站规则覆盖当前程序（含系统首次启动弹窗生成的 webdesk 规则）就不重复添加
    if let Ok(out) = run_netsh(&[
        "advfirewall",
        "firewall",
        "show",
        "rule",
        "name=all",
        "dir=in",
        "verbose",
    ]) {
        if out.status.success()
            && String::from_utf8_lossy(&out.stdout)
                .to_lowercase()
                .contains(&exe_lower)
        {
            crate::log_debug!("检测到已有入站规则放行本程序: {exe_str}");
            return Ok(());
        }
    }

    // 程序被移动或重编译后旧规则会指向失效路径，先清理自己建的旧规则再重建
    let rule = format!("name={RULE_NAME}");
    let _ = run_netsh(&[
        "advfirewall",
        "firewall",
        "delete",
        "rule",
        &rule,
    ]);

    let program = format!("program=\"{exe_str}\"");
    let out = run_netsh(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &rule,
        "dir=in",
        "action=allow",
        &program,
        "enable=yes",
        "profile=any",
    ])
    .map_err(|e| format!("执行 netsh 失败，未能添加防火墙例外: {e}"))?;
    if out.status.success() {
        crate::log_info!("已添加 Windows 防火墙例外（允许入站）: {exe_str}");
        return Ok(());
    }

    // netsh 的错误信息随系统语言变化，原文透传到日志便于排查
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        stderr
    };
    Err(format!(
        "未能添加 Windows 防火墙例外（需要管理员权限）: {detail}；\
         若局域网客户端无法连接，请以管理员身份运行本程序，或手动在 Windows 防火墙中允许该程序"
    ))
}
