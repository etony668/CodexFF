//! 宠物管理 — 扫描 / 导入 / 删除 Codex 自定义宠物包 (~/.codex/pets/<id>/).
//!
//! 宠物包规范: 目录内 pet.json + spritesheet.webp (v1 1536x1872 / v2 1536x2288)。
//! 安装到 ~/.codex/pets 后, 在 Codex 设置 → 外观 → Pets 中选择生效。

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::codex_config;
use crate::vault;

#[derive(Debug, Clone, Serialize)]
pub struct PetMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub sprite_version: u32,
    /// 精灵图绝对路径 (前端用 asset 协议预览)
    pub spritesheet_path: String,
    pub size_bytes: u64,
    pub valid: bool,
    pub validation: String,
}

#[derive(Debug, Deserialize)]
struct PetJson {
    id: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    description: Option<String>,
    #[serde(rename = "spriteVersionNumber")]
    sprite_version_number: Option<u32>,
    #[serde(rename = "spritesheetPath")]
    spritesheet_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PetFileInput {
    /// 相对路径 (webkitdirectory 的 webkitRelativePath)
    pub path: String,
    #[serde(rename = "dataBase64")]
    pub data_base64: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PetError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("无效宠物包: {0}")]
    Invalid(String),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("安装已取消")]
    Cancelled,
    #[error("命令执行超时（10 分钟），已自动终止")]
    Timeout,
}

fn pets_root() -> PathBuf {
    codex_config::codex_config_dir().join("pets")
}

pub(crate) fn sanitize_id(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    let cleaned = cleaned.trim_matches(['.', '-', '_']);
    if cleaned.is_empty() || cleaned.len() > 64 {
        None
    } else {
        Some(cleaned.to_string())
    }
}

/// 读取已安装宠物目录的元信息; 目录里没有 pet.json 时返回 None (跳过)。
fn read_pet_meta(dir: &Path) -> Result<Option<PetMeta>, PetError> {
    let json_path = dir.join("pet.json");
    if !json_path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&json_path)?;
    let pj: PetJson = serde_json::from_str(&text)
        .map_err(|e| PetError::Invalid(format!("pet.json 解析失败: {e}")))?;
    let id = pj
        .id
        .filter(|s| !s.is_empty())
        .or_else(|| dir.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_default();
    let version = pj.sprite_version_number.unwrap_or(1);
    let sprite_name = pj
        .spritesheet_path
        .unwrap_or_else(|| "spritesheet.webp".to_string());
    let sprite_path = dir.join(&sprite_name);
    let (size_bytes, dims) = if sprite_path.exists() {
        (
            std::fs::metadata(&sprite_path).map(|m| m.len()).unwrap_or(0),
            image_dimensions(&sprite_path),
        )
    } else {
        (0, Err(format!("缺少精灵图文件 {sprite_name}")))
    };
    let (valid, validation) = match dims {
        Ok((w, h)) if version == 1 && (w, h) == (1536, 1872) => (true, "OK".to_string()),
        Ok((w, h)) if version == 2 && (w, h) == (1536, 2288) => (true, "OK".to_string()),
        Ok((w, h)) => (
            false,
            format!("图集尺寸 {w}x{h} 与声明版本 v{version} 不匹配"),
        ),
        Err(e) => (false, e),
    };
    Ok(Some(PetMeta {
        id,
        name: pj.display_name.unwrap_or_else(|| {
            dir.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        }),
        description: pj.description.unwrap_or_default(),
        sprite_version: version,
        spritesheet_path: sprite_path.to_string_lossy().to_string(),
        size_bytes,
        valid,
        validation,
    }))
}

