import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  SessionMeta,
  UnifySessionMeta,
  errMsg,
  hasUnifyBackup,
  isCodexRunning,
  listUnifiableSessions,
  listSessions,
  migrateSessionsToShared,
  restoreUnifiedSessions,
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
  // 统一会话历史迁移 (旧官方 "openai" 桶 → 共享 "custom" 桶, 迁移前自动备份)
  const [unifyList, setUnifyList] = useState<UnifySessionMeta[] | null>(null);
  const [unifyBackup, setUnifyBackup] = useState(false);
  const [unifyOpen, setUnifyOpen] = useState(false);
  const [unifySelected, setUnifySelected] = useState<Set<string>>(new Set());
  const [unifyBusy, setUnifyBusy] = useState(false);
  const [unifyStep, setUnifyStep] = useState("");
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
    void refreshUnify();
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

  async function refreshUnify() {
    try {
      const [list, backup] = await Promise.all([listUnifiableSessions(), hasUnifyBackup()]);
      setUnifyList(list);
      setUnifyBackup(backup);
    } catch {
      // 扫描失败不阻塞会话列表
    }
  }

  function requestOpenUnify() {
    if (unifyBusy || !unifyList || unifyList.length === 0) return;
    onToast?.({
      kind: "confirm",
      title: "迁入共享历史？",
      message: `将把所选旧官方会话迁入共享历史（当前共 ${unifyList.length} 个线程）。迁移前会自动备份，之后可随时还原；跨供应商继续旧会话时，对方后端可能无法解密部分推理内容。`,
      confirmLabel: "选择会话",
      cancelLabel: "取消",
      onConfirm: () => {
        setUnifySelected(new Set(unifyList.map((u) => u.thread_id)));
        setUnifyOpen(true);
      },
    });
  }

  async function startMigrate() {
    if (unifyBusy || unifySelected.size === 0) return;
    setUnifyBusy(true);
    setUnifyStep("准备迁移…");
    try {
      await migrateSessionsToShared([...unifySelected]);
      setUnifyOpen(false);
      await Promise.all([load(), refreshUnify()]);
      onToast?.({
        title: "迁移完成",
        message: "所选旧官方会话已迁入共享历史。",
      });
    } catch (e) {
      const msg = errMsg(e);
      onToast?.({
        title: msg.includes("Codex") ? "无法迁移" : "迁移失败",
        message: msg,
      });
    } finally {
      setUnifyBusy(false);
      setUnifyStep("");
    }
  }

  function requestRestore() {
    if (unifyBusy) return;
    onToast?.({
      kind: "confirm",
      title: "从备份还原旧官方会话？",
      message:
        "将按迁移时的备份账本，把已迁入共享历史的旧官方会话还原为独立历史；开启统一后新建的会话不受影响。还原前也会自动备份当前状态。",
      confirmLabel: "确认还原",
      cancelLabel: "取消",
      onConfirm: () => {
        void restoreUnified();
      },
    });
  }

  async function restoreUnified() {
    setUnifyBusy(true);
    setUnifyStep("准备还原…");
    try {
      await restoreUnifiedSessions();
      await Promise.all([load(), refreshUnify()]);
      onToast?.({
        title: "还原完成",
        message: "旧官方会话已还原为独立历史。",
      });
    } catch (e) {
      onToast?.({
        title: "还原失败",
        message: errMsg(e),
      });
    } finally {
      setUnifyBusy(false);
      setUnifyStep("");
    }
  }

  function fmtSize(bytes: number) {
    if (bytes >= 1024 * 1024 * 1024) return (bytes / (1024 * 1024 * 1024)).toFixed(1) + " GB";
    if (bytes >= 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + " MB";
    if (bytes >= 1024) return (bytes / 1024).toFixed(1) + " KB";
    return bytes + " B";
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
              官方与第三方共享同一会话历史列表。旧官方会话（独立历史）可迁入共享列表，
              迁移前自动备份，随时可还原。
            </p>
          </div>
          <div className="unify-actions">
            {unifyList && unifyList.length > 0 && (
              <button onClick={requestOpenUnify} disabled={unifyBusy}>
                迁入旧官方会话 ({unifyList.length})
              </button>
            )}
            {unifyBackup && (
              <button className="link-btn" onClick={requestRestore} disabled={unifyBusy}>
                从备份还原
              </button>
            )}
          </div>
        </div>
        {unifyStep && <p className="hint">迁移进度：{unifyStep}</p>}
        {unifyOpen && unifyList && unifyList.length > 0 && (
          <div className="unify-select">
            <div className="unify-select-head">
              <label className="checkbox-label">
                <input
                  type="checkbox"
                  checked={unifySelected.size === unifyList.length}
                  onChange={(e) =>
                    setUnifySelected(
                      e.target.checked
                        ? new Set(unifyList.map((u) => u.thread_id))
                        : new Set(),
                    )
                  }
                />
                全选
              </label>
              <span className="hint">选择要迁入共享历史的旧官方会话</span>
            </div>
            <div className="unify-rows">
              {unifyList.map((u) => (
                <label key={u.thread_id} className="row-card unify-row">
                  <input
                    type="checkbox"
                    checked={unifySelected.has(u.thread_id)}
                    onChange={(e) => {
                      const next = new Set(unifySelected);
                      if (e.target.checked) next.add(u.thread_id);
                      else next.delete(u.thread_id);
                      setUnifySelected(next);
                    }}
                  />
                  <div className="row-card-main">
                    <strong>{truncate(u.title, 60)}</strong>
                    <span className="mono dim">
                      {u.id} · {fmtSize(u.size)}
                      {u.rollups > 1 ? ` · ${u.rollups} 条` : ""}
                      {u.archived ? " · 归档" : ""}
                    </span>
                  </div>
                </label>
              ))}
            </div>
            <div className="unify-actions">
              <button
                className="primary"
                disabled={unifySelected.size === 0 || unifyBusy}
                onClick={() => void startMigrate()}
              >
                迁移所选（{unifySelected.size}）
              </button>
              <button disabled={unifyBusy} onClick={() => setUnifyOpen(false)}>
                取消
              </button>
            </div>
          </div>
        )}
      </section>

      <div className="sessions-layout">
      <section className="card session-list">
        <h2>会话 ({sessions.length})</h2>
        <p className="hint">
          与官方一致按项目目录/名称分组，默认折叠可展开；勾选 = 该项目下所有
          线程在官方订阅下不可见，切换官方时自动迁移，切回第三方后自动恢复。
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
            点左侧会话查看内容。会话跨 provider 共享；标记隔离的会话官方订阅不可见。
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
