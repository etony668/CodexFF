import { useEffect, useRef, useState } from "react";
import "./App.css";
import {
  AppStatus,
  DnsLeakResult,
  activateOfficial,
  activateRelay,
  applySessionModelRemap,
  checkDnsLeak,
  checkIp,
  errMsg,
  forceQuitApp,
  getStatus,
  isCodexRunning,
  previewSessionModelRemap,
  takePendingDeeplink,
} from "./api";
import { openUrl } from "@tauri-apps/plugin-opener";
import { listen } from "@tauri-apps/api/event";
import { ProfilesPage } from "./pages/ProfilesPage";
import { SessionsPage } from "./pages/SessionsPage";
import { PetsPage } from "./pages/PetsPage";
import { SettingsPage } from "./pages/SettingsPage";
import { WorkflowPage } from "./pages/WorkflowPage";
import { FloatingToast, type ToastRequest } from "./FloatingToast";
import { FirstRunGuide } from "./FirstRunGuide";

type Tab = "profiles" | "sessions" | "workflow" | "pets" | "settings";

function App() {
  const [tab, setTab] = useState<Tab>("profiles");
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [exitBlocked, setExitBlocked] = useState(false);

  // 请求序号: get_status 含出网 IP 检测 (最坏 ~6s), 并发时慢响应
  // 不得覆盖新响应 (切换后 UI 显示旧状态)
  const refreshSeq = useRef(0);

  async function refresh() {
    const seq = ++refreshSeq.current;
    try {
      const s = await getStatus();
      if (seq !== refreshSeq.current) return; // 已有更新的刷新, 丢弃旧结果
      setStatus(s);
      setError(null);
    } catch (e) {
      if (seq !== refreshSeq.current) return;
      setError(errMsg(e));
    }
  }

  // 监听中转站 deeplink 导入结果 (codexff:// 链接唤起)
  useEffect(() => {
    const unlisten = listen<string>("deeplink-result", (e) => {
      const msg = e.payload;
      if (msg.startsWith("imported:")) {
        setNotice(`✓ 已通过链接导入 "${msg.slice(9)}"`);
        setError(null);
      } else {
        setError(msg);
      }
      setTab("profiles");
      refresh();
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  useEffect(() => {
    refresh();
  }, []);

  // 出口 IP 实时跟随网络变化: 每 20s 轮询, 窗口重新聚焦时立即刷新。
  // 后端在路由/utun 接口变化时强制绕过缓存重查, 机场切节点后界面会自动更新。
  useEffect(() => {
    async function pollExitIp() {
      try {
        const ip = await checkIp();
        setStatus((prev) => (prev ? { ...prev, ip } : prev));
      } catch {
        // 网络抖动时保持旧值, 下轮重试
      }
    }
    const t = setInterval(() => void pollExitIp(), 20000);
    const onFocus = () => void pollExitIp();
    window.addEventListener("focus", onFocus);
    return () => {
      clearInterval(t);
      window.removeEventListener("focus", onFocus);
    };
  }, []);

  // 首次运行: 未确认过隐私说明时弹出三页引导, 确认后写本地标记
  useEffect(() => {
    try {
      if (!localStorage.getItem("codexff-firstrun-consent")) {
        setFirstRunOpen(true);
      }
    } catch {
      // localStorage 不可用时仍展示一次
      setFirstRunOpen(true);
    }
  }, []);

  function finishFirstRun() {
    try {
      localStorage.setItem("codexff-firstrun-consent", "1");
    } catch {
      // 忽略写入失败
    }
    setFirstRunOpen(false);
  }

  // 冷启动 deeplink: listener 挂载前 emit 丢失, 从后端拉取待处理结果
  useEffect(() => {
    takePendingDeeplink()
      .then((msg) => {
        if (!msg) return;
        if (msg.startsWith("imported:")) {
          setNotice(`✓ 已通过链接导入 "${msg.slice(9)}"`);
          setError(null);
        } else {
          setError(msg);
        }
        setTab("profiles");
        refresh();
      })
      .catch((e) => setError(errMsg(e)));
  }, []);

  const [switching, setSwitching] = useState(false);
  // 切换检测进度 (官方模式: 出口 IP → 基线比对 → 写入配置)
  const [switchStep, setSwitchStep] = useState<string | null>(null);

  // 切换实际执行：自动把不兼容旧会话迁移到当前供应商模型（无需手动操作）
  async function performSwitch(sel: "official" | string, force: boolean) {
    if (switching) return; // 防双击并发切换 (config 读写非原子, 会互相覆盖)
    setSwitching(true);
    try {
      // 1. 先检查旧会话模型兼容性
      setSwitchStep("检查旧会话模型…");
      const preview = await previewSessionModelRemap(sel === "official" ? null : sel);
      const needMigrate = preview.threads.length > 0 && !preview.models_unknown;
      // 请求层归一化兜底: Codex 运行中也允许切换, 本地路由会把请求 model
      // 改写为当前供应商默认模型, 旧会话无需退出即可接续。
      const codexRunning = needMigrate
        ? await isCodexRunning().catch(() => false)
        : false;

      // 2. 写配置
      if (sel === "official") {
        setSwitchStep("写入官方配置与凭证…");
        await activateOfficial(force);
      } else {
        setSwitchStep("写入中转配置与凭证…");
        await activateRelay(sel);
      }
      const relayName =
        sel === "official"
          ? "官方订阅"
          : status?.relays.find((r) => r.id === sel)?.name ?? sel;

      // 3. 自动迁移全部不兼容旧会话 (Codex 运行中跳过 — 由本地路由请求层归一化)
      if (needMigrate && !codexRunning) {
        setSwitchStep("迁移旧会话模型…");
        const ids = preview.threads.map((t) => t.thread_id);
        try {
          const out = await applySessionModelRemap(ids);
          setSwitchStep(null);
          await refresh();
          setNotice(
            `已切换到 ${relayName}，${out.remapped + out.restored} 个旧会话已自动改用当前模型，可以直接接续。`
          );
        } catch (e) {
          setSwitchStep(null);
          await refresh();
          setNotice(
            `已切换到 ${relayName}，但旧会话模型迁移失败：${errMsg(e)}。`
          );
        }
      } else if (needMigrate && codexRunning) {
        setSwitchStep(null);
        await refresh();
        setNotice(
          `已切换到 ${relayName}。${preview.threads.length} 个旧会话模型由本地路由自动适配，可以直接接续。`
        );
      } else {
        setSwitchStep(null);
        await refresh();
        setNotice(`已切换到 ${relayName}。`);
      }
    } catch (e) {
      setError(errMsg(e));
    } finally {
      setSwitching(false);
      setSwitchStep(null);
    }
  }

  async function switchTo(sel: "official" | string) {
    if (switching) return;
    if (sel !== "official") {
      await performSwitch(sel, false);
      return;
    }
    setSwitching(true);
    try {
      // IP 硬检查分步: 前端先跑检测并显示进度, 比对不一致 → 自定义悬浮
      // 确认提示 (不自动消失), 用户确认后 force 调用。
      setSwitchStep("检测出口 IP…");
      const ip = await checkIp();
      if (ip.current_ip) {
        setSwitchStep("比对官方激活基线…");
        if (ip.changed && ip.last_official_ip) {
          const msg = `出口 IP 已变: 上次官方基线 ${ip.last_official_ip} → 当前 ${ip.current_ip}。官方账号从新 IP 访问有封号风险。`;
          setSwitching(false);
          setSwitchStep(null);
          setConfirmState({
            title: "出口 IP 已变",
            message: `${msg} 确定要继续切换到官方吗?`,
            confirmLabel: "仍然切换",
            onConfirm: () => {
              setConfirmState(null);
              void performSwitch("official", true);
            },
          });
          return;
        }
      }
      setSwitching(false);
      setSwitchStep(null);
      await performSwitch("official", false);
    } catch (e) {
      setSwitching(false);
      setSwitchStep(null);
      setError(errMsg(e));
    }
  }

  const active = status?.active;
  const activeLabel = (() => {
    if (!active) return "未接管";
    if (active.kind === "official") return "官方订阅";
    const relay = status?.relays.find((r) => r.id === active.profile_id);
    return relay?.name ?? active.profile_id;
  })();

  const ipWarn =
    status?.ip.changed &&
    `出口 IP 已变: ${status.ip.current_ip} (上次官方 ${status.ip.last_official_ip})`;

  // DNS 泄露检测 (对齐 ip.net.coffee/dns/ 方法论): 挂载时查一次, 可手动重查
  const [dnsLeak, setDnsLeak] = useState<DnsLeakResult | null>(null);
  const [dnsChecking, setDnsChecking] = useState(false);
  const [confirmState, setConfirmState] = useState<{
    title: string;
    message: string;
    confirmLabel?: string;
    cancelLabel?: string;
    extraActions?: { label: string; onClick: () => void }[];
    onConfirm: () => void;
  } | null>(null);
  // 子页面请求的通用悬浮提示 (统一在 App 的提示容器里, 避免多个容器重叠)
  const [pageToast, setPageToast] = useState<ToastRequest | null>(null);
  // 首次运行引导 (钥匙串/授权弹窗说明 + 隐私确认)
  const [firstRunOpen, setFirstRunOpen] = useState(false);
  // 主题: 默认跟随系统, 用户可手动切换 (auto=跟随系统 / light / dark), 记忆到本地
  const [themePref, setThemePref] = useState<"auto" | "light" | "dark">(() => {
    try {
      const saved = localStorage.getItem("codexff-theme");
      if (saved === "auto" || saved === "light" || saved === "dark") return saved;
    } catch {
      // localStorage 不可用时默认跟随系统
    }
    return "auto";
  });
  // 系统当前外观 (auto 模式实时跟随)
  const [systemDark, setSystemDark] = useState(
    () => window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? false,
  );
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => setSystemDark(mq.matches);
    apply();
    mq.addEventListener("change", apply);
    return () => mq.removeEventListener("change", apply);
  }, []);
  const appliedDark =
    themePref === "dark" || (themePref === "auto" && systemDark);
  useEffect(() => {
    document.documentElement.dataset.theme = appliedDark ? "dark" : "light";
    try {
      localStorage.setItem("codexff-theme", themePref);
    } catch {
      // 持久化失败不影响本次会话
    }
  }, [appliedDark, themePref]);
  function cycleTheme() {
    setThemePref((p) => (p === "auto" ? "light" : p === "light" ? "dark" : "auto"));
  }
  const runDnsCheck = async () => {
    if (dnsChecking) return;
    setDnsChecking(true);
    try {
      const r = await checkDnsLeak();
      setDnsLeak(r);
    } catch (e) {
      setDnsLeak({ token: "", resolver_ips: [], current_ip: null, leaking: null, rounds: 0, error: errMsg(e), doh_protected: false, dns_via_proxy: null });
    } finally {
      setDnsChecking(false);
    }
  };
  useEffect(() => {
    runDnsCheck();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 后端切换进度 (官方: 配置写入 → 凭证恢复 → 会话隔离; 中转: 会话恢复)
  useEffect(() => {
    const unlisten = listen<string>("switch-progress", (e) => {
      setSwitchStep(e.payload);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // 退出拦截: 路由开启 + Codex 运行中时, 后端阻止退出并通知前端确认
  useEffect(() => {
    const unlisten = listen("exit-blocked", () => {
      setExitBlocked(true);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // 会话模型迁移进度（并发改写大文件时逐文件上报）
  useEffect(() => {
    const unlisten = listen<{
      done: number;
      total: number;
      current: string | null;
    }>("session-model-remap-progress", (e) => {
      const p = e.payload;
      setSwitchStep(
        `迁移旧会话模型（${p.done}/${p.total}）${p.current ? ` · ${p.current}` : ""}…`
      );
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  return (
    <div className="app">
      <header className="app-header">
        <h1>CodexFF</h1>
        <div className="banner-stack">
          <div className="banner-row">
            <div className="active-badge" title={ipWarn || ""}>
              当前: {activeLabel}
            </div>
            {/* DNS 泄露检测 */}
            <button
              className={`tab dns-check${dnsLeak?.leaking === true ? " leaking" : ""}`}
              onClick={runDnsCheck}
              title="点击重新检测 DNS 泄露"
              disabled={dnsChecking}
            >
              {dnsChecking
                ? "DNS 检测中…"
                : dnsLeak?.leaking === true
                  ? (
                      <>
                        <svg
                          width="14"
                          height="14"
                          viewBox="0 0 24 24"
                          fill="none"
                          stroke="currentColor"
                          strokeWidth="2"
                          strokeLinecap="round"
                          strokeLinejoin="round"
                          style={{ verticalAlign: "-2px", marginRight: 5 }}
                        >
                          <path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
                          <line x1="12" y1="9" x2="12" y2="13" />
                          <line x1="12" y1="17" x2="12.01" y2="17" />
                        </svg>
                        DNS 泄露
                      </>
                    )
                  : dnsLeak?.leaking === false
                    ? "DNS 正常"
                    : "DNS 未知"}
            </button>
          </div>
        </div>
        <nav>
          <button
            className="tab"
            onClick={cycleTheme}
            title={
              themePref === "auto"
                ? "主题：跟随系统（点击切换浅色）"
                : themePref === "light"
                  ? "主题：浅色（点击切换深色）"
                  : "主题：深色（点击跟随系统）"
            }
            aria-label="切换主题：跟随系统 / 浅色 / 深色"
          >
            {themePref === "auto" ? (
              <svg
                width="15"
                height="15"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                style={{ verticalAlign: "-2px" }}
              >
                <rect x="2" y="3" width="20" height="14" rx="2" />
                <line x1="8" y1="21" x2="16" y2="21" />
                <line x1="12" y1="17" x2="12" y2="21" />
              </svg>
            ) : themePref === "light" ? (
              <svg
                width="15"
                height="15"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                style={{ verticalAlign: "-2px" }}
              >
                <circle cx="12" cy="12" r="4" />
                <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41" />
              </svg>
            ) : (
              <svg
                width="15"
                height="15"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                style={{ verticalAlign: "-2px" }}
              >
                <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
              </svg>
            )}
          </button>
          <button
            className={tab === "profiles" ? "tab active" : "tab"}
            onClick={() => setTab("profiles")}
          >
            Profile 切换
          </button>
          <button
            className={tab === "sessions" ? "tab active" : "tab"}
            onClick={() => setTab("sessions")}
          >
            会话管理
          </button>
          <button
            className={tab === "workflow" ? "tab active" : "tab"}
            onClick={() => setTab("workflow")}
          >
            高效工作流
          </button>
          <button
            className={tab === "pets" ? "tab active" : "tab"}
            onClick={() => setTab("pets")}
          >
            Codex宠物
          </button>
          <button
            className={tab === "settings" ? "tab active" : "tab"}
            onClick={() => setTab("settings")}
          >
            安全守护
          </button>
        </nav>
      </header>

      {notice && (
        <div className="notice-banner">
          {notice}
          <button onClick={() => setNotice(null)}>×</button>
        </div>
      )}
      {exitBlocked && (
        <div className="exit-blocked-overlay">
          <div className="exit-blocked-box">
            <h3>Codex 正在使用本地路由</h3>
            <p>
              直接退出会断开当前 Codex 会话（请求会打到已关闭的本地代理而失败）。
              请先完全退出 Codex / ChatGPT 桌面端与命令行，再退出 CodexFF Pro。
            </p>
            <div className="exit-blocked-actions">
              <button onClick={() => setExitBlocked(false)}>知道了</button>
              <button
                className="danger"
                onClick={() => {
                  setExitBlocked(false);
                  void forceQuitApp();
                }}
              >
                仍然退出
              </button>
            </div>
          </div>
        </div>
      )}
      {error && (
        <div className="error-banner">
          {error}
          <button onClick={() => setError(null)}>×</button>
        </div>
      )}

      <main className="app-main">
        {tab === "profiles" && (
          <ProfilesPage
            status={status}
            onSwitch={switchTo}
            onChanged={refresh}
            switching={switching}
            switchingLabel={switchStep ?? undefined}
          />
        )}
        {tab === "sessions" && <SessionsPage onToast={setPageToast} />}
        {tab === "workflow" && <WorkflowPage onToast={setPageToast} />}
        {tab === "pets" && <PetsPage onToast={setPageToast} />}
        {tab === "settings" && (
          <SettingsPage
            status={status}
            onChanged={refresh}
            dnsLeak={dnsLeak}
            dnsChecking={dnsChecking}
            onRunDnsCheck={runDnsCheck}
          />
        )}
      </main>
      <footer className="app-footer">
        <span>CodexFF v{status?.version ?? ""} · 基础功能永久免费</span>
        <button
          type="button"
          className="link-btn"
          onClick={() => void openUrl("https://code.etony.ccwu.cc/")}
        >
          点击体验 CodexFF Pro
        </button>
      </footer>
      {firstRunOpen && (
        <FirstRunGuide onDone={finishFirstRun} dark={appliedDark} />
      )}
      {(confirmState || pageToast) && (
        <div className="toast-stack">
          {confirmState && (
            <FloatingToast
              kind="confirm"
              title={confirmState.title}
              message={confirmState.message}
              confirmLabel={confirmState.confirmLabel ?? "仍然切换"}
              cancelLabel={confirmState.cancelLabel ?? "取消"}
              extraActions={confirmState.extraActions}
              onConfirm={confirmState.onConfirm}
              onClose={() => setConfirmState(null)}
            />
          )}
          {pageToast && (
            <FloatingToast
              kind={pageToast.kind ?? "warn"}
              title={pageToast.title}
              message={pageToast.message}
              confirmLabel={pageToast.confirmLabel}
              cancelLabel={pageToast.cancelLabel}
              onConfirm={
                pageToast.onConfirm
                  ? () => {
                      pageToast.onConfirm?.();
                      setPageToast(null);
                    }
                  : undefined
              }
              onClose={() => setPageToast(null)}
            />
          )}
        </div>
      )}
    </div>
  );
}

export default App;