/// 扫描 ~/.codex/pets 下所有自定义宠物。
pub fn list_pets() -> Result<Vec<PetMeta>, PetError> {
    let root = pets_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if let Some(meta) = read_pet_meta(&dir)? {
            out.push(meta);
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// 解析 PNG / WebP 尺寸。
fn image_dimensions(path: &Path) -> Result<(u32, u32), String> {
    let data = std::fs::read(path).map_err(|e| format!("读取精灵图失败: {e}"))?;
    if data.len() >= 8 && &data[0..8] == b"\x89PNG\r\n\x1a\n" {
        if data.len() < 24 {
            return Err("PNG 文件不完整".into());
        }
        let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Ok((w, h));
    }
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        if data.len() < 16 {
            return Err("WEBP 文件不完整".into());
        }
        match &data[12..16] {
            b"VP8X" => {
                if data.len() < 30 {
                    return Err("WEBP(VP8X) 文件不完整".into());
                }
                let w =
                    u32::from(data[24]) | (u32::from(data[25]) << 8) | (u32::from(data[26]) << 16);
                let h =
                    u32::from(data[27]) | (u32::from(data[28]) << 8) | (u32::from(data[29]) << 16);
                Ok((w + 1, h + 1))
            }
            b"VP8L" => {
                if data.len() < 25 {
                    return Err("WEBP(VP8L) 文件不完整".into());
                }
                let b = u32::from(data[21])
                    | (u32::from(data[22]) << 8)
                    | (u32::from(data[23]) << 16)
                    | (u32::from(data[24]) << 24);
                Ok(((b & 0x3fff) + 1, ((b >> 14) & 0x3fff) + 1))
            }
            b"VP8 " => {
                if data.len() < 30 {
                    return Err("WEBP(VP8) 文件不完整".into());
                }
                let w = u32::from(data[26]) | (u32::from(data[27]) << 8);
                let h = u32::from(data[28]) | (u32::from(data[29]) << 8);
                Ok((w, h))
            }
            _ => Err("未知 WEBP 格式".into()),
        }
    } else {
        Err("仅支持 PNG / WebP 精灵图".into())
    }
}

fn decode_b64(data: &str) -> Result<Vec<u8>, PetError> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| PetError::Invalid(format!("文件数据解码失败: {e}")))
}

