// codexff — Codex profile switcher
// 凭证金库 + 物理隔离 + 会话管理 + IP 指纹守护

pub mod balance;
pub mod codex_config;
pub mod codex_install;
pub mod import_config;
pub mod ip_guard;
pub mod local_router;
pub mod official_quota;
pub mod pet_manager;
pub mod profiles;
pub mod session_manager;
pub mod session_model;
pub mod session_usage;
pub mod session_unify;
#[cfg(test)]
pub mod test_util;
pub mod tray;
pub mod usage_stats;
pub mod vault;
pub mod workflow;

use serde::Serialize;
use tauri::Manager;

use profiles::{ActiveSelection, ProfilesError, RelayProfile, RelayProfileInput};

#[derive(Serialize)]
struct ApiError {
    message: String,
}

impl From<ProfilesError> for ApiError {
    fn from(e: ProfilesError) -> Self {
        ApiError {
            message: e.to_string(),
        }
    }
}
impl From<codex_config::CodexConfigError> for ApiError {
    fn from(e: codex_config::CodexConfigError) -> Self {
        ApiError {
            message: e.to_string(),
        }
    }
}
impl From<session_manager::SessionError> for ApiError {
    fn from(e: session_manager::SessionError) -> Self {
        ApiError {
            message: e.to_string(),
        }
    }
}
impl From<session_model::ModelRemapError> for ApiError {
    fn from(e: session_model::ModelRemapError) -> Self {
        ApiError {
            message: e.to_string(),
        }
    }
}
impl From<ip_guard::IpGuardError> for ApiError {
    fn from(e: ip_guard::IpGuardError) -> Self {
        ApiError {
            message: e.to_string(),
        }
    }
}
impl From<pet_manager::PetError> for ApiError {
    fn from(e: pet_manager::PetError) -> Self {
        ApiError {
            message: e.to_string(),
        }
    }
}
impl From<workflow::WorkflowError> for ApiError {
    fn from(e: workflow::WorkflowError) -> Self {
        ApiError {
            message: e.to_string(),
        }
    }
}

#[derive(Serialize)]
struct AppStatus {
    active: Option<ActiveSelection>,
    relays: Vec<RelayProfile>,
    official_login_present: bool,
    ip: ip_guard::IpCheckResult,
    version: String,
}

