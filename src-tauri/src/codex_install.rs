//! Codex 桌面端 / CLI 检测、版本检查和一键安装。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

const DESKTOP_ARM_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/Codex.dmg";
const DESKTOP_X64_URL: &str =
    "https://persistent.oaistatic.com/codex-app-prod/Codex-latest-x64.dmg";
const DESKTOP_APPCAST_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/appcast.xml";
const CLI_LATEST_URL: &str = "https://registry.npmjs.org/@openai/codex/latest";

#[derive(Serialize, Clone)]
pub struct InstallProgress {
    pub component: String,
    pub phase: String,
    pub percent: f64,
    pub message: String,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct ComponentStatus {
    pub installed: bool,
    pub current_version: Option<String>,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub source: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct CodexInstallStatus {
    pub desktop: ComponentStatus,
    pub cli: ComponentStatus,
}

fn desktop_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/Applications/Codex.app"),
        PathBuf::from("/Applications/ChatGPT.app"),
    ];
    if let Some(home) = dirs::home_dir() {
        candidates.extend([
            home.join("Applications/Codex.app"),
            home.join("Applications/ChatGPT.app"),
        ]);
    }
    candidates
}

fn plist_value(app: &Path, key: &str) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    if !plist.is_file() {
        return None;
    }
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", &format!("Print :{key}")])
        .arg(plist)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn is_official_codex_app(app: &Path) -> bool {
    app.is_dir() && plist_value(app, "CFBundleIdentifier").as_deref() == Some("com.openai.codex")
}

pub fn codex_desktop_path() -> Option<PathBuf> {
    desktop_candidates()
        .into_iter()
        .find(|candidate| is_official_codex_app(candidate))
}

pub fn is_codex_desktop_installed() -> bool {
    codex_desktop_path().is_some()
}

fn embedded_cli_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(app) = codex_desktop_path() {
        candidates.push(app.join("Contents/Resources/codex"));
    }
    candidates.extend([
        PathBuf::from("/Applications/Codex.app/Contents/Resources/codex"),
        PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex"),
    ]);
    candidates
}

pub fn codex_cli_binary() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(configured) = std::env::var_os("CODEX_CLI_PATH") {
        candidates.push(PathBuf::from(configured));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.extend([
            home.join(".npm-global/bin/codex"),
            home.join(".bun/bin/codex"),
            home.join(".volta/bin/codex"),
        ]);
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ]);
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/codex"));
    }
    #[cfg(windows)]
    {
        if let Some(app_data) = std::env::var_os("APPDATA") {
            candidates.push(PathBuf::from(app_data).join("npm/codex.cmd"));
            candidates.push(PathBuf::from(app_data).join("npm/codex.exe"));
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(local_app_data).join("Programs/codex/codex.exe"));
        }
        if let Some(path) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&path) {
                candidates.push(directory.join("codex.cmd"));
                candidates.push(directory.join("codex.exe"));
                candidates.push(directory.join("codex"));
            }
        }
    }
    candidates.extend(embedded_cli_candidates());
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn command_version(binary: &Path) -> Option<String> {
    let mut command = Command::new(binary);
    crate::process_utils::hide_console_window(&mut command);
    let output = command.arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|part| part.trim().to_string())
}

fn version_numbers(version: &str) -> Vec<u64> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

fn version_is_newer(latest: &str, current: &str) -> bool {
    let latest = version_numbers(latest);
    let current = version_numbers(current);
    let count = latest.len().max(current.len());
    (0..count)
        .find_map(|index| {
            let left = latest.get(index).copied().unwrap_or(0);
            let right = current.get(index).copied().unwrap_or(0);
            (left != right).then_some(left > right)
        })
        .unwrap_or(false)
}