fn tmp_import_dir() -> Result<PathBuf, PetError> {
    let dir = std::env::temp_dir().join(format!("codexff-pet-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 导入 ZIP 宠物包 (base64 上传, 系统 ditto 解压)。
pub fn import_zip(_file_name: &str, data_base64: &str) -> Result<PetMeta, PetError> {
    let bytes = decode_b64(data_base64)?;
    if bytes.len() > 256 * 1024 * 1024 {
        return Err(PetError::Invalid("ZIP 超过 256MB 上限".into()));
    }
    let tmp = tmp_import_dir()?;
    let zip_path = tmp.join("pet.zip");
    std::fs::write(&zip_path, &bytes)?;
    let extract = tmp.join("extracted");
    std::fs::create_dir_all(&extract)?;
    let status = std::process::Command::new("/usr/bin/ditto")
        .arg("-xk")
        .arg(&zip_path)
        .arg(&extract)
        .status()?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(PetError::Invalid("ZIP 解压失败，文件可能损坏或不是有效压缩包".into()));
    }
    let result = install_from_dir(&extract, false);
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// 导入宠物文件夹 (webkitdirectory 逐个文件 base64 上传)。
pub fn import_folder(files: Vec<PetFileInput>) -> Result<PetMeta, PetError> {
    if files.is_empty() {
        return Err(PetError::Invalid("未选择任何文件".into()));
    }
    let tmp = tmp_import_dir()?;
    for f in files {
        let rel = Path::new(&f.path);
        if rel.is_absolute()
            || rel.components().any(|c| {
                matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))
            })
        {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(PetError::Invalid("非法文件路径".into()));
        }
        let target = tmp.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, decode_b64(&f.data_base64)?)?;
    }
    let result = install_from_dir(&tmp, false);
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// 在目录 (含子目录, 最多 3 层) 中查找 pet.json。
fn find_pet_json(dir: &Path, depth: u32) -> Result<PathBuf, PetError> {
    let direct = dir.join("pet.json");
    if direct.exists() {
        return Ok(direct);
    }
    if depth >= 3 {
        return Err(PetError::Invalid("未找到 pet.json".into()));
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().is_dir() {
            if let Ok(found) = find_pet_json(&entry.path(), depth + 1) {
                return Ok(found);
            }
        }
    }
    Err(PetError::Invalid("未找到 pet.json".into()))
}

fn install_from_dir(dir: &Path, replace: bool) -> Result<PetMeta, PetError> {
    let json_path = find_pet_json(dir, 0)?;
    let package_dir = json_path
        .parent()
        .ok_or_else(|| PetError::Invalid("pet.json 缺少父目录".into()))?
        .to_path_buf();
    let text = std::fs::read_to_string(&json_path)?;
    let pj: PetJson = serde_json::from_str(&text)
        .map_err(|e| PetError::Invalid(format!("pet.json 解析失败: {e}")))?;
    let raw_id = pj
        .id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            pj.display_name
                .clone()
                .unwrap_or_else(|| "custom-pet".to_string())
        });
    let id = sanitize_id(&raw_id)
        .ok_or_else(|| PetError::Invalid(format!("宠物 id 含非法字符: {raw_id}")))?;
    let version = pj.sprite_version_number.unwrap_or(1);
    if version != 1 && version != 2 {
        return Err(PetError::Invalid(format!(
            "不支持的图集版本 v{version} (仅支持 v1/v2)"
        )));
    }
    let sprite_name = pj
        .spritesheet_path
        .unwrap_or_else(|| "spritesheet.webp".to_string());
    let sprite_src = package_dir.join(&sprite_name);
    if !sprite_src.exists() {
        return Err(PetError::Invalid(format!("缺少精灵图文件 {sprite_name}")));
    }
    let canon_src = sprite_src
        .canonicalize()
        .map_err(|_| PetError::Invalid("精灵图文件不可访问".into()))?;
    let canon_pkg = package_dir
        .canonicalize()
        .map_err(|_| PetError::Invalid("宠物包目录不可访问".into()))?;
    if !canon_src.starts_with(&canon_pkg) {
        return Err(PetError::Invalid("精灵图路径越界".into()));
    }
    let dims = image_dimensions(&canon_src).map_err(PetError::Invalid)?;
    let expected = if version == 1 {
        (1536, 1872)
    } else {
        (1536, 2288)
    };
    if dims != expected {
        return Err(PetError::Invalid(format!(
            "图集尺寸 {}x{} 与声明版本 v{version} 不匹配 (应为 {}x{})",
            dims.0, dims.1, expected.0, expected.1
        )));
    }

    let root = pets_root();
    let dest = root.join(&id);
    if dest.exists() {
        if replace {
            trash_dir(&dest)?;
        } else {
            return Err(PetError::Invalid("同名宠物已存在，请先删除再导入".into()));
        }
    }
    std::fs::create_dir_all(&dest)?;
    std::fs::copy(&json_path, dest.join("pet.json"))?;
    std::fs::copy(&canon_src, dest.join(&sprite_name))?;

    read_pet_meta(&dest)?.ok_or_else(|| PetError::Invalid("安装后校验失败".into()))
}

/// 把目录移入金库回收区 (跨卷回退复制后删除)。
fn trash_dir(src: &Path) -> Result<(), PetError> {
    let trash_root = vault::vault_dir().join("pets-trash");
    std::fs::create_dir_all(&trash_root)?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "pet".to_string());
    let dst = trash_root.join(format!("{name}-{ts}"));
    match std::fs::rename(src, &dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_dir(src, &dst)?;
            std::fs::remove_dir_all(src)?;
            Ok(())
        }
    }
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), PetError> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// 当前正在执行的命令安装进程组 (无则 None)。
static ACTIVE_INSTALL_PGID: Mutex<Option<u32>> = Mutex::new(None);
/// 用户是否请求取消当前安装。
static INSTALL_CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