#[tauri::command]
async fn get_status() -> Result<AppStatus, ApiError> {
    let active = profiles::current_active().ok();
    let relays = profiles::list_relay_profiles()?;
    // 官方模式下 codex login 后 vault 可能还没有副本 (原捕获时机 = 切中转那刻)。
    // 每次状态刷新尝试补捕获 — 幂等 (vault 已有副本或非官方凭证形态则跳过)。
    let _ = vault::capture_official_if_missing();
    let official_login_present = vault::restore_has_credentials();
    let ip = ip_guard::check_ip().await;
    Ok(AppStatus {
        active,
        relays,
        official_login_present,
        ip,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[tauri::command]
fn list_relays() -> Result<Vec<RelayProfile>, ApiError> {
    Ok(profiles::list_relay_profiles()?)
}

#[tauri::command]
fn add_relay(input: RelayProfileInput) -> Result<RelayProfile, ApiError> {
    Ok(profiles::add_relay_profile(input)?)
}

#[tauri::command]
fn update_relay(id: String, input: RelayProfileInput) -> Result<RelayProfile, ApiError> {
    Ok(profiles::update_relay_profile(&id, input)?)
}

#[tauri::command]
fn get_common_config() -> Result<Option<String>, ApiError> {
    Ok(profiles::get_common_config()?)
}

#[tauri::command]
fn set_common_config(snippet: String) -> Result<(), ApiError> {
    Ok(profiles::set_common_config(&snippet)?)
}

/// 添加供应商的默认 config.toml 底稿: 磁盘通用段 + 预设 provider 段合并
/// (preset_config = 预设 TOML, None = 自定义空预设)。见 merge_preset_config。
#[tauri::command]
fn get_default_config_toml(preset_config: Option<String>) -> Result<String, ApiError> {
    Ok(codex_config::merge_preset_config(preset_config.as_deref())
        .map_err(profiles::ProfilesError::from)?)
}

#[tauri::command]
fn delete_relay(id: String) -> Result<(), ApiError> {
    Ok(profiles::delete_relay_profile(&id)?)
}

/// 激活官方 profile。激活前 IP 硬检查: 有基线且当前出口 ≠ 基线 →
/// 拒绝 (除非 force=true, 前端确认后重试)。激活后后台记录出口 IP 作为基线。
#[tauri::command]
async fn activate_official(
    app: tauri::AppHandle,
    force: Option<bool>,
) -> Result<ActiveSelection, ApiError> {
    use tauri::Emitter;
    // 封号主因 = 官方账号活跃 IP 变化。基线存在且不一致 → 拦截,
    // 提示用户先固定出口; 确认无误后 force 重试。
    if !force.unwrap_or(false) {
        let current = ip_guard::current_public_ip().await;
        if let (Some(baseline), Some(cur)) = (ip_guard::last_official_ip(), &current) {
            if baseline != *cur {
                return Err(ApiError {
                    message: format!(
                        "出口 IP 已变: 上次官方基线 {baseline} → 当前 {cur}。\
                         官方账号从新 IP 访问有封号风险。如已固定新网络出口, \
                         点击「仍然切换」强制继续"
                    ),
                });
            }
        }
    }
    // 切换进度逐步入前端 (配置写入 → 凭证恢复 → 会话隔离)
    let result = profiles::activate_official_with_progress(&|step| {
        use tauri::Emitter;
        let _ = app.emit("switch-progress", step);
    })?;
    // 记录官方激活基线 IP — 必须后台跑: current_public_ip 无缓存时
    // 探测 3 个服务最坏 ~18s, await 会让"切回官方"按钮卡死
    let handle = tauri::async_runtime::spawn(async move {
        let ip = ip_guard::current_public_ip().await;
        let _ = ip_guard::record_official_activation(ip);
    });
    drop(handle);
    // 本地路由开启时, 官方模式无需改写 base_url (还原真实配置)
    local_router::sync_active();
    let _ = app.emit("provider-changed", ());
    Ok(result)
}

/// 出口 IP 类型检测 (数据中心/住宅, 风控风险提示)
#[tauri::command]
async fn check_ip_type() -> Result<ip_guard::IpTypeResult, ApiError> {
    Ok(ip_guard::check_ip_type().await)
}

/// 最近 30 分钟切换次数 (频繁切换告警)
#[tauri::command]
fn get_switch_stats() -> Result<usize, ApiError> {
    Ok(profiles::recent_switch_count(30))
}

#[tauri::command]
async fn activate_relay(
    app: tauri::AppHandle,
    profile_id: String,
) -> Result<ActiveSelection, ApiError> {
    use tauri::Emitter;
    let result = profiles::activate_relay_with_progress(&profile_id, &|step| {
        use tauri::Emitter;
        let _ = app.emit("switch-progress", step);
    })?;
    // 本地路由开启时, 把激活供应商 base_url 改写为本地代理
    local_router::sync_active();
    let _ = app.emit("provider-changed", ());
    Ok(result)
}

#[tauri::command]
fn list_sessions() -> Result<Vec<session_manager::SessionMeta>, ApiError> {
    Ok(session_manager::scan_sessions()?)
}

#[tauri::command]
fn session_detail(
    path: String,
    max_lines: Option<usize>,
) -> Result<Vec<serde_json::Value>, ApiError> {
    Ok(session_manager::session_detail(
        &path,
        max_lines.unwrap_or(500),
    )?)
}

/// 标记/取消标记线程“官方订阅不可见” (该线程所有 rollout 文件一起隔离)。
/// 隔离前必须完全退出 Codex (桌面/CLI), 防止移动正在写入的会话文件;
/// 隔离过程逐步 emit 进度事件。
#[tauri::command]
fn set_session_isolated(
    app: tauri::AppHandle,
    thread_id: String,
    isolated: bool,
) -> Result<(), ApiError> {
    use tauri::Emitter;
    if isolated && session_manager::codex_running() {
        return Err(ApiError {
            message: "请先完全退出 Codex / ChatGPT 桌面端与命令行后再隔离会话".into(),
        });
    }
    let result =
        session_manager::set_session_isolated_with_progress(&thread_id, isolated, &|step| {
            let _ = app.emit("session-isolate-progress", step);
        });
    let _ = app.emit("session-isolate-progress", "完成");
    result.map_err(ApiError::from)
}

/// 扫描仍停留在 "openai" 桶的旧官方会话 (统一历史迁移候选)
#[tauri::command]
fn list_unifiable_sessions() -> Result<Vec<session_unify::UnifySessionMeta>, ApiError> {
    Ok(session_unify::scan_openai_sessions()?)
}

/// 是否存在统一历史迁移备份 (前端据此显示"从备份还原")
#[tauri::command]
fn has_unify_backup() -> Result<bool, ApiError> {
    Ok(session_unify::has_backup())
}

/// 迁移选中线程的旧官方会话到共享 "custom" 桶 (迁移前自动备份)
#[tauri::command]
fn migrate_sessions_to_shared(
    app: tauri::AppHandle,
    thread_ids: Vec<String>,
) -> Result<session_unify::UnifyOutcome, ApiError> {
    use tauri::Emitter;
    let result = session_unify::migrate_selected(&thread_ids, &|step| {
        let _ = app.emit("session-unify-progress", step);
    });
    let _ = app.emit("session-unify-progress", "完成");
    result.map_err(ApiError::from)
}

/// 按迁移备份账本还原旧官方会话到 "openai" 桶
#[tauri::command]
fn restore_unified_sessions(
    app: tauri::AppHandle,
) -> Result<session_unify::UnifyOutcome, ApiError> {
    use tauri::Emitter;
    let result = session_unify::restore_from_backup(&|step| {
        let _ = app.emit("session-unify-progress", step);
    });
    let _ = app.emit("session-unify-progress", "完成");
    result.map_err(ApiError::from)
}

/// 扫描 ~/.codex/pets 下已安装的自定义宠物
#[tauri::command]
fn list_pets() -> Result<Vec<pet_manager::PetMeta>, ApiError> {
    Ok(pet_manager::list_pets()?)
}

/// 导入 ZIP 宠物包 (前端 base64 上传)
#[tauri::command]
fn import_pet_zip(
    file_name: String,
    data_base64: String,
) -> Result<pet_manager::PetMeta, ApiError> {
    Ok(pet_manager::import_zip(&file_name, &data_base64)?)
}

/// 导入宠物文件夹 (webkitdirectory 逐个文件 base64 上传)
#[tauri::command]
fn import_pet_folder(
    files: Vec<pet_manager::PetFileInput>,
) -> Result<pet_manager::PetMeta, ApiError> {
    Ok(pet_manager::import_folder(files)?)
}

/// 删除自定义宠物 (移入金库回收区)
#[tauri::command]
fn delete_pet(pet_id: String) -> Result<(), ApiError> {
    Ok(pet_manager::delete_pet(&pet_id)?)
}

/// 执行用户粘贴的终端安装命令 (npx / curl|sh / git clone 等), 输出流式上抛到
/// "pet-install-output" 事件; 完成后返回新增宠物列表。
#[tauri::command]
async fn install_pet_from_command(
    app: tauri::AppHandle,
    command: String,
) -> Result<Vec<pet_manager::PetMeta>, ApiError> {
    use tauri::Emitter;
    let on_line: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
        std::sync::Arc::new(move |line: &str| {
            let _ = app.emit("pet-install-output", line);
        });
    let handle = tauri::async_runtime::spawn_blocking(move || {
        pet_manager::install_from_command(&command, Some(on_line))
    });
    handle
        .await
        .map_err(|e| ApiError {
            message: format!("安装任务异常: {e}"),
        })?
        .map_err(ApiError::from)
}

/// 取消正在执行的命令安装。
#[tauri::command]
fn cancel_pet_command_install() -> Result<(), ApiError> {
    Ok(pet_manager::cancel_command_install()?)
}

#[tauri::command]
fn list_workflow_agents() -> Result<workflow::WorkflowAgentsResult, ApiError> {
    Ok(workflow::list_workflow_agents()?)
}

/// 高效工作流可选模型列表 (当前供应商模型目录)
#[tauri::command]
fn list_workflow_models() -> Result<Vec<String>, ApiError> {
    Ok(workflow::list_catalog_models())
}

#[tauri::command]
fn install_workflow_preset(kind: String) -> Result<workflow::WorkflowAgentInfo, ApiError> {
    Ok(workflow::install_workflow_preset(&kind)?)
}

#[tauri::command]
fn update_workflow_preset(
    kind: String,
    model: String,
    reasoning_effort: String,
) -> Result<workflow::WorkflowAgentInfo, ApiError> {
    Ok(workflow::update_workflow_preset(
        &kind,
        &model,
        &reasoning_effort,
    )?)
}

#[tauri::command]
fn reset_workflow_presets() -> Result<Vec<workflow::WorkflowAgentInfo>, ApiError> {
    Ok(workflow::reset_workflow_presets()?)
}

#[tauri::command]
fn uninstall_workflow_preset(kind: String) -> Result<workflow::WorkflowActionOutcome, ApiError> {
    Ok(workflow::uninstall_workflow_preset(&kind)?)
}

#[tauri::command]
fn restore_workflow_preset(kind: String) -> Result<workflow::WorkflowAgentInfo, ApiError> {
    Ok(workflow::restore_workflow_preset(&kind)?)
}

/// 退出整个应用 (首次引导关闭按钮使用; 托盘常驻场景下不隐藏, 直接退出)
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/// Codex 桌面/CLI 是否在运行 (前端隔离前预检, 弹悬浮提示用)
#[tauri::command]
fn is_codex_running() -> Result<bool, ApiError> {
    Ok(session_manager::codex_running())
}

/// Codex 桌面端是否已安装 (标准路径 / 运行进程)
#[tauri::command]
fn is_codex_installed() -> Result<bool, ApiError> {
    Ok(codex_install::is_codex_desktop_installed())
}

/// 一键下载并安装 Codex 桌面端 (官方 DMG), 进度上抛到 "codex-install-progress"
#[tauri::command]
async fn install_codex(app: tauri::AppHandle) -> Result<(), ApiError> {
    use tauri::Emitter;
    let handle = app.clone();
    codex_install::install_desktop(move |p| {
        let _ = handle.emit("codex-install-progress", &p);
    })
    .await
    .map_err(|e| ApiError { message: e })
}

#[tauri::command]
async fn check_ip() -> Result<ip_guard::IpCheckResult, ApiError> {
    Ok(ip_guard::check_ip().await)
}

/// DNS 泄露检测 (对齐 ip.net.coffee/dns/ 方法论): 系统解析器查唯一子域名,
/// 对方权威 DNS 记录解析器出口 IP, 与当前出口比对 — 不一致 = DNS 没走代理。
/// 不出网失败返回 error 字段, 命令本身不报错。
#[tauri::command]
async fn check_dns_leak() -> Result<ip_guard::DnsLeakResult, ApiError> {
    Ok(ip_guard::check_dns_leak().await)
}

/// 测试中转连接: GET {base_url}/models + Bearer key, 验证连通与 key 有效性。
#[derive(Serialize)]
struct RelayTestResult {
    ok: bool,
    /// 成功时返回模型数量与列表前几条
    model_count: Option<usize>,
    models: Vec<String>,
    /// 失败时返回原因 + HTTP 状态码
    error: Option<String>,
    status_code: Option<u16>,
}

/// 拉取中转的 /models 列表（OpenAI 兼容格式）。
/// 与 test_relay 同源对齐：base_url 无 /v1 后缀时自动补 /v1。
async fn fetch_relay_models(base_url: &str, key: &str) -> Result<Vec<String>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .map_err(|e| e.to_string())?;
    // 只允许 http(s), 防 key 被发给任意本地服务/非 HTTP 端点
    let parsed = url::Url::parse(base_url.trim_end_matches('/'))
        .map_err(|e| format!("无效 Base URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Base URL 必须是 http(s) 地址".to_string());
    }
    let endpoint_base = parsed.to_string();
    let candidates = balance::candidate_bases(&endpoint_base);
    let mut last_err: Option<String> = None;
    for base in &candidates {
        let endpoint = format!("{base}/models");
        let resp = match client.get(&endpoint).bearer_auth(key).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(format!("GET {endpoint} 网络错误: {e}"));
                continue;
            }
        };
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let snippet: String = body_text.chars().take(200).collect();
            last_err = Some(format!("GET {endpoint} → HTTP {status}: {snippet}"));
            continue;
        }
        let body: serde_json::Value = match serde_json::from_str(&body_text) {
            Ok(b) => b,
            Err(e) => {
                let snippet: String = body_text.chars().take(200).collect();
                last_err = Some(format!(
                    "GET {endpoint} → HTTP {status}, 响应不是 JSON: {e} — 内容: {snippet:?}"
                ));
                continue;
            }
        };
        // OpenAI 兼容: {"data": [{"id": "model-1"}, ...]}
        let models: Vec<String> = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        return Ok(models);
    }
    Err(last_err.unwrap_or_else(|| "获取模型列表失败".to_string()))
}

