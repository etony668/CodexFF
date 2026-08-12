import { invoke } from "@tauri-apps/api/core";

export type ActiveSelection =
  | { kind: "official" }
  | { kind: "relay"; profile_id: string };

export interface RelayProfile {
  id: string;
  name: string;
  base_url: string;
  model: string;
  wire_api: string | null;
  model_reasoning_effort: string | null;
  disable_response_storage: boolean;
  /** 上下文窗口 (token); null = 不写, codex 默认 128k, 官方模型 400k */
  model_context_window: number | null;
  model_auto_compact_token_limit: number | null;
  has_key: boolean;
  // ---- cc-switch 对齐字段 (供应商表单全量) ----
  notes: string | null;
  website_url: string | null;
  /** 自定义完整 auth.json (None = 自动生成 {OPENAI_API_KEY}) */
  auth_json: string | null;
  /** 自定义 config.toml 底稿 (None = 程序化生成); 用户全控, reasoning effort 在此体现 */
  config_toml: string | null;
  /** anthropic 格式认证字段名 (ANTHROPIC_AUTH_TOKEN | ANTHROPIC_API_KEY) */
  anthropic_auth_field: string | null;
  /** 保存时合并全局公共配置片段 */
  use_common_config: boolean;
  /** 该中转 /models 返回的真实模型列表（空 = 未知/未获取） */
  supported_models: string[];
}

/** 供应商表单全量入参 (add/update 共用) */
export interface RelayProfileInput {
  name: string;
  base_url: string;
  model: string;
  /** "openai_chat" | "openai_responses" | "anthropic" */
  wire_api: string | null;
  /** add 必填; update 传 "" = 不修改 */
  key: string | null;
  model_reasoning_effort: string | null;
  disable_response_storage: boolean;
  /** add 可空; update null = 不修改 */
  model_context_window: number | null;
  model_auto_compact_token_limit: number | null;
  notes: string | null;
  website_url: string | null;
  auth_json: string | null;
  config_toml: string | null;
  anthropic_auth_field: string | null;
  use_common_config: boolean;
  /** add 保存测试到的模型列表; update null = 不修改 */
  supported_models: string[] | null;
}

export interface RelayTestResult {
  ok: boolean;
  model_count: number | null;
  models: string[];
  error: string | null;
  status_code: number | null;
}

export interface ModelRemapThread {
  thread_id: string;
  title: string;
  model: string;
  reasoning_effort: string | null;
  last_active_ms: number;
}

export interface ModelRemapPreview {
  threads: ModelRemapThread[];
  target_model: string;
  target_effort: string | null;
  supported_models: string[];
  /** true = 拿不到目标供应商模型清单，无法判断不兼容会话 */
  models_unknown: boolean;
}

export interface ModelRemapOutcome {
  remapped: number;
  restored: number;
  thread_ids: string[];
}

export interface CurrentModelInfo {
  model: string | null;
  reasoning_effort: string | null;
  supported_models: string[];
}

export interface IpCheckResult {
  current_ip: string | null;
  last_official_ip: string | null;
  changed: boolean;
  unknown: boolean;
}

export interface SessionMeta {
  id: string;
  /** session_meta 里的真实线程 ID */
  thread_id: string;
  provider: string;
  title: string;
  model: string;
  last_active_ms: number;
  path: string;
  archived: boolean;
  /** 已标记“官方订阅不可见”(文件在金库隔离区, 官方 CLI 扫不到) */
  isolated: boolean;
  /** 首条用户消息摘要 (内容搜索/预览) */
  preview: string;
  /** 该线程包含的 rollout 文件数 (续聊/子任务已合并) */
  rollups: number;
  /** 线程工作目录 (项目目录, 官方侧边栏按它分组) */
  cwd: string;
  /** 注册项目名 (local-projects 匹配到 cwd 的名称; 空 = 未注册项目) */
  project: string;
}

/** 统一历史迁移候选 (仍停留在 "openai" 桶的旧官方会话) */
export interface UnifySessionMeta {
  id: string;
  thread_id: string;
  title: string;
  path: string;
  archived: boolean;
  size: number;
  last_active_ms: number;
  rollups: number;
}

export interface UnifyOutcome {
  migrated_files: number;
  migrated_rows: number;
  thread_ids: string[];
}

/** Codex 自定义宠物包元信息 */
export interface PetMeta {
  id: string;
  name: string;
  description: string;
  sprite_version: number;
  /** 精灵图绝对路径 (asset 协议预览用) */
  spritesheet_path: string;
  size_bytes: number;
  valid: boolean;
  validation: string;
}