fn kill_process_group(pgid: u32) {
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// GUI 启动时 PATH 可能不含 Homebrew / nvm / bun 等目录, 拼一份常见路径。
fn common_bin_dirs() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs = vec![
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        format!("{home}/.local/bin"),
        format!("{home}/.bun/bin"),
        format!("{home}/.volta/bin"),
        format!("{home}/.fnm/aliases/default/bin"),
    ];
    if let Ok(entries) = std::fs::read_dir(format!("{home}/.nvm/versions/node")) {
        let mut versions: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("bin").to_string_lossy().to_string())
            .collect();
        versions.sort();
        versions.reverse();
        dirs.extend(versions);
    }
    dirs
}

/// 识别 Windows / PowerShell 专用命令, 给 macOS 用户友好提示。
fn is_windows_command(command: &str) -> bool {
    let lower = command.trim_start().to_ascii_lowercase();
    lower.starts_with("powershell")
        || lower.starts_with("pwsh")
        || lower.starts_with("iwr ")
        || lower.starts_with("irm ")
        || lower.contains("| iex")
        || lower.contains("install-codepet")
        || lower.contains(".ps1")
}

/// 执行用户粘贴的终端安装命令 (bash -c, 支持 npx / curl|sh / git clone 及多行脚本)。
/// 输出行通过 on_line 回调流式上抛; 完成后对比安装前后宠物列表, 返回新增宠物。
pub fn install_from_command(
    command: &str,
    on_line: Option<Arc<dyn Fn(&str) + Send + Sync>>,
) -> Result<Vec<PetMeta>, PetError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(PetError::Invalid("请先粘贴安装命令".into()));
    }
    if trimmed.chars().count() > 16 * 1024 {
        return Err(PetError::Invalid("命令过长（超过 16KB）".into()));
    }
    if is_windows_command(trimmed) {
        return Err(PetError::Invalid(
            "检测到这是 Windows / PowerShell 命令，macOS 请改用该仓库的 curl / sh 版本".into(),
        ));
    }
    if trimmed.contains("sudo ") || trimmed.starts_with("sudo") {
        return Err(PetError::Invalid(
            "命令包含 sudo，App 无法弹出密码授权；请去掉 sudo 或改用系统终端执行".into(),
        ));
    }

    let before: HashSet<String> = list_pets()?.into_iter().map(|p| p.id).collect();

    // 本次安装状态先初始化: 代理扫描等准备工作可能耗时, 期间用户已点取消
    // 时不能让 spawn 后的重置把取消标记吞掉。
    *ACTIVE_INSTALL_PGID.lock().unwrap() = None;
    INSTALL_CANCEL_REQUESTED.store(false, Ordering::SeqCst);

    let mut path = std::env::var("PATH").unwrap_or_default();
    for dir in common_bin_dirs() {
        if !path.split(':').any(|p| p == dir) {
            path = format!("{dir}:{path}");
        }
    }

    let mut cmd = Command::new("/bin/bash");
    // pipefail: curl ... | bash 这类管道在下载失败时会真实报错,
    // 否则 bash 拿到空输入也退出 0, App 会误报“命令执行成功”。
    cmd.arg("-o").arg("pipefail");
    cmd.arg("-c").arg(trimmed);
    cmd.env("PATH", &path);
    cmd.env("CODEX_HOME", codex_config::codex_config_dir());
    // macOS 的 curl 不会自动读系统代理, App 内执行安装命令时把系统代理
    // 注入子进程 (FlClash 等客户端设置的 HTTP/SOCKS 代理), 否则 GitHub
    // 等源下载会失败; 本地地址排除, 避免代理自环。TUN/增强模式没有系统代理
    // 时, 自动扫描常见本地代理端口兜底。
    let proxy = crate::official_quota::effective_proxy_url();
    if let Some(proxy) = proxy {
        if let Some(cb) = &on_line {
            cb(&format!("使用代理: {proxy}"));
        }
        cmd.env("http_proxy", &proxy);
        cmd.env("https_proxy", &proxy);
        cmd.env("HTTP_PROXY", &proxy);
        cmd.env("HTTPS_PROXY", &proxy);
        cmd.env("all_proxy", &proxy);
        cmd.env("ALL_PROXY", &proxy);
        cmd.env("no_proxy", "127.0.0.1,localhost,::1");
        cmd.env("NO_PROXY", "127.0.0.1,localhost,::1");
    }
    // npx 首次拉包时非交互环境可能卡在确认, 预置自动确认
    cmd.env("npm_config_yes", "true");
    cmd.env("NPM_CONFIG_YES", "true");
    cmd.env("npm_config_fund", "false");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| PetError::Invalid(format!("无法启动命令: {e}")))?;
    let pgid = child.id();
    *ACTIVE_INSTALL_PGID.lock().unwrap() = Some(pgid);

    if let Some(out) = child.stdout.take() {
        let cb = on_line.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if let Some(cb) = &cb {
                    cb(&line);
                }
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let cb = on_line;
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines() {
                let Ok(line) = line else { break };
                if let Some(cb) = &cb {
                    cb(&line);
                }
            }
        });
    }

    let deadline = Instant::now() + Duration::from_secs(600);
    let result = loop {
        if INSTALL_CANCEL_REQUESTED.load(Ordering::SeqCst) {
            kill_process_group(pgid);
            let _ = child.wait();
            *ACTIVE_INSTALL_PGID.lock().unwrap() = None;
            break Err(PetError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                *ACTIVE_INSTALL_PGID.lock().unwrap() = None;
                if status.success() {
                    break Ok(());
                }
                break Err(PetError::Invalid(format!(
                    "命令执行失败（退出码 {}）",
                    status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "信号终止".into())
                )));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_process_group(pgid);
                    let _ = child.wait();
                    *ACTIVE_INSTALL_PGID.lock().unwrap() = None;
                    break Err(PetError::Timeout);
                }
            }
            Err(e) => {
                *ACTIVE_INSTALL_PGID.lock().unwrap() = None;
                break Err(PetError::Invalid(format!("等待命令退出失败: {e}")));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    result?;

    let after = list_pets()?;
    Ok(after.into_iter().filter(|p| !before.contains(&p.id)).collect())
}