#[tauri::command]
async fn test_relay(
    base_url: String,
    key: String,
    wire_api: Option<String>,
) -> Result<RelayTestResult, ApiError> {
    let _ = wire_api;
    match fetch_relay_models(&base_url, &key).await {
        Ok(models) => Ok(RelayTestResult {
            ok: true,
            model_count: Some(models.len()),
            models,
            error: None,
            status_code: None,
        }),
        Err(message) => Err(ApiError { message }),
    }
}

/// 当前配置的默认模型 + 思考档位 + 可用模型清单（会话页“用当前模型续聊”用）
#[derive(Serialize)]
struct CurrentModelInfo {
    model: Option<String>,
    reasoning_effort: Option<String>,
    supported_models: Vec<String>,
}

/// 计算切换目标（官方或中转）的默认模型/档位与可用模型清单。
/// 中转没有保存模型清单时在线拉取并回填。
async fn current_remap_target() -> Result<(String, Option<String>, Vec<String>), ApiError> {
    let active = profiles::current_active()?;
    let (model, effort, _) = codex_config::top_level_fields()?;
    match active {
        ActiveSelection::Official => {
            let supported = if session_model::list_catalog_slugs().is_empty() {
                session_model::OFFICIAL_MODELS
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            } else {
                session_model::list_catalog_slugs()
            };
            Ok((
                model.unwrap_or_else(|| "gpt-5.6-sol".to_string()),
                effort,
                supported,
            ))
        }
        ActiveSelection::Relay { profile_id } => {
            let relays = profiles::list_relay_profiles()?;
            let profile = relays
                .iter()
                .find(|p| p.id == profile_id)
                .cloned()
                .ok_or_else(|| ApiError {
                    message: format!("供应商不存在: {profile_id}"),
                })?;
            let mut supported = profile.supported_models.clone();
            if supported.is_empty() {
                if let Ok(Some(key)) = vault::get_relay_key(&profile_id) {
                    if let Ok(models) = fetch_relay_models(&profile.base_url, &key).await {
                        supported = models.clone();
                        let _ = profiles::update_relay_supported_models(&profile_id, models);
                    }
                }
            }
            Ok((
                model.unwrap_or(profile.model),
                effort.or(profile.model_reasoning_effort),
                supported,
            ))
        }
    }
}