export interface PetFileInput {
  /** 相对路径 (webkitdirectory 的 webkitRelativePath) */
  path: string;
  dataBase64: string;
}

export interface AppStatus {
  active: ActiveSelection | null;
  relays: RelayProfile[];
  official_login_present: boolean;
  ip: IpCheckResult;
  version: string;
}

/** invoke 拒绝值可能是对象 (后端 ApiError {message}) — 提取可读消息, 避免 [object Object] */
export function errMsg(e: unknown): string {
  if (e instanceof Error) return e.message;
  if (typeof e === "string") return e;
  if (e && typeof e === "object") {
    const m = (e as { message?: unknown }).message;
    if (typeof m === "string") return m;
  }
  return String(e);
}

export function getStatus(): Promise<AppStatus> {
  return invoke("get_status");
}

export function addRelay(input: RelayProfileInput): Promise<RelayProfile> {
  return invoke("add_relay", { input });
}

export function updateRelay(
  id: string,
  input: RelayProfileInput,
): Promise<RelayProfile> {
  return invoke("update_relay", { id, input });
}

export function getCommonConfig(): Promise<string | null> {
  return invoke("get_common_config");
}

/** 添加供应商默认 config.toml 底稿: 磁盘通用段 + 预设 provider 段 (null = 自定义) */
export function getDefaultConfigToml(presetConfig: string | null): Promise<string> {
  return invoke("get_default_config_toml", { presetConfig });
}

export function setCommonConfig(snippet: string): Promise<void> {
  return invoke("set_common_config", { snippet });
}

export function testRelay(
  baseUrl: string,
  key: string,
  wireApi: string | null,
): Promise<RelayTestResult> {
  return invoke("test_relay", { baseUrl, key, wireApi });
}

export function takePendingDeeplink(): Promise<string | null> {
  return invoke("take_pending_deeplink");
}

export interface BalanceInfo {
  provider: string;
  success: boolean;
  balance: number | null;
  currency: string | null;
  total: number | null;
  used: number | null;
  error: string | null;
}

export function getBalance(profileId: string): Promise<BalanceInfo> {
  return invoke("get_balance", { profileId });
}

export interface UsageDailyPoint {
  date: string;
  balance: number | null;
}

export interface ProviderUsage {
  provider_id: string;
  provider_name: string;
  latest: {
    ts_ms: number;
    balance: number | null;
    currency: string | null;
    total: number | null;
    used: number | null;
  } | null;
  series: UsageDailyPoint[];
}

export interface UsageOverview {
  providers: ProviderUsage[];
  requests: number;
  total_tokens: number;
  estimated_cost: number;
  last_request_ms: number | null;
  /** 会话扫描统计 (Codex 会话文件, 最近 30 天) */
  session_requests: number;
  session_tokens: number;
  session_cost: number;
}

export interface RouterStatus {
  enabled: boolean;
  port: number;
  rewritten: boolean;
  active_provider: string | null;
}

/** 用量统计汇总 (余额历史 + 本地路由请求统计) */
export function listUsageStats(): Promise<UsageOverview> {
  return invoke("list_usage_stats");
}

/** 本地路由开关 (开启/关闭 127.0.0.1 代理 + base_url 接管) */
export function setLocalRouter(enabled: boolean): Promise<RouterStatus> {
  return invoke("set_local_router", { enabled });
}

/** 本地路由状态 */
export function localRouterStatus(): Promise<RouterStatus> {
  return invoke("local_router_status");
}

/** 读取中转 key (编辑表单回填, cc-switch 对齐) */
export function getRelayKey(profileId: string): Promise<string> {
  return invoke("get_relay_key", { profileId });
}

export function deleteRelay(id: string): Promise<void> {
  return invoke("delete_relay", { id });
}

export interface QuotaWindow {
  used_percent: number;
  limit_window_seconds: number;
  reset_after_seconds: number | null;
  reset_at: number | null;
}

export interface OfficialQuota {
  plan_type: string | null;
  email: string | null;
  allowed: boolean;
  limit_reached: boolean;
  /** 周窗口 (604800s) */
  primary_window: QuotaWindow | null;
  /** 5 小时窗口 (18000s) — Plus 计划可能为 null */
  secondary_window: QuotaWindow | null;
  error: string | null;
}