fn client_with_proxy() -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(1800));
    if let Some(proxy) = crate::official_quota::effective_proxy_url() {
        let proxy = reqwest::Proxy::all(&proxy).map_err(|e| format!("代理配置无效: {e}"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| format!("网络客户端初始化失败: {e}"))
}

fn extract_xml_value(xml: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{tag}>");
    let start = xml.find(&start_tag)? + start_tag.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    let value = xml[start..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

async fn latest_desktop_version(client: &reqwest::Client) -> Result<String, String> {
    let response = client
        .get(DESKTOP_APPCAST_URL)
        .send()
        .await
        .map_err(|e| format!("桌面端版本检查失败: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("桌面端版本检查失败: HTTP {}", response.status()));
    }
    let xml = response
        .text()
        .await
        .map_err(|e| format!("桌面端版本信息读取失败: {e}"))?;
    extract_xml_value(&xml, "sparkle:shortVersionString")
        .or_else(|| extract_xml_value(&xml, "sparkle:version"))
        .ok_or_else(|| "官方更新信息中未找到桌面端版本".to_string())
}

async fn latest_cli_version(client: &reqwest::Client) -> Result<String, String> {
    let response = client
        .get(CLI_LATEST_URL)
        .send()
        .await
        .map_err(|e| format!("CLI 版本检查失败: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("CLI 版本检查失败: HTTP {}", response.status()));
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("CLI 版本信息解析失败: {e}"))?;
    value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "npm 官方版本信息中未找到 CLI 版本".to_string())
}

pub async fn install_status(check_latest: bool) -> CodexInstallStatus {
    let desktop_path = codex_desktop_path();
    let desktop_version = desktop_path
        .as_deref()
        .and_then(|path| plist_value(path, "CFBundleShortVersionString"));
    let cli_path = codex_cli_binary();
    let cli_version = cli_path.as_deref().and_then(command_version);
    let mut status = CodexInstallStatus {
        desktop: ComponentStatus {
            installed: desktop_path.is_some(),
            current_version: desktop_version,
            source: desktop_path.map(|path| path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        cli: ComponentStatus {
            installed: cli_path.is_some(),
            current_version: cli_version,
            source: cli_path.map(|path| {
                if embedded_cli_candidates().contains(&path) {
                    "Codex 桌面端内置".to_string()
                } else {
                    path.to_string_lossy().into_owned()
                }
            }),
            ..Default::default()
        },
    };
    if !check_latest {
        return status;
    }
    let client = match client_with_proxy() {
        Ok(client) => client,
        Err(error) => {
            status.desktop.error = Some(error.clone());
            status.cli.error = Some(error);
            return status;
        }
    };
    let (desktop_latest, cli_latest) =
        tokio::join!(latest_desktop_version(&client), latest_cli_version(&client));
    match desktop_latest {
        Ok(latest) => {
            status.desktop.update_available = status
                .desktop
                .current_version
                .as_deref()
                .is_some_and(|current| version_is_newer(&latest, current));
            status.desktop.latest_version = Some(latest);
        }
        Err(error) => status.desktop.error = Some(error),
    }
    match cli_latest {
        Ok(latest) => {
            status.cli.update_available = status
                .cli
                .current_version
                .as_deref()
                .is_some_and(|current| version_is_newer(&latest, current));
            status.cli.latest_version = Some(latest);
        }
        Err(error) => status.cli.error = Some(error),
    }
    status
}

fn fmt_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn detach(mount: &Path) {
    let _ = Command::new("/usr/bin/hdiutil")
        .arg("detach")
        .arg(mount)
        .output();
}

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
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0);
    Some((major, minor))
}

#[cfg(unix)]
fn free_disk_bytes() -> u64 {
    use std::ffi::CString;
    let Ok(path) = CString::new("/") else {
        return 0;
    };
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut vfs) };
    if rc != 0 {
        return 0;
    }
    (vfs.f_bavail as u64).saturating_mul(vfs.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_disk_bytes() -> u64 {
    // Windows 不依赖 Unix statvfs；系统会在下载/安装阶段报告磁盘错误。
    0
}

fn cleanup(tmp: &Path, mount: &Path) {
    detach(mount);
    let _ = fs::remove_file(tmp);
    let _ = fs::remove_dir_all(mount);
}

fn verify_official_codex(app: &Path) -> Result<(), String> {
    let verify = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .output()
        .map_err(|e| format!("无法运行 codesign: {e}"))?;
    if !verify.status.success() {
        return Err(format!(
            "官方安装包签名校验失败: {}",
            String::from_utf8_lossy(&verify.stderr).trim()
        ));
    }
    let detail = Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=4"])
        .arg(app)
        .output()
        .map_err(|e| format!("无法读取安装包签名: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&detail.stdout),
        String::from_utf8_lossy(&detail.stderr)
    );
    if !text
        .lines()
        .any(|line| line.trim() == "Identifier=com.openai.codex")
    {
        return Err("安装包标识不是官方 Codex（com.openai.codex）".into());
    }
    if !text
        .lines()
        .any(|line| line.trim() == "TeamIdentifier=2DC432GLL2")
    {
        return Err("安装包开发者 Team ID 不是 OpenAI 官方签名".into());
    }
    let assess = Command::new("/usr/sbin/spctl")
        .args(["--assess", "--type", "execute", "--verbose=2"])
        .arg(app)
        .output()
        .map_err(|e| format!("无法运行 Gatekeeper 校验: {e}"))?;
    if !assess.status.success() {
        return Err(format!(
            "Gatekeeper 拒绝该安装包: {}",
            String::from_utf8_lossy(&assess.stderr).trim()
        ));
    }
    Ok(())
}

fn mounted_codex_app(mount: &Path) -> Option<PathBuf> {
    fs::read_dir(mount)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().and_then(|value| value.to_str()) == Some("app")
                && is_official_codex_app(path)
        })
}

fn codex_process_running() -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-f", "/Applications/(Codex|ChatGPT)\\.app/Contents/MacOS/"])
        .status()
        .is_ok_and(|status| status.success())
}

pub async fn install_desktop(
    allow_update: bool,
    emit: impl Fn(InstallProgress) + Send + Sync,
) -> Result<(), String> {
    let existing = codex_desktop_path();
    if existing.is_some() && !allow_update {
        return Ok(());
    }
    if existing.is_some() && codex_process_running() {
        return Err("请先完全退出 Codex 桌面端，再执行更新".into());
    }
    if let Some((major, minor)) = macos_version() {
        if major < 13 {
            return Err(format!(
                "Codex 桌面端需要 macOS 13 或更高版本（当前 {major}.{minor}）"
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
        DESKTOP_X64_URL
    } else {
        DESKTOP_ARM_URL
    };
    emit(InstallProgress {
        component: "desktop".into(),
        phase: "下载".into(),
        percent: 0.0,
        message: "正在下载 Codex 官方安装包…".into(),
    });
    let client = client_with_proxy()?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("下载失败: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("下载失败: HTTP {}", response.status()));
    }
    let total = response.content_length().unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "codexff-codex-{}-{}.dmg",
        std::process::id(),
        chrono::Local::now().format("%Y%m%d%H%M%S")
    ));
    let mut file = fs::File::create(&tmp).map_err(|e| format!("无法创建临时文件: {e}"))?;
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("下载中断: {e}")
    })? {
        file.write_all(&chunk)
            .map_err(|e| format!("写入失败: {e}"))?;
        written += chunk.len() as u64;
        emit(InstallProgress {
            component: "desktop".into(),
            phase: "下载".into(),
            percent: if total > 0 {
                (written as f64 / total as f64) * 90.0
            } else {
                -1.0
            },
            message: format!(
                "下载中 {} / {}",
                fmt_mb(written),
                if total > 0 {
                    fmt_mb(total)
                } else {
                    "未知".into()
                }
            ),
        });
    }
    drop(file);
    if written < 10 * 1024 * 1024 {
        let _ = fs::remove_file(&tmp);
        return Err("下载文件异常（过小），请重试".into());
    }

    emit(InstallProgress {
        component: "desktop".into(),
        phase: "安装".into(),
        percent: 93.0,
        message: "正在挂载并校验安装包…".into(),
    });
    let mount = std::env::temp_dir().join(format!("codexff-codex-mount-{}", std::process::id()));
    let _ = fs::remove_dir_all(&mount);
    fs::create_dir_all(&mount).map_err(|e| format!("无法创建挂载点: {e}"))?;
    let attach = Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount)
        .arg(&tmp)
        .output()
        .map_err(|e| format!("无法运行 hdiutil: {e}"))?;
    if !attach.status.success() {
        cleanup(&tmp, &mount);
        return Err(format!(
            "安装包挂载失败: {}",
            String::from_utf8_lossy(&attach.stderr).trim()
        ));
    }
    let Some(source) = mounted_codex_app(&mount) else {
        cleanup(&tmp, &mount);
        return Err("安装包中未找到官方 Codex App".into());
    };
    if let Err(error) = verify_official_codex(&source) {
        cleanup(&tmp, &mount);
        return Err(error);
    }

    let target = existing.unwrap_or_else(|| {
        PathBuf::from("/Applications").join(
            source
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("Codex.app")),
        )
    });
    let stage = target.with_file_name(format!(
        ".codexff-installing-{}-{}.app",
        std::process::id(),
        chrono::Local::now().timestamp()
    ));
    let backup = target.with_file_name(format!(
        ".codexff-backup-{}-{}.app",
        std::process::id(),
        chrono::Local::now().timestamp()
    ));
    let _ = fs::remove_dir_all(&stage);
    emit(InstallProgress {
        component: "desktop".into(),
        phase: "安装".into(),
        percent: 96.0,
        message: if target.exists() {
            "正在安全更新 Codex 桌面端…".into()
        } else {
            "正在安装 Codex 到「应用程序」…".into()
        },
    });
    let copied = Command::new("/usr/bin/ditto")
        .arg(&source)
        .arg(&stage)
        .status()
        .map_err(|e| format!("无法运行 ditto: {e}"))?;
    if !copied.success() {
        cleanup(&tmp, &mount);
        return Err("复制到应用程序失败，请检查磁盘权限".into());
    }
    if let Err(error) = verify_official_codex(&stage) {
        let _ = fs::remove_dir_all(&stage);
        cleanup(&tmp, &mount);
        return Err(format!("复制后的应用校验失败: {error}"));
    }
    if target.exists() {
        fs::rename(&target, &backup).map_err(|e| {
            let _ = fs::remove_dir_all(&stage);
            cleanup(&tmp, &mount);
            format!("无法备份现有 Codex App: {e}")
        })?;
    }
    if let Err(error) = fs::rename(&stage, &target) {
        if backup.exists() {
            let _ = fs::rename(&backup, &target);
        }
        let _ = fs::remove_dir_all(&stage);
        cleanup(&tmp, &mount);
        return Err(format!("安装最终落盘失败: {error}"));
    }
    let _ = fs::remove_dir_all(&backup);
    cleanup(&tmp, &mount);
    emit(InstallProgress {
        component: "desktop".into(),
        phase: "完成".into(),
        percent: 100.0,
        message: "Codex 桌面端安装完成".into(),
    });
    Ok(())
}