/// 切换前预览：目标供应商不支持的旧会话模型清单。
/// profile_id = None 表示官方订阅。
#[tauri::command]
async fn preview_session_model_remap(
    profile_id: Option<String>,
) -> Result<session_model::ModelRemapPreview, ApiError> {
    let (target_model, target_effort, supported, models_unknown) = match profile_id {
        None => {
            let state = vault::load_relay_state();
            let model = state
                .prev_model
                .clone()
                .unwrap_or_else(|| "gpt-5.6-sol".to_string());
            let supported = if session_model::list_catalog_slugs().is_empty() {
                session_model::OFFICIAL_MODELS
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            } else {
                session_model::list_catalog_slugs()
            };
            (model, state.prev_effort.clone(), supported, false)
        }
        Some(id) => {
            let relays = profiles::list_relay_profiles()?;
            let profile = relays
                .iter()
                .find(|p| p.id == id)
                .cloned()
                .ok_or_else(|| ApiError {
                    message: format!("供应商不存在: {id}"),
                })?;
            let mut supported = profile.supported_models.clone();
            let mut unknown = supported.is_empty();
            if supported.is_empty() {
                if let Ok(Some(key)) = vault::get_relay_key(&id) {
                    if let Ok(models) = fetch_relay_models(&profile.base_url, &key).await {
                        supported = models.clone();
                        unknown = false;
                        let _ = profiles::update_relay_supported_models(&id, models);
                    }
                }
            }
            (
                profile.model,
                profile.model_reasoning_effort,
                supported,
                unknown,
            )
        }
    };
    let threads = session_model::incompatible_threads(&supported)?;
    Ok(session_model::ModelRemapPreview {
        threads,
        target_model,
        target_effort,
        supported_models: supported,
        models_unknown,
    })
}

