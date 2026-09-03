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
pub mod prefix_diag;
pub mod process_utils;
pub mod profiles;
pub mod session_manager;
pub mod session_model;
pub mod session_unify;
pub mod session_usage;
#[cfg(test)]
pub mod test_util;
pub mod tray;
pub mod usage_stats;
pub mod vault;
pub mod workflow;

use serde::Serialize;
use std::sync::LazyLock;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use tauri::Manager;
use tokio::sync::Mutex as AsyncMutex;

use profiles::{ActiveSelection, ProfilesError, RelayProfile, RelayProfileInput};

/// 覆盖完整的 async 供应商事务：预检、路由解除、profile 写入、接管验证、
/// 补偿回滚和切换记录都必须串行，不能只锁住中间的同步文件写入阶段。
static PROVIDER_SWITCH_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

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

/// 本地路由自愈节流 (30s): 中转激活 + 会话需要清洗 + Codex 运行中时,
/// 本地路由必须开启做请求层清洗, 防止实例重启/切换流程漏开导致直连中转报错。
static ROUTER_HEAL_LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[tauri::command]
async fn get_status() -> Result<AppStatus, ApiError> {
    let active = profiles::current_active().ok();
    let relays = profiles::list_relay_profiles()?;
    // 官方模式下 codex login 后 vault 可能还没有副本 (原捕获时机 = 切中转那刻)。
    // 每次状态刷新尝试补捕获 — 幂等 (vault 已有副本或非官方凭证形态则跳过)。
    let _ = vault::capture_official_if_missing();
    let official_login_present = vault::restore_has_credentials();
    let ip = ip_guard::check_ip().await;
    // 自愈: 中转激活 + 会话需要清洗 + Codex 运行中 → 确保本地路由开启。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let active_compat_supported = profiles::current_active()
        .ok()
        .and_then(|active| match active {
            ActiveSelection::Relay { profile_id } => relays
                .iter()
                .find(|profile| profile.id == profile_id)
                .map(local_router::profile_supports_lossless_compatibility),
            ActiveSelection::Official => Some(false),
        })
        .unwrap_or(false);
    if now.saturating_sub(ROUTER_HEAL_LAST.load(std::sync::atomic::Ordering::Relaxed)) >= 30
        && !local_router::status().enabled
        && active_compat_supported
        && crate::session_manager::codex_running()
    {
        // 请求层兜底 (reasoning 清洗 + 模型归一化), 中转激活 + Codex 运行中时
        // 必须开启, 否则旧会话绑定官方模型直连中转会被拒。
        if let Ok(_switch_guard) = PROVIDER_SWITCH_LOCK.try_lock() {
            let _ = local_router::set_enabled(true).await;
        }
        ROUTER_HEAL_LAST.store(now, std::sync::atomic::Ordering::Relaxed);
    }
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
async fn add_relay(input: RelayProfileInput) -> Result<RelayProfile, ApiError> {
    let _switch_guard = PROVIDER_SWITCH_LOCK.lock().await;
    Ok(profiles::add_relay_profile(input)?)
}

#[tauri::command]
async fn update_relay(id: String, input: RelayProfileInput) -> Result<RelayProfile, ApiError> {
    let _switch_guard = PROVIDER_SWITCH_LOCK.lock().await;
    let editing_active = matches!(
        profiles::current_active(),
        Ok(ActiveSelection::Relay { ref profile_id }) if profile_id == &id
    );
    if editing_active
        && (local_router::status().enabled
            || local_router::codex_may_depend_on_router()
            || crate::session_manager::codex_running())
    {
        return Err(ApiError {
            message:
                "当前供应商正在使用中。请先完全退出 Codex / ChatGPT 并关闭会话兼容路由，再修改该供应商。"
                    .into(),
        });
    }
    Ok(profiles::update_relay_profile(&id, input)?)
}

#[tauri::command]
fn get_common_config() -> Result<Option<String>, ApiError> {
    Ok(profiles::get_common_config()?)
}

#[tauri::command]
async fn set_common_config(snippet: String) -> Result<(), ApiError> {
    let _switch_guard = PROVIDER_SWITCH_LOCK.lock().await;
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
async fn delete_relay(id: String) -> Result<(), ApiError> {
    let _switch_guard = PROVIDER_SWITCH_LOCK.lock().await;
    Ok(profiles::delete_relay_profile(&id)?)
}

async fn restore_router_after_switch_failure(
    was_active: bool,
    was_automatic: bool,
) -> Result<(), String> {
    if !was_active {
        return Ok(());
    }
    let status = if was_automatic {
        local_router::ensure_session_compatibility(true).await?
    } else {
        local_router::set_enabled(true).await?
    };
    if status.enabled && status.rewritten && !status.degraded {
        Ok(())
    } else {
        Err("路由恢复后未达到可用状态".into())
    }
}

async fn degrade_router_after_incomplete_snapshot(
    should_keep_listener: bool,
    reason: String,
) -> Result<(), String> {
    let mut errors = Vec::new();
    if should_keep_listener {
        if let Err(e) = local_router::prepare_session_compatibility().await {
            errors.push(format!("保留监听器失败: {e}"));
        }
    }
    if let Err(e) = local_router::mark_degraded(reason.clone()) {
        errors.push(format!("保存路由降级状态失败: {e}"));
    }
    if errors.is_empty() {
        Err(reason)
    } else {
        Err(format!("{reason}; {}", errors.join("; ")))
    }
}

async fn restore_router_after_verified_snapshot(
    snapshot_restore: &Result<(), ProfilesError>,
    was_active: bool,
    was_automatic: bool,
) -> Result<(), String> {
    match snapshot_restore {
        Ok(()) => restore_router_after_switch_failure(was_active, was_automatic).await,
        Err(e) => {
            degrade_router_after_incomplete_snapshot(
                was_active || local_router::codex_may_depend_on_router(),
                format!("供应商快照未完整恢复，已禁止兼容路由重新接管: {e}"),
            )
            .await
        }
    }
}

async fn restore_relay_router_after_verified_snapshot(
    snapshot_restore: &Result<(), ProfilesError>,
    router_was_active: bool,
    compatibility_prepared: bool,
) -> Result<(), String> {
    match snapshot_restore {
        Ok(()) if router_was_active => local_router::sync_active().map(|_| ()),
        Ok(()) if compatibility_prepared => local_router::cancel_prepared_compatibility(),
        Ok(()) => Ok(()),
        Err(e) => {
            degrade_router_after_incomplete_snapshot(
                router_was_active
                    || compatibility_prepared
                    || local_router::codex_may_depend_on_router(),
                format!("供应商快照未完整恢复，已禁止兼容路由重新接管: {e}"),
            )
            .await
        }
    }
}

/// 激活官方 profile。激活前 IP 硬检查: 有基线且当前出口 ≠ 基线 →
/// 拒绝 (除非 force=true, 前端确认后重试)。激活后后台记录出口 IP 作为基线。
#[tauri::command]
async fn activate_official(
    app: tauri::AppHandle,
    force: Option<bool>,
) -> Result<ActiveSelection, ApiError> {
    use tauri::Emitter;
    let _switch_guard = PROVIDER_SWITCH_LOCK.lock().await;
    let profile_kind = codex_config::current_profile_kind();
    let active_identity = profiles::current_active();
    let safe_known_official = matches!(profile_kind, Ok(codex_config::CurrentProfile::Official))
        && matches!(active_identity, Ok(ActiveSelection::Official));
    if !safe_known_official && crate::session_manager::codex_running() {
        return Err(ApiError {
            message:
                "请先完全退出 Codex / ChatGPT，再切换官方订阅。当前供应商身份为第三方或存在不一致，必须确保第三方地址与凭证已从进程内退出。"
                    .to_string(),
        });
    }
    // 账号被风控主因 = 官方账号活跃 IP 变化。基线存在且不一致 → 拦截,
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
    let router_before = local_router::status();
    let router_was_active = router_before.enabled || local_router::codex_may_depend_on_router();
    let switch_snapshot = profiles::capture_switch_snapshot()?;
    if router_was_active {
        local_router::shutdown().map_err(|e| ApiError {
            message: format!("切换官方前无法安全退出会话兼容路由: {e}"),
        })?;
    }
    // 切换进度逐步入前端 (配置写入 → 凭证恢复 → 会话隔离)
    let result = match profiles::activate_official_with_progress(&|step| {
        use tauri::Emitter;
        let _ = app.emit("switch-progress", step);
    }) {
        Ok(result) => result,
        Err(e) => {
            let rollback = profiles::restore_switch_snapshot(&switch_snapshot);
            let router_rollback = restore_router_after_verified_snapshot(
                &rollback,
                router_was_active,
                router_before.automatic,
            )
            .await;
            return Err(ApiError {
                message: format!(
                    "切换官方失败: {e}; 状态回滚: {rollback:?}; 路由回滚: {router_rollback:?}"
                ),
            });
        }
    };
    // 官方模式不再需要本地路由: 彻底关闭 (不还原中转 base_url, 官方 config 已生效)
    if let Err(e) = local_router::disable_for_official().await {
        let rollback = profiles::restore_switch_snapshot(&switch_snapshot);
        let router_rollback = restore_router_after_verified_snapshot(
            &rollback,
            router_was_active,
            router_before.automatic,
        )
        .await;
        return Err(ApiError {
            message: format!(
                "切换官方后无法安全关闭会话兼容路由: {e}; 状态回滚: {rollback:?}; 路由回滚: {router_rollback:?}"
            ),
        });
    }
    // 记录官方激活基线 IP — 必须后台跑: current_public_ip 无缓存时
    // 探测 3 个服务最坏 ~18s, await 会让"切回官方"按钮卡死
    let handle = tauri::async_runtime::spawn(async move {
        let ip = ip_guard::current_public_ip().await;
        let _ = ip_guard::record_official_activation(ip);
    });
    drop(handle);
    if let Err(e) = profiles::record_switch() {
        log::warn!("记录官方切换历史失败: {e}");
        let _ = app.emit(
            "switch-warning",
            format!("供应商已切换，但切换历史记录失败，频繁切换告警可能不准确: {e}"),
        );
    }
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
    let _switch_guard = PROVIDER_SWITCH_LOCK.lock().await;
    let target_profile = profiles::list_relay_profiles()?
        .into_iter()
        .find(|p| p.id == profile_id)
        .ok_or_else(|| ApiError {
            message: format!("profile 不存在: {profile_id}"),
        })?;
    let mut target_models = target_profile.supported_models.clone();
    let target_wire = target_profile.wire_api.as_deref().unwrap_or("openai_chat");
    if target_models.is_empty() {
        if let Ok(Some(key)) = vault::get_relay_key(&profile_id) {
            if let Ok(models) = fetch_relay_models(&target_profile.base_url, &key).await {
                target_models = models.clone();
                let _ = profiles::update_relay_supported_models(&profile_id, models);
            }
        }
    }
    let has_history = session_model::has_historical_threads().unwrap_or(true);
    let model_incompatible =
        session_model::has_incompatible_threads(&target_models).unwrap_or(true);
    let compatibility_required = has_history || model_incompatible;
    if (compatibility_required || local_router::codex_may_depend_on_router())
        && !matches!(target_wire, "responses" | "openai_responses")
    {
        return Err(ApiError {
            message: format!(
                "当前供应商使用 {target_wire} 协议。历史会话无损接续目前仅支持 Responses 协议；为避免损坏会话或错误认证，本次切换已取消。"
            ),
        });
    }
    let router_already_active = {
        let status = local_router::status();
        status.enabled
            && status.rewritten
            && !status.degraded
            && local_router::codex_points_at_router()
    };
    if !router_already_active && crate::session_manager::codex_running() {
        return Err(ApiError {
            message:
                "请先完全退出 Codex / ChatGPT，再切换第三方供应商。首次切换会安全启用会话兼容层；下次启动后历史会话可直接续聊，且不会修改原始会话文件。"
                    .to_string(),
        });
    }
    let switch_snapshot = profiles::capture_switch_snapshot()?;
    let compatibility_prepared = if compatibility_required && !router_already_active {
        local_router::prepare_session_compatibility()
            .await
            .map_err(|e| ApiError {
                message: format!("无法启动会话兼容层，本次未切换供应商: {e}"),
            })?
    } else {
        false
    };
    if router_already_active {
        local_router::prepare_provider_switch().map_err(|e| ApiError {
            message: format!("无法解除旧供应商的兼容路由接管，本次未切换: {e}"),
        })?;
    }
    let result = match profiles::activate_relay_with_progress(&profile_id, &|step| {
        use tauri::Emitter;
        let _ = app.emit("switch-progress", step);
    }) {
        Ok(result) => result,
        Err(e) => {
            let rollback = profiles::restore_switch_snapshot(&switch_snapshot);
            let router_rollback = restore_relay_router_after_verified_snapshot(
                &rollback,
                router_already_active,
                compatibility_prepared,
            )
            .await;
            return Err(ApiError {
                message: format!(
                    "供应商切换失败: {e}; 状态回滚: {rollback:?}; 路由回滚: {router_rollback:?}"
                ),
            });
        }
    };
    if compatibility_required {
        let _ = app.emit("switch-progress", "启用会话兼容层…");
        let router = match local_router::ensure_session_compatibility(true).await {
            Ok(router) if router.enabled && router.rewritten && !router.degraded => router,
            Ok(_) => {
                let e = "兼容路由未完成接管".to_string();
                let rollback = profiles::restore_switch_snapshot(&switch_snapshot);
                let router_rollback = restore_relay_router_after_verified_snapshot(
                    &rollback,
                    router_already_active,
                    compatibility_prepared,
                )
                .await;
                return Err(ApiError {
                    message: format!(
                        "供应商切换已回滚: {e}; 状态回滚: {rollback:?}; 路由回滚: {router_rollback:?}"
                    ),
                });
            }
            Err(e) => {
                let rollback = profiles::restore_switch_snapshot(&switch_snapshot);
                let router_rollback = restore_relay_router_after_verified_snapshot(
                    &rollback,
                    router_already_active,
                    compatibility_prepared,
                )
                .await;
                return Err(ApiError {
                    message: format!(
                        "会话兼容层接管失败，供应商切换已回滚: {e}; 状态回滚: {rollback:?}; 路由回滚: {router_rollback:?}"
                    ),
                });
            }
        };
        let _ = app.emit("router-status", router);
    } else if local_router::status().enabled {
        if let Err(e) = local_router::sync_active() {
            let rollback = profiles::restore_switch_snapshot(&switch_snapshot);
            let router_rollback = restore_relay_router_after_verified_snapshot(
                &rollback,
                router_already_active,
                compatibility_prepared,
            )
            .await;
            return Err(ApiError {
                message: format!(
                    "本地路由跟随供应商失败，切换已回滚: {e}; 状态回滚: {rollback:?}; 路由回滚: {router_rollback:?}"
                ),
            });
        }
    }
    if let Err(e) = profiles::record_switch() {
        log::warn!("记录第三方切换历史失败: {e}");
        let _ = app.emit(
            "switch-warning",
            format!("供应商已切换，但切换历史记录失败，频繁切换告警可能不准确: {e}"),
        );
    }
    let _ = app.emit("provider-changed", ());
    Ok(result)
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

/// 高效工作流可选模型来源 (各供应商已保存模型 + 官方订阅)
#[tauri::command]
fn list_workflow_model_sources() -> Result<Vec<workflow::WorkflowModelSource>, ApiError> {
    Ok(workflow::list_model_sources())
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

/// Codex 桌面端是否已安装。
#[tauri::command]
fn is_codex_installed() -> Result<bool, ApiError> {
    Ok(codex_install::is_codex_desktop_installed())
}

/// 获取桌面端 / CLI 当前版本，可选联网检查官方最新版本。
#[tauri::command]
async fn codex_install_status(
    check_latest: Option<bool>,
) -> Result<codex_install::CodexInstallStatus, ApiError> {
    Ok(codex_install::install_status(check_latest.unwrap_or(false)).await)
}

/// 一键补齐 Codex 桌面端与 CLI。
#[tauri::command]
async fn install_codex(app: tauri::AppHandle) -> Result<(), ApiError> {
    use tauri::Emitter;
    let handle = app.clone();
    codex_install::install_all(move |p| {
        let _ = handle.emit("codex-install-progress", &p);
    })
    .await
    .map_err(|e| ApiError { message: e })
}

/// 更新 Codex 桌面端。
#[tauri::command]
async fn update_codex_desktop(app: tauri::AppHandle) -> Result<(), ApiError> {
    use tauri::Emitter;
    let handle = app.clone();
    codex_install::install_desktop(true, move |p| {
        let _ = handle.emit("codex-install-progress", &p);
    })
    .await
    .map_err(|e| ApiError { message: e })
}

/// 安装或更新 Codex CLI。
#[tauri::command]
async fn update_codex_cli(app: tauri::AppHandle) -> Result<(), ApiError> {
    use tauri::Emitter;
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        codex_install::install_cli(true, move |p| {
            let _ = handle.emit("codex-install-progress", &p);
        })
    })
    .await
    .map_err(|e| ApiError {
        message: format!("CLI 安装任务失败: {e}"),
    })?
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
    let _switch_guard = PROVIDER_SWITCH_LOCK.lock().await;
    if enabled && session_model::has_historical_threads().unwrap_or(true) {
        if let ActiveSelection::Relay { profile_id } = profiles::current_active()? {
            let profile = profiles::list_relay_profiles()?
                .into_iter()
                .find(|profile| profile.id == profile_id)
                .ok_or_else(|| ApiError {
                    message: "当前第三方供应商记录不存在".into(),
                })?;
            if !local_router::profile_supports_lossless_compatibility(&profile) {
                return Err(ApiError {
                    message: "当前供应商不是 Responses 协议，不能开启历史会话兼容路由".into(),
                });
            }
        }
    }
    Ok(local_router::set_enabled(enabled)
        .await
        .map_err(|e| ApiError { message: e })?)
}

#[tauri::command]
fn set_local_router_auto_failover(enabled: bool) -> Result<local_router::RouterStatus, ApiError> {
    local_router::set_auto_failover_enabled(enabled).map_err(|e| ApiError { message: e })
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
    tauri::async_runtime::spawn(async move {
        let _switch_guard = PROVIDER_SWITCH_LOCK.lock().await;
        let result = tauri::async_runtime::spawn_blocking(move || {
            profiles::import_from_text(&url)
                .map(|p| format!("imported:{}", p.name))
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(format!("导入任务失败: {e}")));
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
            list_pets,
            import_pet_zip,
            import_pet_folder,
            delete_pet,
            install_pet_from_command,
            cancel_pet_command_install,
            list_workflow_agents,
            list_workflow_models,
            list_workflow_model_sources,
            install_workflow_preset,
            update_workflow_preset,
            reset_workflow_presets,
            uninstall_workflow_preset,
            restore_workflow_preset,
            quit_app,
            is_codex_running,
            is_codex_installed,
            codex_install_status,
            install_codex,
            update_codex_desktop,
            update_codex_cli,
            check_ip,
            check_dns_leak,
            test_relay,
            get_balance,
            list_usage_stats,
            set_local_router,
            set_local_router_auto_failover,
            local_router_status,
            get_relay_key,
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

            // 恢复上一实例仍被 Codex 缓存的兼容路由；若不存在有效接管，
            // 清理陈旧 localhost 状态。整个过程不改写任何历史会话。
            if let Err(e) =
                tauri::async_runtime::block_on(local_router::resume_or_recover_startup())
            {
                log::warn!("启动恢复会话兼容路由失败: {e}");
            }
            // 会话管理已退役：仅在 Codex/ChatGPT 完全退出时执行一次轻量归属恢复，
            // 后续不再迁移、隔离或持续备份会话正文。
            if let Err(e) = session_unify::retire_session_management(&|step| {
                log::info!("session management retirement: {step}");
            }) {
                log::warn!("会话管理退役迁移失败，将在下次启动重试: {e}");
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        // macOS: 点击 Dock 图标唤回主窗口 (窗口被关闭按钮隐藏后,
        // 从 Dock 或状态栏都能重新打开)
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            tray::show_main_window(app_handle);
        }
        // 退出拦截: 本地路由正在为 Codex 转发时, 直接退出会让 Codex 下一请求
        // 打到已关闭的本地端口 → 502/会话连接失败。提示用户先退出 Codex;
        // 只要 Codex 仍依赖本地路由，就必须阻止退出，避免下一请求命中
        // 已关闭的 19331 端口。不存在绕过此保护的“强制退出”入口。
        if let tauri::RunEvent::ExitRequested { ref api, .. } = event {
            use tauri::Emitter;
            if local_router::codex_may_depend_on_router() && crate::session_manager::codex_running()
            {
                api.prevent_exit();
                tray::show_main_window(app_handle);
                let _ = app_handle.emit("exit-blocked", ());
            }
        }
        // 退出前先关闭本地路由 (还原 base_url + 停端口), 避免 Codex 会话
        // 在 App 退出后仍指向已停止的本地代理而连接失败
        if matches!(event, tauri::RunEvent::Exit) {
            if local_router::codex_may_depend_on_router() && crate::session_manager::codex_running()
            {
                log::warn!(
                    "退出时 Codex 仍在运行且指向本地路由 — 本次退出可能由外部途径触发, \
                     Codex 会话将在下次请求时短暂断连; 重新打开 App 会自动恢复路由"
                );
            }
            if local_router::status().enabled || local_router::codex_may_depend_on_router() {
                let _ = local_router::shutdown();
            }
        }
    });
}
