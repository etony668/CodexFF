//! Codex 桌面端检测 / 一键安装 (官方 DMG: 下载 → 挂载 → 复制到 /Applications)。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

/// 安装进度事件 (percent < 0 表示总大小未知, 前端显示不定进度)。
#[derive(Serialize, Clone)]
pub struct InstallProgress {
    pub phase: String,
    pub percent: f64,
    pub message: String,
}

/// Codex 桌面端是否已安装。
///
/// 当前 Codex 桌面端有两种形态: 独立 Codex.app, 以及集成在 ChatGPT.app
/// 里的 Codex Framework (进程名带空格, 不在 /Codex.app/ 路径下), 所以
/// 除了标准路径外还要检查 ChatGPT.app / Codex 数据目录 / 运行进程。
pub fn is_codex_desktop_installed() -> bool {
    let home = dirs::home_dir().unwrap_or_default();
    let candidates = [
        PathBuf::from("/Applications/Codex.app"),
        home.join("Applications/Codex.app"),
        PathBuf::from("/Applications/ChatGPT.app"),
        home.join("Applications/ChatGPT.app"),
    ];
    if candidates.iter().any(|p| p.exists()) {
        return true;
    }
    // Codex 桌面端数据目录 (ChatGPT 集成形态运行后也会创建)
    if home.join("Library/Application Support/Codex").exists() {
        return true;
    }
    // 运行中的进程: Codex (含带空格进程名) / ChatGPT / 命令行含 codex 的辅助进程
    for args in [
        &["-x", "Codex"][..],
        &["-x", "ChatGPT"][..],
        &["-f", "-i", "codex"][..],
    ] {
        if let Ok(out) = Command::new("pgrep").args(args).output() {
            if out.status.success() {
                return true;
            }
        }
    }
    false
}

fn client_with_proxy() -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(1800));
    if let Some(proxy) = crate::official_quota::effective_proxy_url() {
        let p = reqwest::Proxy::all(&proxy).map_err(|e| format!("代理配置无效: {e}"))?;
        builder = builder.proxy(p);
    }
    builder.build().map_err(|e| format!("网络客户端初始化失败: {e}"))
}

fn fmt_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn detach(mount: &Path) {
    let _ = Command::new("/usr/bin/hdiutil").arg("detach").arg(mount).output();
}

/// 当前 macOS 主/次版本 (sw_vers -productVersion), 失败返回 None。
fn macos_version() -> Option<(u32, u32)> {
    let out = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.trim().split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts
        .next()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);
    Some((major, minor))
}

/// 根卷可用磁盘字节数 (statvfs), 失败返回 0 (调用方跳过检查)。
fn free_disk_bytes() -> u64 {
    use std::ffi::CString;
    let Ok(cpath) = CString::new("/") else {
        return 0;
    };
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut vfs) };
    if rc != 0 {
        return 0;
    }
    (vfs.f_bavail as u64).saturating_mul(vfs.f_frsize as u64)
}

fn cleanup(tmp: &Path, mount: &Path) {
    let _ = detach(mount);
    let _ = std::fs::remove_file(tmp);
    let _ = std::fs::remove_dir_all(mount);
}