/// 切换完成后执行模型迁移（thread_ids = None 表示迁移全部不兼容会话）。
#[tauri::command]
async fn apply_session_model_remap(
    app: tauri::AppHandle,
    thread_ids: Option<Vec<String>>,
) -> Result<session_model::ModelRemapOutcome, ApiError> {
    let (model, effort, supported) = current_remap_target().await?;
    use tauri::Emitter;
    let handle = app.clone();
    let progress = move |p: session_model::RemapProgress| {
        let _ = handle.emit("session-model-remap-progress", p);
    };
    Ok(session_model::apply_remap(
        thread_ids.as_deref(),
        &model,
        effort.as_deref(),
        &supported,
        &progress,
    )?)
}

/// 会话管理页：把单个会话改为当前供应商默认模型（原模型备份，切回自动恢复）。
#[tauri::command]
async fn remap_single_thread(
    app: tauri::AppHandle,
    thread_id: String,
) -> Result<session_model::ModelRemapOutcome, ApiError> {
    let (model, effort, supported) = current_remap_target().await?;
    use tauri::Emitter;
    let handle = app.clone();
    let progress = move |p: session_model::RemapProgress| {
        let _ = handle.emit("session-model-remap-progress", p);
    };
    Ok(session_model::remap_single_thread(
        &thread_id,
        &model,
        effort.as_deref(),
        &supported,
        &progress,
    )?)
}