/** 官方订阅额度 (5 小时/周进度条) */
export function getOfficialQuota(): Promise<OfficialQuota> {
  return invoke("get_official_quota");
}

/** force=true 绕过 IP 基线检查 (前端二次确认后调用) */
export function activateOfficial(force = false): Promise<ActiveSelection> {
  return invoke("activate_official", { force });
}

export interface IpTypeResult {
  ip: string | null;
  org: string | null;
  /** true = 数据中心/云厂商 IP (风控高风险); null = 无法判定 */
  hosting: boolean | null;
  error: string | null;
}

/** 出口 IP 类型检测 (数据中心/住宅) */
export function checkIpType(): Promise<IpTypeResult> {
  return invoke("check_ip_type");
}

/** 最近 30 分钟切换次数 (频繁切换告警) */
export function getSwitchStats(): Promise<number> {
  return invoke("get_switch_stats");
}

export function activateRelay(profileId: string): Promise<ActiveSelection> {
  return invoke("activate_relay", { profileId });
}

/** 切换前预览：目标供应商不支持的旧会话模型清单（null = 官方） */
export function previewSessionModelRemap(
  profileId: string | null,
): Promise<ModelRemapPreview> {
  return invoke("preview_session_model_remap", { profileId });
}

/** 切换完成后迁移旧会话模型（null = 全部不兼容会话） */
export function applySessionModelRemap(
  threadIds: string[] | null,
): Promise<ModelRemapOutcome> {
  return invoke("apply_session_model_remap", { threadIds });
}

/** 会话管理页：单个会话改为当前供应商默认模型 */
export function remapSingleThread(threadId: string): Promise<ModelRemapOutcome> {
  return invoke("remap_single_thread", { threadId });
}

/** 当前配置默认模型 / 思考档位 / 可用模型列表 */
export function getCurrentModelInfo(): Promise<CurrentModelInfo> {
  return invoke("get_current_model_info");
}

export function listSessions(): Promise<SessionMeta[]> {
  return invoke("list_sessions");
}

export function sessionDetail(path: string, maxLines?: number): Promise<unknown[]> {
  return invoke("session_detail", { path, maxLines: maxLines ?? 500 });
}

/** 标记/取消标记线程对官方订阅隔离 (该线程所有 rollout 文件一起) */
export function setSessionIsolated(threadId: string, isolated: boolean): Promise<void> {
  return invoke("set_session_isolated", { threadId, isolated });
}

/** 扫描仍停留在 "openai" 桶的旧官方会话 */
export function listUnifiableSessions(): Promise<UnifySessionMeta[]> {
  return invoke("list_unifiable_sessions");
}

/** 是否存在统一历史迁移备份 */
export function hasUnifyBackup(): Promise<boolean> {
  return invoke("has_unify_backup");
}

/** 迁移选中线程到共享 "custom" 桶 (迁移前自动备份) */
export function migrateSessionsToShared(threadIds: string[]): Promise<UnifyOutcome> {
  return invoke("migrate_sessions_to_shared", { threadIds });
}

/** 按迁移备份账本还原旧官方会话 */
export function restoreUnifiedSessions(): Promise<UnifyOutcome> {
  return invoke("restore_unified_sessions");
}

/** 扫描 ~/.codex/pets 下已安装的自定义宠物 */
export function listPets(): Promise<PetMeta[]> {
  return invoke("list_pets");
}

/** 导入 ZIP 宠物包 (base64 上传) */
export function importPetZip(fileName: string, dataBase64: string): Promise<PetMeta> {
  return invoke("import_pet_zip", { fileName, dataBase64 });
}

/** 导入宠物文件夹 (webkitdirectory 逐个文件上传) */
export function importPetFolder(files: PetFileInput[]): Promise<PetMeta> {
  return invoke("import_pet_folder", { files });
}

/** 删除自定义宠物 (移入金库回收区) */
export function deletePet(petId: string): Promise<void> {
  return invoke("delete_pet", { petId });
}

/** 执行用户粘贴的终端安装命令, 返回新增宠物列表 (npx / curl|sh / git clone 等) */
export function installPetFromCommand(command: string): Promise<PetMeta[]> {
  return invoke("install_pet_from_command", { command });
}

/** 取消正在执行的命令安装 */
export function cancelPetCommandInstall(): Promise<void> {
  return invoke("cancel_pet_command_install");
}

