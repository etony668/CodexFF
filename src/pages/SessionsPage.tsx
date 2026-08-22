import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  SessionMeta,
  errMsg,
  getSessionUnifyState,
  isCodexRunning,
  listSessions,
  setSessionUnifyEnabled,
  sessionDetail,
  setSessionIsolated,
} from "../api";
import type { ToastRequest } from "../FloatingToast";

interface Props {
  /** 请求 App 层统一弹出悬浮提示 (避免独立容器与其它提示重叠) */
  onToast?: (t: ToastRequest) => void;
}

interface SessionGroup {
  key: string;
  name: string;
  dir: string;
  sessions: SessionMeta[];
}

export function SessionsPage({ onToast }: Props) {
  const [sessions, setSessions] = useState<SessionMeta[]>([]);
  const [loading, setLoading] = useState(true);
  const [selected, setSelected] = useState<SessionMeta | null>(null);
  const [detail, setDetail] = useState<unknown[]>([]);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailUnavailable, setDetailUnavailable] = useState(false);
  const [search, setSearch] = useState("");
  // 隔离进度 (Codex 必须完全退出; 过程显示进度)
  const [isolatingGroup, setIsolatingGroup] = useState<string | null>(null);
  const [isolateStep, setIsolateStep] = useState("");
  // null 表示首次加载：所有项目默认折叠。
  const [collapsed, setCollapsed] = useState<Set<string> | null>(null);
  // 统一会话历史：由用户显式开关控制，开启前备份，期间增量备份，关闭按账本恢复
  const [unifyBusy, setUnifyBusy] = useState(false);
  const [unifyStep, setUnifyStep] = useState("");
  const [unifyState, setUnifyState] = useState<{
    enabled: boolean;
    generation: string | null;
    last_checkpoint_ms: number;
    backed_up_threads: number;
    error: string | null;
  }>({
    enabled: false,
    generation: null,
    last_checkpoint_ms: 0,
    backed_up_threads: 0,
    error: null,
  });
  // 请求序号: 快速点 A→B 时, A 的响应晚到不能覆盖 B 的内容
  const detailReq = useRef(0);

  async function load() {
    setLoading(true);
    try {
      setSessions(await listSessions());
    } catch (e) {
      onToast?.({
        title: "加载会话失败",
        message: errMsg(e),
        kind: "warn",
      });
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    load();
    void refreshUnifyState();
  }, []);

  // 统一历史迁移进度事件
  useEffect(() => {
    const unlisten = listen<string>("session-unify-progress", (e) => {
      setUnifyStep(e.payload);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // 后端隔离进度事件
  useEffect(() => {
    const unlisten = listen<string>("session-isolate-progress", (e) => {
      setIsolateStep(e.payload);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  async function open(s: SessionMeta) {
    const req = ++detailReq.current;
    setSelected(s);
    setDetail([]);
    setDetailLoading(true);
    setDetailUnavailable(false);
    try {
      const d = await sessionDetail(s.path, 300);
      if (req === detailReq.current) setDetail(d);
    } catch (e) {
      if (req === detailReq.current) {
        setDetailUnavailable(true);
        onToast?.({
          title: "加载会话内容失败",
          message: errMsg(e),
          kind: "warn",
        });
      }
    } finally {
      if (req === detailReq.current) setDetailLoading(false);
    }
  }

  // 项目级隔离: 勾选后该项目下所有线程在官方订阅下不可见。
  // 确认流程与单会话一致, 取消勾选不弹确认, 直接执行。
  function requestToggleGroupIsolation(
    g: SessionGroup,
    checked: boolean,
  ) {
    if (isolatingGroup) return;
    if (!checked) {
      void doToggleGroupIsolation(g, false);
      return;
    }
    onToast?.({
      kind: "confirm",
      title: "确认隔离该项目会话？",
      message: `勾选后官方订阅将看不到该项目下 ${g.sessions.length} 个线程的全部会话（含侧边栏的项目/目录与标题）；切回第三方后自动恢复。`,
      confirmLabel: "确认隔离",
      cancelLabel: "取消",
      onConfirm: () => {
        void doToggleGroupIsolation(g, true);
      },
    });
  }

  // 隔离: 官方订阅激活时线程文件移入金库, 官方 CLI 不可见;
  // 隔离前要求 Codex (桌面/CLI) 完全退出, 防止移动正在写入的会话文件。
  async function doToggleGroupIsolation(g: SessionGroup, isolated: boolean) {
    if (isolatingGroup) return;
    if (isolated) {
      try {
        if (await isCodexRunning()) {
          onToast?.({
            title: "无法隔离",
            message: "Codex / ChatGPT 桌面端正在运行。请先完全退出后再隔离会话。",
          });
          return;
        }
      } catch {
        // 预检失败不阻塞, 后端命令还有一层强制检测
      }
    }
    setIsolatingGroup(g.key);
    try {
      const pending = g.sessions.filter((s) => s.isolated !== isolated);
      const failures: string[] = [];
      for (const s of pending) {
        setIsolateStep(
          `${isolated ? "正在隔离" : "正在恢复"} ${truncate(s.title, 40)}…`,
        );
        try {
          await setSessionIsolated(s.thread_id, isolated);
        } catch (e) {
          failures.push(`${truncate(s.title, 32)}：${errMsg(e)}`);
        }
      }
      if (failures.length > 0) {
        onToast?.({
          title: "部分会话处理失败",
          message: `${pending.length - failures.length}/${pending.length} 个会话处理完成；${failures.length} 个失败。${failures[0]}`,
          kind: "warn",
        });
      }
    } catch (e) {
      const msg = errMsg(e);
      if (msg.includes("Codex")) {
        onToast?.({
          title: "无法隔离",
          message: msg,
        });
      } else {
        onToast?.({ title: "会话隔离失败", message: msg, kind: "warn" });
      }
    } finally {
      setIsolatingGroup(null);
      setIsolateStep("");
      await load();
    }
  }

  function toggleGroup(key: string) {
    setCollapsed((prev) => {
      // 首次交互前全部折叠；点击某一组时只展开该组。
      const next = prev === null ? new Set(groups.map((g) => g.key)) : new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  async function refreshUnifyState() {
    try {
      setUnifyState(await getSessionUnifyState());
    } catch {
      // 旧版本后端没有该命令时保持关闭，避免静默执行统一。
    }
  }

  function requestToggleUnify(enabled: boolean) {
    if (unifyBusy || enabled === unifyState.enabled) return;
    onToast?.({
      kind: "confirm",
      title: enabled ? "开启会话统一？" : "关闭会话统一并恢复归属？",
      message: enabled
        ? "开启前会完整备份当前会话、项目索引和状态库，并记录每个会话的渠道/账号归属。开启期间会持续增量备份。请先完全退出 Codex / ChatGPT 桌面端与命令行；任何失败都会保留原状态，不会删除会话。"
        : "关闭前会先备份最新会话内容，再按归属账本恢复渠道/账号标识。统一期间新建会话不会被错误改回旧渠道；恢复失败会保留统一状态并提示，不会静默覆盖。",
      confirmLabel: enabled ? "确认开启" : "确认关闭并恢复",
      cancelLabel: "取消",
      onConfirm: () => {
        void toggleUnify(enabled);
      },
    });
  }

  async function toggleUnify(enabled: boolean) {
    setUnifyBusy(true);
    setUnifyStep(enabled ? "准备安全备份…" : "准备恢复归属…");
    try {
      const next = await setSessionUnifyEnabled(enabled);
      setUnifyState(next);
      await load();
      onToast?.({
        title: enabled ? "会话统一已开启" : "会话统一已关闭",
        message: enabled
          ? `已备份 ${next.backed_up_threads} 个线程，后续会持续增量备份。`
          : "已按最新归属账本恢复；统一期间新增内容已保留。",
      });
    } catch (e) {
      onToast?.({
        title: enabled ? "开启会话统一失败" : "关闭会话统一失败",
        message: errMsg(e),
        kind: "warn",
      });
      await refreshUnifyState();
    } finally {
      setUnifyBusy(false);
      setUnifyStep("");
    }
  }

  function fmt(ms: number) {
    return new Date(ms).toLocaleString();
  }

  function truncate(s: string, n: number) {
    return s.length > n ? s.slice(0, n) + "…" : s;
  }

  const filtered = sessions.filter((s) =>
    (s.title +
      " " +
      s.id +
      " " +
      s.thread_id +
      " " +
      s.model +
      " " +
      s.preview +
      " " +
      s.cwd +
      " " +
      s.project)
      .toLowerCase()
      .includes(search.toLowerCase()),
  );

  // 与官方侧边栏一致: 按项目分组 (注册项目名优先, 否则用目录名; 无目录归入“未分类”)。
  const groups: SessionGroup[] = useMemo(() => {
    const map = new Map<string, SessionGroup>();
    for (const s of filtered) {
      const dir = s.cwd || "";
      const key = s.project || dir || "__none__";
      const name =
        s.project || dir.split("/").filter(Boolean).pop() || "未分类";
      let g = map.get(key);
      if (!g) {
        g = { key, name, dir, sessions: [] };
        map.set(key, g);
      }
      g.sessions.push(s);
    }
    return [...map.values()].sort((a, b) =>
      a.name.localeCompare(b.name, "zh-Hans-CN"),
    );
  }, [filtered]);

  return (
    <div className="page">
      <section className="card unify-card">
        <div className="unify-head">
          <div className="unify-copy">
            <h2>统一会话历史</h2>
            <p className="hint">
              默认关闭。开启后官方与第三方共享会话列表；开启前完整备份，期间持续增量备份，
              关闭时按最新归属账本恢复。
            </p>
          </div>
          <div className="unify-actions">
            <button
              type="button"
              role="switch"
              aria-checked={unifyState.enabled}
              className={`switch${unifyState.enabled ? " on" : ""}`}
              onClick={() => requestToggleUnify(!unifyState.enabled)}
              disabled={unifyBusy}
              title={unifyState.enabled ? "关闭统一会话历史" : "开启统一会话历史"}
            >
              <span className="switch-knob" />
            </button>
            <span className={unifyState.enabled ? "ok" : "dim"}>
              {unifyBusy
                ? unifyState.enabled
                  ? "关闭中…"
                  : "开启中…"
                : unifyState.enabled
                  ? "已开启"
                  : "已关闭"}
            </span>
          </div>
        </div>
        {unifyState.enabled && (
          <p className="hint">
            已备份 {unifyState.backed_up_threads} 个线程；会话统一期间会持续保护最新会话内容。
          </p>
        )}
        {unifyStep && <p className="hint">迁移进度：{unifyStep}</p>}
      </section>

      <div className="sessions-layout">
      <section className="card session-list">
        <h2>会话 ({sessions.length})</h2>
        <p className="hint">
          与官方一致按项目目录/名称分组，默认折叠可展开；勾选 = 该项目下所有
          线程在官方订阅下不可见；会话统一仅在上方开关开启后生效。
          同线程已合并为一条，点击查看最新一条。
        </p>
        <input
          className="search"
          placeholder="搜索标题 / 内容 / ID / 模型 / 项目…"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
        {loading && <p className="hint">扫描中…</p>}
        {isolateStep && <p className="hint">隔离进度：{isolateStep}</p>}
        {!loading && filtered.length === 0 && (
          <p className="hint">没有会话 (在 codex 里用过之后才会生成)。</p>
        )}
        {groups.map((g) => {
          const allIsolated = g.sessions.every((s) => s.isolated);
          const anyIsolated = g.sessions.some((s) => s.isolated);
          const isCollapsed = collapsed === null || collapsed.has(g.key);
          return (
            <div key={g.key} className="session-group">
              <div className="session-group-head">
                <button
                  type="button"
                  className={`group-toggle${isCollapsed ? "" : " open"}`}
                  onClick={() => toggleGroup(g.key)}
                  aria-label={isCollapsed ? "展开项目" : "收起项目"}
                >
                  <span className="group-plus">
                    {isCollapsed ? (
                      <svg
                        width="12"
                        height="12"
                        viewBox="0 0 12 12"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth="1.6"
                        strokeLinecap="round"
                      >
                        <path d="M6 1v10M1 6h10" />
                      </svg>
                    ) : (
                      <svg
                        width="12"
                        height="12"
                        viewBox="0 0 12 12"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth="1.6"
                        strokeLinecap="round"
                      >
                        <path d="M1 6h10" />
                      </svg>
                    )}
                  </span>
                </button>
                <div className="group-title" onClick={() => toggleGroup(g.key)}>
                  <strong>{g.name}</strong>
                  {g.dir && <span className="mono dim">{g.dir}</span>}
                </div>
                <label
                  className={`isolate-check${allIsolated ? " checked" : ""}`}
                  onClick={(e) => e.stopPropagation()}
                  title={
                    allIsolated
                      ? "取消隔离：官方订阅将可看到该项目下的全部会话"
                      : "隔离：官方订阅不可见该项目下的全部会话"
                  }
                >
                  <input
                    type="checkbox"
                    checked={allIsolated}
                    disabled={isolatingGroup === g.key}
                    ref={(el) => {
                      if (el) el.indeterminate = anyIsolated && !allIsolated;
                    }}
                    onChange={(e) => {
                      e.stopPropagation();
                      requestToggleGroupIsolation(g, e.target.checked);
                    }}
                  />
                  <span>
                    {isolatingGroup === g.key
                      ? "隔离中…"
                      : allIsolated
                        ? "已隔离"
                        : anyIsolated
                          ? "部分隔离"
                          : "隔离"}
                  </span>
                </label>
              </div>
              {!isCollapsed && (
                <div className="session-group-body">
                  {g.sessions.map((s) => (
                    <div
                      key={s.id}
                      className={`row-card clickable ${selected?.id === s.id ? "selected" : ""}`}
                      onClick={() => open(s)}
                    >
                      <div className="row-card-main">
                        <strong>{truncate(s.title, 60)}</strong>
                        <span className="mono dim">
                          {s.id} · {s.model || "—"} · {fmt(s.last_active_ms)}
                          {s.rollups > 1 ? ` · ${s.rollups} 条` : ""}
                          {s.archived ? " · 归档" : ""}
                          {s.isolated ? " · 已隔离 (官方不可见)" : ""}
                        </span>
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </div>
          );
        })}
      </section>

      <section className="card session-detail">
        <h2>{selected ? truncate(selected.title, 80) : "会话内容"}</h2>
        {!selected && (
        <p className="hint">
          点左侧会话查看内容。会话是否跨官方/第三方共享由上方“统一会话历史”开关决定；
          标记隔离的会话官方订阅不可见。
        </p>
        )}
        {selected && detailLoading && (
          <p className="hint">加载中…</p>
        )}
        {selected && detailUnavailable && !detailLoading && (
          <p className="hint">内容暂未加载，可重新点击该会话重试。</p>
        )}
        {selected && !detailUnavailable && !detailLoading && detail.length === 0 && (
          <p className="hint">会话无可见内容。</p>
        )}
        <div className="session-lines">
          {detail.map((line, i) => {
            const item = line as {
              type?: string;
              role?: string;
              text?: string;
              payload?: Record<string, unknown>;
            };
            const text = item.text ?? (item.payload?.text as string) ?? "";
            return (
              <div key={i} className="session-line">
                <span className="mono dim">
                  {item.role === "user"
                    ? "用户"
                    : item.role === "assistant"
                      ? "模型"
                      : item.type ?? "?"}
                </span>
                <pre>{typeof text === "string" ? truncate(text, 2000) : JSON.stringify(text).slice(0, 2000)}</pre>
              </div>
            );
          })}
        </div>
      </section>
      </div>
    </div>
  );
}