/// 当前配置默认模型 / 思考档位 / 可用模型列表
#[tauri::command]
fn get_current_model_info() -> Result<CurrentModelInfo, ApiError> {
    let (model, effort, _) = codex_config::top_level_fields()?;
    Ok(CurrentModelInfo {
        model,
        reasoning_effort: effort,
        supported_models: session_model::list_catalog_slugs(),
    })
}

/// 读取中转 key (编辑表单回填用, cc-switch 对齐 — API Key 字段可见)。
/// key 存 keyring/vault, 不回传列表, 只在编辑指定 profile 时取。
#[tauri::command]
fn get_relay_key(profile_id: String) -> Result<String, ApiError> {
    vault::get_relay_key(&profile_id)
        .map_err(|e| ApiError {
            message: e.to_string(),
        })?
        .ok_or_else(|| ApiError {
            message: "该 profile 没有保存 key".to_string(),
        })
}

/// 查询中转站余额: 厂商专用 API 或 new-api 通用探测。
/// key 从 keyring 读取, UI 不接触明文 key。
#[tauri::command]
async fn get_balance(profile_id: String) -> Result<balance::BalanceInfo, ApiError> {
    let relays = profiles::list_relay_profiles()?;
    let Some(profile) = relays.iter().find(|p| p.id == profile_id) else {
        return Err(ApiError {
            message: "profile 不存在".to_string(),
        });
    };
    let Some(key) = vault::get_relay_key(&profile_id).map_err(|e| ApiError {
        message: e.to_string(),
    })?
    else {
        return Ok(balance::BalanceInfo {
            provider: profile.name.clone(),
            success: false,
            balance: None,
            currency: None,
            total: None,
            used: None,
            error: Some("该 profile 没有保存 key".to_string()),
        });
    };
    // usage script 优先 (profile 自带 → cc-switch DB 按名回填) → 厂商/通用探测
    let usage = (!profile
        .usage_script
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty())
    .then(|| balance::UsageScriptCfg {
        code: profile.usage_script.clone().unwrap_or_default(),
        api_key: profile.usage_api_key.clone(),
        base_url: profile.usage_base_url.clone(),
        access_token: profile.usage_access_token.clone(),
        user_id: profile.usage_user_id.clone(),
        timeout_secs: profile.usage_timeout_secs,
    });
    let info = balance::get_balance(&profile.base_url, &key, usage.as_ref(), &profile.name)
        .await
        .map_err(|e| ApiError {
            message: e.to_string(),
        })?;
    // 成功查询 → 记录本地余额快照 (用量统计)
    if info.success {
        usage_stats::record_balance(
            &profile.id,
            &profile.name,
            info.balance,
            info.currency.clone(),
            info.total,
            info.used,
        );
    }
    Ok(info)
}