/** Codex 高效工作流预设 Agent 信息 */
export interface WorkflowAgentInfo {
  id: string;
  path: string;
  name: string;
  description: string;
  model: string | null;
  reasoning_effort: string | null;
  installed: boolean;
  preset: boolean;
  /** 已安装的预设文件是否被用户自定义过 */
  customized: boolean;
  backup_exists: boolean;
  modified_ms: number | null;
}

export interface WorkflowAgentsResult {
  codex_home: string;
  agents_dir: string;
  agents: WorkflowAgentInfo[];
}

export interface WorkflowActionOutcome {
  id: string;
  removed: boolean;
  backup_exists: boolean;
}

/** 扫描 ~/.codex/agents 下自定义 Agent 与预设状态 */
export function listWorkflowAgents(): Promise<WorkflowAgentsResult> {
  return invoke("list_workflow_agents");
}

/** 高效工作流可选模型列表 (当前供应商模型目录) */
export function listWorkflowModels(): Promise<string[]> {
  return invoke("list_workflow_models");
}

/** 高效工作流可选模型来源 (供应商或官方订阅) */
export interface WorkflowModelSource {
  id: string;
  name: string;
  models: string[];
}

/** 高效工作流可选模型来源列表 (各供应商已保存模型 + 官方订阅) */
export function listWorkflowModelSources(): Promise<WorkflowModelSource[]> {
  return invoke("list_workflow_model_sources");
}

/** 安装工作流预设 (覆盖前自动备份) */
export function installWorkflowPreset(kind: string): Promise<WorkflowAgentInfo> {
  return invoke("install_workflow_preset", { kind });
}

/** 自定义预设的模型与思考档位 (覆盖前自动备份) */
export function updateWorkflowPreset(
  kind: string,
  model: string,
  reasoningEffort: string,
): Promise<WorkflowAgentInfo> {
  return invoke("update_workflow_preset", { kind, model, reasoningEffort });
}

/** 恢复全部预设到默认模型与档位 */
export function resetWorkflowPresets(): Promise<WorkflowAgentInfo[]> {
  return invoke("reset_workflow_presets");
}

/** 卸载工作流预设 (文件移入备份, 可恢复) */
export function uninstallWorkflowPreset(kind: string): Promise<WorkflowActionOutcome> {
  return invoke("uninstall_workflow_preset", { kind });
}

/** 从备份恢复工作流预设 */
export function restoreWorkflowPreset(kind: string): Promise<WorkflowAgentInfo> {
  return invoke("restore_workflow_preset", { kind });
}

/** 退出整个应用 */
export function quitApp(): Promise<void> {
  return invoke("quit_app");
}

/** 用户确认“仍然退出”: 绕过 Codex 运行中的退出拦截 */
export function forceQuitApp(): Promise<void> {
  return invoke("force_quit_app");
}

/** Codex 桌面/CLI 是否在运行 (隔离前预检) */
export function isCodexRunning(): Promise<boolean> {
  return invoke("is_codex_running");
}

/** Codex 桌面端是否已安装 */
export function isCodexInstalled(): Promise<boolean> {
  return invoke("is_codex_installed");
}

/** 一键下载并安装 Codex 桌面端 (进度事件: codex-install-progress) */
export function installCodex(): Promise<void> {
  return invoke("install_codex");
}

export function checkIp(): Promise<IpCheckResult> {
  return invoke("check_ip");
}

export interface DnsLeakResult {
  /** 本次检测 token (唯一子域名) */
  token: string;
  /** 解析器出口 IP 集合 (对方权威 DNS 观测到的) */
  resolver_ips: string[];
  /** 当前出口 IP */
  current_ip: string | null;
  /** true = 泄露 (任一解析器出口 ≠ 当前出口); null = 无法判定 */
  leaking: boolean | null;
  /** 成功触发的轮数 */
  rounds: number;
  error: string | null;
  /** DoH 保护开启且存活: 系统 DNS 确实指向本地 stub (查询已加密) */
  doh_protected: boolean;
  /** DoH 保护中, stub 最近是否通过系统代理/TUN 隧道出口 */
  dns_via_proxy: boolean | null;
}

export function checkDnsLeak(): Promise<DnsLeakResult> {
  return invoke("check_dns_leak");
}

/** 解析器列表截断显示: 前 N 个 + "+剩余" 后缀。40+ IP 全列会把 header 撑出横向滚动条 */
export function fmtResolverIps(ips: string[], max = 3, sep = "/"): string {
  if (ips.length <= max) return ips.join(sep);
  return `${ips.slice(0, max).join(sep)} +${ips.length - max}更多`;
}