/// 请求取消当前正在执行的命令安装 (终止整个进程组)。
pub fn cancel_command_install() -> Result<(), PetError> {
    INSTALL_CANCEL_REQUESTED.store(true, Ordering::SeqCst);
    if let Some(pgid) = *ACTIVE_INSTALL_PGID.lock().unwrap() {
        kill_process_group(pgid);
    }
    Ok(())
}

/// 删除自定义宠物 (移入金库回收区, 可手动找回)。
pub fn delete_pet(pet_id: &str) -> Result<(), PetError> {
    let id = sanitize_id(pet_id).ok_or_else(|| PetError::Invalid("非法宠物 id".into()))?;
    let src = pets_root().join(&id);
    if !src.exists() {
        return Err(PetError::Invalid("宠物不存在".into()));
    }
    trash_dir(&src)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_webp(width: u32, height: u32) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&36u32.to_le_bytes());
        data.extend_from_slice(b"WEBP");
        data.extend_from_slice(b"VP8X");
        data.extend_from_slice(&10u32.to_le_bytes());
        data.push(0);
        data.extend_from_slice(&[0, 0, 0]);
        data.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        data.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        data
    }

    fn file_input(path: &str, data: Vec<u8>) -> PetFileInput {
        PetFileInput {
            path: path.to_string(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
        }
    }

    #[test]
    fn install_list_delete_roundtrip() {
        let _guard = crate::test_util::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("codexff-pet-home-{}", std::process::id()));
        let vault = std::env::temp_dir().join(format!("codexff-pet-vault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&vault);
        std::fs::create_dir_all(home.join("pets")).expect("create pets dir");
        std::env::set_var("CODEX_HOME", &home);
        std::env::set_var("CODEXFF_VAULT_DIR", &vault);

        let sprite = fake_webp(1536, 2288);
        let pet_json = br#"{"id":"test-pet","displayName":"Test Pet","description":"demo","spriteVersionNumber":2,"spritesheetPath":"spritesheet.webp"}"#;
        let meta = import_folder(vec![
            file_input("pet.json", pet_json.to_vec()),
            file_input("spritesheet.webp", sprite.clone()),
        ])
        .expect("import should succeed");
        assert_eq!(meta.id, "test-pet");
        assert!(meta.valid);
        assert_eq!(meta.sprite_version, 2);

        let pets = list_pets().expect("list");
        assert_eq!(pets.len(), 1);
        assert!(pets[0].spritesheet_path.ends_with("spritesheet.webp"));

        // 同名重复导入应被拒绝
        let dup = import_folder(vec![
            file_input("pet.json", pet_json.to_vec()),
            file_input("spritesheet.webp", sprite.clone()),
        ]);
        assert!(dup.is_err());

        delete_pet("test-pet").expect("delete");
        assert!(list_pets().expect("list after delete").is_empty());
        let trash = vault.join("pets-trash");
        assert!(std::fs::read_dir(&trash).expect("trash").next().is_some());

        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CODEXFF_VAULT_DIR");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&vault);
    }

    #[test]
    fn rejects_wrong_dimensions() {
        let _guard = crate::test_util::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("codexff-pet-home2-{}", std::process::id()));
        let vault = std::env::temp_dir().join(format!("codexff-pet-vault2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&vault);
        std::fs::create_dir_all(home.join("pets")).expect("create pets dir");
        std::env::set_var("CODEX_HOME", &home);
        std::env::set_var("CODEXFF_VAULT_DIR", &vault);

        // v2 声明但给了 v1 尺寸
        let err = import_folder(vec![
            file_input(
                "pet.json",
                br#"{"id":"bad-pet","displayName":"Bad","spriteVersionNumber":2}"#.to_vec(),
            ),
            file_input("spritesheet.webp", fake_webp(1536, 1872)),
        ])
        .expect_err("should reject");
        assert!(err.to_string().contains("不匹配"), "{err}");

        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CODEXFF_VAULT_DIR");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&vault);
    }

    #[test]
    fn command_install_cancel() {
        let _guard = crate::test_util::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("codexff-pet-home3-{}", std::process::id()));
        let vault = std::env::temp_dir().join(format!("codexff-pet-vault3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&vault);
        std::fs::create_dir_all(home.join("pets")).expect("create pets dir");
        std::env::set_var("CODEX_HOME", &home);
        std::env::set_var("CODEXFF_VAULT_DIR", &vault);

        let handle = std::thread::spawn(move || install_from_command("sleep 30", None));
        std::thread::sleep(Duration::from_millis(400));
        cancel_command_install().expect("cancel should work");
        let res = handle.join().expect("thread");
        assert!(matches!(res, Err(PetError::Cancelled)), "{res:?}");

        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CODEXFF_VAULT_DIR");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&vault);
    }

    #[test]
    fn command_install_pipefail_surfaces_failure() {
        let _guard = crate::test_util::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("codexff-pet-home4-{}", std::process::id()));
        let vault = std::env::temp_dir().join(format!("codexff-pet-vault4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&vault);
        std::fs::create_dir_all(home.join("pets")).expect("create pets dir");
        std::env::set_var("CODEX_HOME", &home);
        std::env::set_var("CODEXFF_VAULT_DIR", &vault);

        // 无 pipefail 时 false | true 退出码为 0, 会被误判为成功;
        // 开启 pipefail 后应真实报错。
        let res = install_from_command("false | true", None);
        assert!(
            matches!(res, Err(PetError::Invalid(ref msg)) if msg.contains("命令执行失败")),
            "{res:?}"
        );

        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CODEXFF_VAULT_DIR");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&vault);
    }
}
