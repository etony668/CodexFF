import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  AppStatus,
  BalanceInfo,
  IpTypeResult,
  OfficialQuota,
  QuotaWindow,
  RelayProfile,
  RelayProfileInput,
  addRelay,
  checkIpType,
  deleteRelay,
  errMsg,
  getBalance,
  getOfficialQuota,
  getSwitchStats,
  installCodex,
  isCodexInstalled,
  UsageDailyPoint,
  UsageOverview,
  RouterStatus,
  listUsageStats,
  localRouterStatus,
  setLocalRouter,
  updateRelay,
} from "../api";
import { AddProviderPanel } from "../AddProviderPanel";
import type { ToastRequest } from "../FloatingToast";

interface Props {
  status: AppStatus | null;
  onSwitch: (sel: "official" | string) => Promise<void>;
  onChanged: () => Promise<void>;
  /** 切换进行中 (App 层守卫): 禁用切换按钮并提示, 避免无反馈吞点击 */
  switching?: boolean;
  /** 切换检测进度文案 (如 "检测出口 IP…") */
  switchingLabel?: string;
  /** 请求 App 层统一显示悬浮提示。 */
  onToast?: (toast: ToastRequest) => void;
}

const emptyQuota: OfficialQuota = {
  plan_type: null,
  email: null,
  allowed: false,
  limit_reached: false,
  primary_window: null,
  secondary_window: null,
  error: null,
};

/** 额度进度条 (used_percent → 宽度, >=90% 变红; reset_at 倒计时, 归零自动触发刷新) */
function QuotaBar({
  label,
  w,
  onExpired,
}: {
  label: string;
  w: QuotaWindow;
  onExpired?: () => void;
}) {
  const [now, setNow] = useState(() => Date.now());
  const expiredFired = useRef(false);
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);

  const pct = Math.min(100, Math.max(0, Math.round(w.used_percent)));
  const danger = pct >= 90;
  const resetAtMs = w.reset_at != null ? w.reset_at * 1000 : null;
  let resetTxt: string | null = null;
  if (resetAtMs != null) {
    const remain = Math.max(0, resetAtMs - now);
    if (remain > 0) {
      const h = Math.floor(remain / 3600000);
      const m = Math.floor((remain % 3600000) / 60000);
      const s = Math.floor((remain % 60000) / 1000);
      resetTxt = h > 0 ? `重置 ${h}小时${m}分${s}秒后` : `重置 ${m}分${s}秒后`;
    } else {
      resetTxt = "即将重置…";
      if (onExpired && !expiredFired.current) {
        expiredFired.current = true;
        onExpired();
      }
    }
  } else if (w.reset_after_seconds) {
    resetTxt = `重置 ${Math.round(w.reset_after_seconds / 3600)} 小时后`;
  }
  return (
    <div className="quota-row">
      <div className="quota-label">
        <span>{label}</span>
        <span className={danger ? "warn" : ""}>
          {pct}%{resetTxt ? ` · ${resetTxt}` : ""}
        </span>
      </div>
      <div className="quota-bar">
        <div
          className={`quota-fill${danger ? " danger" : ""}`}
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}