/// 用量统计汇总: 各供应商余额历史 + 本地路由请求统计
#[tauri::command]
async fn list_usage_stats() -> usage_stats::UsageOverview {
    tauri::async_runtime::spawn_blocking(usage_stats::overview)
        .await
        .unwrap_or_else(|_| usage_stats::overview())
}

/// 本地路由开关 (启动/停止 127.0.0.1 代理)
#[tauri::command]
async fn set_local_router(enabled: bool) -> Result<local_router::RouterStatus, ApiError> {
    Ok(local_router::set_enabled(enabled)
        .await
        .map_err(|e| ApiError { message: e })?)
}

/// 本地路由状态
#[tauri::command]
fn local_router_status() -> local_router::RouterStatus {
    local_router::status()
}

/// 官方订阅额度 (5 小时/周进度条, cc-switch 对齐): wham/usage + codex-cli UA
#[tauri::command]
async fn get_official_quota() -> Result<official_quota::OfficialQuota, ApiError> {
    official_quota::query_official_quota()
        .await
        .map_err(|e| ApiError { message: e })
}

/// 去重: macOS 冷启动时 get_current 与 on_open_url 可能都报同一 URL,
/// 避免 profile 被重复创建。待处理缓冲: 冷启动时前端 listener 未挂上,
/// emit 丢失, 前端挂载后通过 take_pending_deeplink 拉取。
static HANDLED_DEEPLINKS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static PENDING_DEEPLINK: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 前端挂载后拉取冷启动期间的 deeplink 导入结果 (emit 在 listener 挂载前丢失)
#[tauri::command]
fn take_pending_deeplink() -> Option<String> {
    PENDING_DEEPLINK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// 处理 deeplink 打开事件: codexff://v1/import?...
/// 解析并创建 profile, 然后通知前端刷新。
///
/// 注意: 在后台线程执行导入 — keyring (SecItem) 首次访问会弹 keychain 授权
/// 对话框, 主线程同步调用会冻结整个 UI (输入框卡死, 无法点允许)。
fn handle_deeplink(app: &tauri::AppHandle, url: &str) {
    {
        let mut seen = HANDLED_DEEPLINKS.lock().unwrap_or_else(|e| e.into_inner());
        if seen.iter().any(|u| u == url) {
            return;
        }
        seen.push(url.to_string());
    }
    let handle = app.clone();
    let url = url.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> Result<String, String> {
            let p = profiles::import_from_text(&url).map_err(|e| e.to_string())?;
            Ok(format!("imported:{}", p.name))
        })();
        let msg = match result {
            Ok(v) => v,
            Err(e) => format!("error:{e}"),
        };
        *PENDING_DEEPLINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg.clone());
        use tauri::Emitter;
        let _ = handle.emit("deeplink-result", msg);
        // 常驻模式下窗口可能隐藏 — 导入后唤回, 否则用户看不到结果
        tray::show_main_window(&handle);
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::init();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        // 单实例: 重复启动时聚焦已有实例并退出, 防止旧版本实例残留
        // (旧实例可能没有最新的 Codex 运行检测, 导致切换守卫被绕过)
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            let _ = tray::show_main_window(app);
        }))
        // 状态栏常驻: 点关闭按钮 = 隐藏窗口 + 隐藏 Dock 图标, 只留状态栏;
        // 从状态栏"打开主界面"或点击 Dock 图标唤回时恢复。真正退出走 tray 菜单。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                #[cfg(target_os = "macos")]
                let _ = window.app_handle().set_dock_visibility(false);
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            list_relays,
            add_relay,
            update_relay,
            delete_relay,
            activate_official,
            activate_relay,
            list_sessions,
            session_detail,
            set_session_isolated,
            list_unifiable_sessions,
            has_unify_backup,
            migrate_sessions_to_shared,
            restore_unified_sessions,
            list_pets,
            import_pet_zip,
            import_pet_folder,
            delete_pet,
            install_pet_from_command,
            cancel_pet_command_install,
            list_workflow_agents,
            list_workflow_models,
            install_workflow_preset,
            update_workflow_preset,
            reset_workflow_presets,
            uninstall_workflow_preset,
            restore_workflow_preset,
            quit_app,
            is_codex_running,
            is_codex_installed,
            install_codex,
            check_ip,
            check_dns_leak,
            test_relay,
            get_balance,
            list_usage_stats,
            set_local_router,
            local_router_status,
            get_relay_key,
            preview_session_model_remap,
            apply_session_model_remap,
            remap_single_thread,
            get_current_model_info,
            take_pending_deeplink,
            get_common_config,
            set_common_config,
            get_default_config_toml,
            get_official_quota,
            check_ip_type,
            get_switch_stats,
        ])
        .setup(|app| {
            #[cfg(desktop)]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                // 处理 app 已运行时从浏览器/中转站页面唤起的链接
                // (macOS scheme 注册由 Info.plist 的 CFBundleURLTypes 完成)
                let handle = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    for url in event.urls() {
                        handle_deeplink(&handle, url.as_str());
                    }
                });
                // 启动时检查是否由 deeplink 唤起 (app 未运行时点链接 → 先启动)
                if let Ok(Some(urls)) = app.deep_link().get_current() {
                    let handle = app.handle().clone();
                    for url in urls {
                        handle_deeplink(&handle, url.as_str());
                    }
                }
            }

            // 状态栏常驻: 注册 tray 图标 + 菜单; Dock 图标平时保留
            // (普通 App), 仅用户点窗口关闭按钮时隐藏 (见 on_window_event)
            tray::setup_tray(app.handle())?;

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        // macOS: 点击 Dock 图标唤回主窗口 (窗口被关闭按钮隐藏后,
        // 从 Dock 或状态栏都能重新打开)
        if let tauri::RunEvent::Reopen { .. } = event {
            tray::show_main_window(app_handle);
        }
        // 退出前还原本地路由的 base_url 改写, 避免 Codex 指向已停止的代理
        if matches!(event, tauri::RunEvent::Exit) {
            local_router::shutdown();
        }
    });
}