/// 下载官方 DMG 并安装 Codex 桌面端到 /Applications, 进度通过 emit 上抛。
pub async fn install_desktop(
    emit: impl Fn(InstallProgress) + Send + Sync,
) -> Result<(), String> {
    if is_codex_desktop_installed() {
        return Err("已检测到 Codex 桌面端，无需重复安装".into());
    }

    // 下载前体检: Codex 桌面端要求 macOS 14+; 安装包约 618MB, 解包后需要
    // 更多空间, 提前检查避免下载到一半才发现环境不满足。
    if let Some((major, minor)) = macos_version() {
        if major < 14 {
            return Err(format!(
                "Codex 桌面端需要 macOS 14 或更高版本（当前 {major}.{minor}）"
            ));
        }
    }
    let free = free_disk_bytes();
    if free > 0 && free < 3 * 1024 * 1024 * 1024 {
        return Err(format!(
            "磁盘可用空间不足（当前 {:.1} GB，建议至少 3 GB）",
            free as f64 / (1024.0 * 1024.0 * 1024.0)
        ));
    }

    let url = if std::env::consts::ARCH == "x86_64" {
        "https://persistent.oaistatic.com/codex-app-prod/Codex-latest-x64.dmg"
    } else {
        "https://persistent.oaistatic.com/codex-app-prod/Codex.dmg"
    };

    emit(InstallProgress {
        phase: "下载".into(),
        percent: 0.0,
        message: "正在下载官方安装包…".into(),
    });

    let client = client_with_proxy()?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("下载失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载失败: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);

    let tmp = std::env::temp_dir().join(format!(
        "codexff-codex-{}-{}.dmg",
        std::process::id(),
        chrono::Local::now().format("%Y%m%d%H%M%S")
    ));
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("无法创建临时文件: {e}"))?;
    let mut resp = resp;
    let mut written: u64 = 0;
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("下载中断: {e}")
            })?;
        let Some(chunk) = chunk else { break };
        file.write_all(&chunk)
            .map_err(|e| format!("写入失败: {e}"))?;
        written += chunk.len() as u64;
        let percent = if total > 0 {
            (written as f64 / total as f64) * 90.0
        } else {
            -1.0
        };
        emit(InstallProgress {
            phase: "下载".into(),
            percent,
            message: format!(
                "下载中 {} / {}",
                fmt_mb(written),
                if total > 0 { fmt_mb(total) } else { "未知".into() }
            ),
        });
    }
    drop(file);

    if written < 10 * 1024 * 1024 {
        let _ = std::fs::remove_file(&tmp);
        return Err("下载文件异常（过小），请重试".into());
    }

    emit(InstallProgress {
        phase: "安装".into(),
        percent: 93.0,
        message: "正在挂载安装包…".into(),
    });
    let mount = std::env::temp_dir().join(format!("codexff-codex-mount-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&mount);
    std::fs::create_dir_all(&mount).map_err(|e| format!("无法创建挂载点: {e}"))?;
    let attach = Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount)
        .arg(&tmp)
        .output()
        .map_err(|e| format!("无法运行 hdiutil: {e}"))?;
    if !attach.status.success() {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_dir_all(&mount);
        return Err(format!(
            "安装包挂载失败: {}",
            String::from_utf8_lossy(&attach.stderr).trim()
        ));
    }

    let src = mount.join("Codex.app");
    if !src.exists() {
        cleanup(&tmp, &mount);
        return Err("安装包中未找到 Codex.app".into());
    }
    if Path::new("/Applications/Codex.app").exists() {
        cleanup(&tmp, &mount);
        return Err("/Applications/Codex.app 已存在，无需重复安装".into());
    }

    emit(InstallProgress {
        phase: "安装".into(),
        percent: 96.0,
        message: "正在复制到应用程序…".into(),
    });
    let copy = Command::new("/usr/bin/ditto")
        .arg(&src)
        .arg("/Applications/Codex.app")
        .status()
        .map_err(|e| {
            cleanup(&tmp, &mount);
            format!("无法运行 ditto: {e}")
        })?;
    cleanup(&tmp, &mount);
    if !copy.success() {
        return Err("复制到应用程序失败，请检查磁盘权限".into());
    }
    if !Path::new("/Applications/Codex.app").exists() {
        return Err("安装失败：未找到目标应用".into());
    }
    // 移除隔离属性, 避免首次打开被 Gatekeeper 拦截
    let _ = Command::new("xattr")
        .args(["-dr", "com.apple.quarantine", "/Applications/Codex.app"])
        .status();

    emit(InstallProgress {
        phase: "完成".into(),
        percent: 100.0,
        message: "Codex 桌面端已安装到应用程序".into(),
    });
    Ok(())
}