fn npm_binary() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.extend([
            home.join(".npm-global/bin/npm"),
            home.join(".volta/bin/npm"),
        ]);
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/npm"),
        PathBuf::from("/usr/local/bin/npm"),
        PathBuf::from("/usr/bin/npm"),
    ]);
    candidates.into_iter().find(|path| path.is_file())
}

fn verify_official_cli(binary: &Path) -> Result<(), String> {
    let verify = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(binary)
        .output()
        .map_err(|e| format!("无法校验 Codex CLI: {e}"))?;
    if !verify.status.success() {
        return Err(format!(
            "Codex CLI 签名校验失败: {}",
            String::from_utf8_lossy(&verify.stderr).trim()
        ));
    }
    let detail = Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=4"])
        .arg(binary)
        .output()
        .map_err(|e| format!("无法读取 Codex CLI 签名: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&detail.stdout),
        String::from_utf8_lossy(&detail.stderr)
    );
    if !text
        .lines()
        .any(|line| line.trim() == "TeamIdentifier=2DC432GLL2")
    {
        return Err("Codex CLI 不是 OpenAI 官方签名".into());
    }
    Ok(())
}

fn install_cli_binary(source: &Path) -> Result<(), String> {
    verify_official_cli(source)?;
    let home = dirs::home_dir().ok_or_else(|| "无法定位用户目录".to_string())?;
    let bin_dir = home.join(".local/bin");
    fs::create_dir_all(&bin_dir).map_err(|e| format!("无法创建 CLI 目录: {e}"))?;
    let target = bin_dir.join("codex");
    let stage = bin_dir.join(format!(".codex-installing-{}", std::process::id()));
    let _ = fs::remove_file(&stage);
    fs::copy(source, &stage).map_err(|e| format!("无法复制 Codex CLI: {e}"))?;
    verify_official_cli(&stage)?;
    fs::rename(&stage, &target).map_err(|e| {
        let _ = fs::remove_file(&stage);
        format!("无法安装 Codex CLI: {e}")
    })
}

pub fn install_cli(
    prefer_latest: bool,
    emit: impl Fn(InstallProgress) + Send + Sync,
) -> Result<(), String> {
    emit(InstallProgress {
        component: "cli".into(),
        phase: "安装".into(),
        percent: -1.0,
        message: "正在安装 Codex CLI…".into(),
    });
    if prefer_latest {
        if let Some(npm) = npm_binary() {
            let mut command = Command::new(npm);
            crate::process_utils::hide_console_window(&mut command);
            let output = command
                .args(["install", "--global", "@openai/codex@latest"])
                .output()
                .map_err(|e| format!("无法运行 npm: {e}"))?;
            if output.status.success() {
                emit(InstallProgress {
                    component: "cli".into(),
                    phase: "完成".into(),
                    percent: 100.0,
                    message: "Codex CLI 已更新到官方最新版本".into(),
                });
                return Ok(());
            }
        }
    }
    let source = embedded_cli_candidates()
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| "未找到 Codex 桌面端内置 CLI，请先完成桌面端安装".to_string())?;
    install_cli_binary(&source)?;
    emit(InstallProgress {
        component: "cli".into(),
        phase: "完成".into(),
        percent: 100.0,
        message: "Codex CLI 已安装，并建立用户级命令入口".into(),
    });
    Ok(())
}

pub async fn install_all(emit: impl Fn(InstallProgress) + Send + Sync) -> Result<(), String> {
    if !is_codex_desktop_installed() {
        install_desktop(false, &emit).await?;
    }
    if codex_cli_binary().is_none() {
        install_cli(false, &emit)?;
    } else if let Some(source) = embedded_cli_candidates()
        .into_iter()
        .find(|path| path.is_file())
    {
        let _ = install_cli_binary(&source);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{extract_xml_value, version_is_newer};

    #[test]
    fn compares_versions_numerically() {
        assert!(version_is_newer("0.148.0", "0.99.0"));
        assert!(!version_is_newer("0.148.0", "0.148.0-alpha.9"));
        assert!(version_is_newer("26.811.1", "26.810.52044"));
    }

    #[test]
    fn extracts_appcast_version() {
        let xml = "<sparkle:shortVersionString>26.811.123</sparkle:shortVersionString>";
        assert_eq!(
            extract_xml_value(xml, "sparkle:shortVersionString").as_deref(),
            Some("26.811.123")
        );
    }
}