/** 30 天余额趋势迷你图 */
function UsageSparkline({ series }: { series: UsageDailyPoint[] }) {
  const values = series.map((d) => d.balance).filter((v): v is number => v != null);
  const w = 180;
  const h = 34;
  if (values.length === 0) {
    return <span className="dim usage-empty">暂无趋势</span>;
  }
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const pts = series
    .map((d, i) => {
      if (d.balance == null) return null;
      const x = series.length <= 1 ? 0 : (i / (series.length - 1)) * (w - 2) + 1;
      const y = h - 2 - ((d.balance - min) / span) * (h - 6);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .filter((p): p is string => p != null)
    .join(" ");
  return (
    <svg
      className="usage-spark"
      width={w}
      height={h}
      viewBox={`0 0 ${w} ${h}`}
      role="img"
      aria-label="最近30天余额趋势"
    >
      <polyline
        points={pts}
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinejoin="round"
      />
    </svg>
  );
}

export function ProfilesPage({
  status,
  onSwitch,
  onChanged,
  switching,
  switchingLabel,
  onToast,
}: Props) {
  const [busy, setBusy] = useState(false);
  const showError = (title: string, message: string) =>
    onToast?.({ title, message, kind: "warn" });

  // 添加/编辑供应商面板 (cc-switch 式全屏面板 + 预设选择)
  const [panelOpen, setPanelOpen] = useState(false);
  const [editingProfile, setEditingProfile] = useState<RelayProfile | null>(null);

  // 余额: profile_id → 结果
  const [balances, setBalances] = useState<Record<string, BalanceInfo>>({});
  const [balanceLoading, setBalanceLoading] = useState<Set<string>>(new Set());
  const autoFetched = useRef<Set<string>>(new Set());

  // 官方订阅额度 (5 小时/周进度条, wham/usage)
  const [officialQuota, setOfficialQuota] = useState<OfficialQuota | null>(null);
  const [quotaLoading, setQuotaLoading] = useState(false);

  // Codex 桌面端检测 / 一键安装
  const [codexInstalled, setCodexInstalled] = useState<boolean | null>(null);
  const [codexInstalling, setCodexInstalling] = useState(false);
  const [codexProgress, setCodexProgress] = useState<{
    phase: string;
    percent: number;
    message: string;
  } | null>(null);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void isCodexInstalled()
      .then((ok) => {
        if (!disposed) setCodexInstalled(ok);
      })
      .catch(() => {});
    void listen<{ phase: string; percent: number; message: string }>(
      "codex-install-progress",
      (e) => {
        if (disposed || !e.payload) return;
        setCodexProgress(e.payload);
      },
    ).then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  async function doInstallCodex() {
    if (codexInstalling) return;
    setCodexInstalling(true);
    setCodexProgress(null);
    try {
      await installCodex();
      setCodexInstalled(true);
      setCodexProgress({
        phase: "完成",
        percent: 100,
        message: "Codex 桌面端已安装",
      });
    } catch (e) {
      showError("安装失败", errMsg(e));
    } finally {
      setCodexInstalling(false);
    }
  }

  // 用量统计 (余额快照历史 + 本地路由请求统计)
  const [usage, setUsage] = useState<UsageOverview | null>(null);
  const [routerStatus, setRouterStatus] = useState<RouterStatus | null>(null);
  const [routerBusy, setRouterBusy] = useState(false);
  // 乐观翻转: 点击后立即更新开关位置, 后端返回后再确认
  const [pendingEnabled, setPendingEnabled] = useState<boolean | null>(null);
  const routerOn = pendingEnabled ?? routerStatus?.enabled ?? false;
  async function refreshUsage() {
    try {
      setUsage(await listUsageStats());
    } catch {
      // 统计拉取失败不打断页面
    }
  }
  async function refreshRouter() {
    try {
      setRouterStatus(await localRouterStatus());
    } catch {
      // 状态拉取失败保持旧值
    }
  }
  useEffect(() => {
    void refreshUsage();
    void refreshRouter();
  }, []);
  // 故障转移提示: 每 15s 刷新路由状态, 主供应商失败自动切备用时提醒用户
  useEffect(() => {
    const t = setInterval(() => void refreshRouter(), 15000);
    return () => clearInterval(t);
  }, []);

  async function toggleRouter() {
    if (routerBusy) return;
    const next = !routerOn;
    setRouterBusy(true);
    setPendingEnabled(next);
    try {
      const s = await setLocalRouter(next);
      setRouterStatus(s);
      void refreshUsage();
    } catch (e) {
      const message = errMsg(e);
      showError(
        next ? "开启本地路由失败" : "关闭本地路由失败",
        message,
      );
    } finally {
      setRouterBusy(false);
      setPendingEnabled(null);
    }
  }

  // 防封: 出口 IP 类型 (数据中心/住宅) + 30 分钟切换次数
  const [ipType, setIpType] = useState<IpTypeResult | null>(null);
  const [switchCount, setSwitchCount] = useState(0);
  useEffect(() => {
    checkIpType()
      .then(setIpType)
      .catch(() => {});
    getSwitchStats()
      .then(setSwitchCount)
      .catch(() => {});
  }, []);

  async function queryOfficialQuota() {
    setQuotaLoading(true);
    try {
      setOfficialQuota(await getOfficialQuota());
    } catch (e) {
      setOfficialQuota({ ...emptyQuota, error: errMsg(e) });
    } finally {
      setQuotaLoading(false);
    }
  }

  // 每次进入官方态自动查询 (挂载时官方态 / 切到官方 / 切走再切回都触发)
  useEffect(() => {
    if (status?.active?.kind !== "official") return;
    queryOfficialQuota();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status?.active?.kind]);

  // 官方额度自动刷新: 每 10 分钟一次 (wham/usage 是官方客户端同款轻量接口,
  // 10 分钟间隔风险很低; 仅在官方模式且应用打开时轮询, 切走后自动停止)
  useEffect(() => {
    if (status?.active?.kind !== "official") return;
    const timer = setInterval(() => {
      void queryOfficialQuota();
    }, 10 * 60 * 1000);
    return () => clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status?.active?.kind]);

  // 自动查余额: 已保存 key 的 profile 加载即查询, 能取到直接显示; 按钮用于手动刷新
  useEffect(() => {
    (status?.relays ?? []).forEach((p) => {
      if (!p.has_key || autoFetched.current.has(p.id)) return;
      autoFetched.current.add(p.id);
      getBalance(p.id)
        .then((info) => {
          setBalances((prev) => ({ ...prev, [p.id]: info }));
          void refreshUsage();
        })
        .catch((e) => {
          setBalances((prev) => ({
            ...prev,
            [p.id]: {
              provider: p.name,
              success: false,
              balance: null,
              currency: null,
              total: null,
              used: null,
              error: errMsg(e),
            },
          }));
        });
    });
  }, [status?.relays]);

  async function queryBalance(p: RelayProfile) {
    setBalanceLoading((prev) => new Set(prev).add(p.id));
    try {
      const info = await getBalance(p.id);
      setBalances((prev) => ({ ...prev, [p.id]: info }));
      void refreshUsage();
    } catch (e) {
      setBalances((prev) => ({
        ...prev,
        [p.id]: {
          provider: p.name,
          success: false,
          balance: null,
          currency: null,
          total: null,
          used: null,
          error: errMsg(e),
        },
      }));
    } finally {
      setBalanceLoading((prev) => {
        const next = new Set(prev);
        next.delete(p.id);
        return next;
      });
    }
  }

  function startEdit(p: RelayProfile) {
    setEditingProfile(p);
    setPanelOpen(true);
  }

  function startAdd() {
    setEditingProfile(null);
    setPanelOpen(true);
  }

  async function saveProvider(input: RelayProfileInput) {
    if (editingProfile) {
      await updateRelay(editingProfile.id, input);
      // key 可能已变 → 旧余额失效, 允许自动重查
      autoFetched.current.delete(editingProfile.id);
    } else {
      await addRelay(input);
    }
  }

  // 删除确认: Tauri 2 WKWebView 不支持 window.confirm (永远返回 false → 点击无效),
  // 用二次点击代替: 第一次点击进入"确认?"态, 3s 未再点自动还原。
  const [confirmDel, setConfirmDel] = useState<string | null>(null);
  const confirmTimer = useRef<number | null>(null);

  function armDelete(p: RelayProfile) {
    if (confirmDel === p.id) {
      doDelete(p);
      return;
    }
    setConfirmDel(p.id);
    if (confirmTimer.current !== null) window.clearTimeout(confirmTimer.current);
    confirmTimer.current = window.setTimeout(() => setConfirmDel(null), 3000);
  }

  async function doDelete(p: RelayProfile) {
    setBusy(true);
    try {
      await deleteRelay(p.id);
      setConfirmDel(null);
      await onChanged();
    } catch (e) {
      showError("删除供应商失败", errMsg(e));
    } finally {
      setBusy(false);
    }
  }

  const relays = status?.relays ?? [];

  return (
    <div className="page">
      <section className="card">
        <h2>官方订阅</h2>
        <p>
          官方模式 = 直连官方, 零中间层; 与中转共用会话历史桶 (custom),
          互切时同一会话可接续。官方凭证只在此模式下出现在本机。
          使用官方凭证时保持固定网络出口，避免出口频繁变化。
        </p>
        {status?.active?.kind === "official" ? (
          <span className="badge active-badge">✓ 已激活</span>
        ) : (
          <button
            onClick={() => onSwitch("official")}
            disabled={busy || switching}
            className="primary"
          >
            <svg
              className="btn-icon"
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              aria-hidden="true"
            >
              <polyline points="17 1 21 5 17 9" />
              <path d="M3 11V9a4 4 0 0 1 4-4h14" />
              <polyline points="7 23 3 19 7 15" />
              <path d="M21 13v2a4 4 0 0 1-4 4H3" />
            </svg>
            {switching ? (switchingLabel ?? "切换中…") : "切换到官方"}
          </button>
        )}
        {switching && switchingLabel && (
          <p className="hint">正在: {switchingLabel}</p>
        )}
        {!status?.official_login_present && (
          <p className="hint">
            尚未保存官方登录凭证 — 切到官方后运行 <code>codex login</code>,
            凭证将自动存入金库, 之后切换自动恢复。
          </p>
        )}
        {(ipType || switchCount >= 3) && (
          <div className="ip-risk">
            {ipType?.hosting === true && (
              <p className="warn">
                当前出口是数据中心/云厂商 IP{ipType.org ? ` (${ipType.org})` : ""} —
                官方账号用机房 IP 访问是风控高危信号, 建议固定住宅/专属出口。
              </p>
            )}
            {ipType?.error && !ipType.hosting && (
              <p className="hint">出口类型检测失败: {ipType.error}</p>
            )}
            {switchCount >= 3 && (
              <p className="warn">
                近 30 分钟已切换 {switchCount} 次 — 频繁切换导致出口抖动, 增加风控信号。
              </p>
            )}
          </div>
        )}
        {officialQuota && (
          <div className="quota-box">
            {officialQuota.error ? (
              <p className="hint">额度: {officialQuota.error}</p>
            ) : (
              <>
                <div className="quota-head">
                  <span>
                    {officialQuota.plan_type
                      ? `订阅: ${officialQuota.plan_type.toUpperCase()}`
                      : "订阅额度"}
                    {officialQuota.limit_reached && (
                      <span className="warn"> · 已达额度上限</span>
                    )}
                  </span>
                  <button
                    type="button"
                    className="link-btn"
                    onClick={queryOfficialQuota}
                    disabled={quotaLoading}
                  >
                    {quotaLoading ? "刷新中…" : "刷新"}
                  </button>
                </div>
                {officialQuota.primary_window && (
                  <QuotaBar
                    label="周额度"
                    w={officialQuota.primary_window}
                    onExpired={queryOfficialQuota}
                  />
                )}
                {officialQuota.secondary_window && (
                  <QuotaBar
                    label="5 小时额度"
                    w={officialQuota.secondary_window}
                    onExpired={queryOfficialQuota}
                  />
                )}
                {!officialQuota.primary_window && !officialQuota.secondary_window && (
                  <p className="hint">当前计划暂无窗口额度数据</p>
                )}
              </>
            )}
          </div>
        )}
        {codexInstalled === false && (
          <div className="router-card">
            <div className="router-copy">
              <strong>未检测到 Codex 桌面端</strong>
              <span className="hint">
                使用官方订阅需要 Codex 桌面端。点击右侧按钮自动下载官方安装包并安装到「应用程序」。
              </span>
              {codexProgress && (
                <div className="codex-install-progress">
                  <div className="quota-label">
                    <span>{codexProgress.phase}</span>
                    <span>
                      {codexProgress.percent >= 0
                        ? `${Math.round(codexProgress.percent)}%`
                        : "下载中…"}
                    </span>
                  </div>
                  <div className="quota-bar">
                    <div
                      className={`quota-fill${
                        codexProgress.percent < 0 ? " indeterminate" : ""
                      }`}
                      style={{
                        width:
                          codexProgress.percent >= 0
                            ? `${Math.min(100, codexProgress.percent)}%`
                            : "20%",
                      }}
                    />
                  </div>
                  <span className="hint">{codexProgress.message}</span>
                </div>
              )}
            </div>
            <div className="router-actions">
              <button
                type="button"
                className="primary"
                onClick={() => void doInstallCodex()}
                disabled={codexInstalling}
              >
                {codexInstalling ? "安装中…" : "一键下载安装"}
              </button>
            </div>
          </div>
        )}
      </section>

      <section className="card">
        <h2>供应商 ({relays.length})</h2>
        <div className="router-card">
          <div className="router-copy">
            <strong>本地路由</strong>
            <span className="hint">
              供应商故障自动切换、熔断保护、Token 用量记录；自动跟随系统代理。
              开启后需重启 Codex 生效。
            </span>
          </div>
          <div className="router-actions">
            <button
              role="switch"
              aria-checked={routerOn}
              className={`switch${routerOn ? " on" : ""}`}
              onClick={() => void toggleRouter()}
              disabled={routerBusy}
              title={routerOn ? "关闭本地路由" : "开启本地路由"}
            >
              <span className="switch-knob" />
            </button>
            <span className={routerOn ? "ok" : "dim"}>
              {routerBusy ? "处理中…" : routerOn ? "已开启" : "未开启"}
            </span>
          </div>
        </div>
        <p className="hint router-status">
          {routerStatus?.degraded
            ? routerStatus.recovery_message || "会话兼容路由需要恢复"
            : routerStatus?.enabled
            ? `运行中 · 端口 ${routerStatus.port}${
                routerStatus.rewritten
                  ? " · 已接管激活供应商"
                  : " · 未接管（请切换到中转供应商）"
              }${routerStatus.automatic ? " · 会话兼容自动启用" : ""}`
            : ""}
        </p>
        {routerStatus?.enabled && (
          <p className="hint">
            本地路由接管期间请通过本 App 的「退出」菜单退出；用系统命令等方式
            强制退出会导致 Codex 会话短暂断连（重新打开 App 会自动恢复路由）。
          </p>
        )}
        {(() => {
          const fb = routerStatus?.last_fallback;
          if (!fb) return null;
          const [fid, ts] = fb;
          const fresh = Date.now() - ts < 5 * 60 * 1000;
          if (!fresh) return null;
          const fname = relays.find((r) => r.id === fid)?.name ?? fid;
          return (
            <p className="warn">
              检测到主供应商故障，已自动切换到备用供应商「{fname}」。
              请确认当前实际使用的模型与计费符合预期。
            </p>
          );
        })()}
        {relays.length === 0 && (
          <p className="hint">还没有中转 profile, 点下方"添加供应商"或粘贴导入链接。</p>
        )}
        {relays.map((p) => (
          <div key={p.id} className="row-card">
            <div className="row-card-main">
              <strong>{p.name}</strong>
              <code className="mono">{p.base_url}</code>
              <span className="mono dim">
                {p.model || "默认模型"} {p.wire_api ? `· ${p.wire_api}` : ""}
                {p.disable_response_storage ? " · 不存响应" : ""}
              </span>
              {!p.has_key && (
                <span className="warn">
                  <svg
                    width="12"
                    height="12"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    style={{ verticalAlign: "-2px", marginRight: 4 }}
                  >
                    <path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
                    <line x1="12" y1="9" x2="12" y2="13" />
                    <line x1="12" y1="17" x2="12.01" y2="17" />
                  </svg>
                  未保存 key
                </span>
              )}
              {balances[p.id]?.success && (
                <span className={(balances[p.id].balance ?? 0) < 1 ? "warn" : "ok"}>
                  余额: {balances[p.id].balance?.toFixed(2)} {balances[p.id].currency}
                  {balances[p.id].total != null &&
                    ` / 总额 ${balances[p.id].total?.toFixed(2)}`}
                </span>
              )}
              {balances[p.id] && !balances[p.id].success && (
                <span className="error" title={balances[p.id].error ?? ""}>
                  余额查询失败
                </span>
              )}
            </div>
            <div className="row-card-actions">
              <button onClick={() => queryBalance(p)} disabled={balanceLoading.has(p.id)}>
                {balanceLoading.has(p.id) ? "查询中…" : "刷新余额"}
              </button>
              {status?.active?.kind === "relay" && status.active.profile_id === p.id ? (
                <span className="badge active-badge">✓ 已激活</span>
              ) : (
                <button onClick={() => onSwitch(p.id)} disabled={busy || switching}>
                  {switching ? "切换中…" : "切换"}
                </button>
              )}
              <button onClick={() => startEdit(p)} disabled={busy}>
                编辑
              </button>
              <button
                className={confirmDel === p.id ? "danger armed" : "danger"}
                onClick={() => armDelete(p)}
                disabled={busy}
              >
                {confirmDel === p.id ? "确认删除?" : "删除"}
              </button>
            </div>
          </div>
        ))}
        <div className="add-row">
          <button className="primary" onClick={startAdd}>
            ＋ 添加供应商
          </button>
        </div>
      </section>

      <section className="card usage-card">
        <h2>用量统计</h2>
        <p className="hint">
          余额快照本地保存 90 天，点击供应商的「刷新余额」即可记录；
          Token 统计来自 Codex 会话扫描（最近 30 天，本地解析）；
          本地路由开启后还会合并代理日志。
        </p>
        {usage && (
          <>
            <div className="usage-summary">
              <div className="usage-metric">
                <strong>{usage.requests}</strong>
                <span>请求</span>
              </div>
              <div className="usage-metric">
                <strong>{usage.total_tokens.toLocaleString()}</strong>
                <span>Token</span>
              </div>
            </div>
            {usage.session_requests > 0 && (
              <p className="hint">
                其中会话扫描：{usage.session_requests} 请求 /{" "}
                {usage.session_tokens.toLocaleString()} Token
              </p>
            )}
            <div className="usage-rows">
              {usage.providers.length === 0 && (
                <p className="hint usage-empty">还没有余额记录，先点一次「刷新余额」。</p>
              )}
              {usage.providers.map((p) => (
                <div key={p.provider_id} className="usage-row">
                  <div className="usage-row-main">
                    <strong>{p.provider_name}</strong>
                    <span className="dim">
                      {p.latest
                        ? `余额 ${p.latest.balance?.toFixed(2) ?? "-"} ${p.latest.currency ?? ""} · 更新于 ${new Date(p.latest.ts_ms).toLocaleString()}`
                        : "暂无余额记录"}
                    </span>
                  </div>
                  <UsageSparkline series={p.series} />
                </div>
              ))}
            </div>
          </>
        )}
      </section>

      <section className="card settings-card">
        <h3>关于与致谢</h3>
        <p className="hint">
          CodexFF v{status?.version ?? ""} · 本应用部分实现参考 CC Switch
          （MIT License, Copyright Jason Young）。
        </p>
        <details>
          <summary>CC Switch MIT License</summary>
          <pre className="mit-license">
{`MIT License

Copyright (c) Jason Young

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.`}
          </pre>
        </details>
      </section>

      <AddProviderPanel
        open={panelOpen}
        editing={editingProfile}
        onClose={() => setPanelOpen(false)}
        onSave={saveProvider}
        onSaved={onChanged}
        onToast={onToast}
      />
    </div>
  );
}
