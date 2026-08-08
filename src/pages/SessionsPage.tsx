import { useEffect, useRef, useState } from "react";
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

export function SessionsPage({ onToast }: Props) {
  const [sessions, setSessions] = useState<SessionMeta[]>([]);
  const [loading, setLoading] = useState(true);
  const [err, setErr] = useState<string | null>(null);
  const [selected, setSelected] = useState<SessionMeta | null>(null);
  const [detail, setDetail] = useState<unknown[]>([]);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailErr, setDetailErr] = useState<string | null>(null);
  const [search, setSearch] = useState("");
  // 隔离进度 (Codex 必须完全退出; 过程显示进度)
  const [isolatingId, setIsolatingId] = useState<string | null>(null);
  const [isolateStep, setIsolateStep] = useState("");
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
      setErr(null);
    } catch (e) {
      setErr(errMsg(e));
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
    setDetailErr(null);
    try {
      const d = await sessionDetail(s.path, 300);
      if (req === detailReq.current) setDetail(d);
    } catch (e) {
      if (req === detailReq.current) setDetailErr(errMsg(e));
    } finally {
      if (req === detailReq.current) setDetailLoading(false);
    }
  }

  // 勾选隔离: 先弹确认悬浮提示说明隔离作用, 确认后才开始隔离;
  // 取消勾选不弹确认, 直接执行。
  function requestToggleIsolation(s: SessionMeta, checked: boolean) {
    if (isolatingId) return;
    if (!checked) {
      void doToggleIsolation(s, false);
      return;
    }
    onToast?.({
      kind: "confirm",
      title: "确认隔离会话？",
      message: "勾选后官方订阅将看不到该线程的全部会话（含侧边栏的项目/目录与标题）；切回第三方后自动恢复。",
      confirmLabel: "确认隔离",
      cancelLabel: "取消",
      onConfirm: () => {
        void doToggleIsolation(s, true);
      },
    });
  }

  // 隔离: 官方订阅激活时该线程全部文件移入金库, 官方 CLI 不可见;
  // 隔离前要求 Codex (桌面/CLI) 完全退出, 防止移动正在写入的会话文件。
  async function doToggleIsolation(s: SessionMeta, isolated: boolean) {
    if (isolatingId) return;
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
    setIsolatingId(s.thread_id);
    setIsolateStep("准备隔离…");
    try {
      await setSessionIsolated(s.thread_id, isolated);
    } catch (e) {
      const msg = errMsg(e);
      if (msg.includes("Codex")) {
        onToast?.({
          title: "无法隔离",
          message: msg,
        });
      } else {
        setErr(msg);
      }
    } finally {
      setIsolatingId(null);
      setIsolateStep("");
      await load();
    }
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
    (s.title + " " + s.id + " " + s.thread_id + " " + s.model + " " + s.preview)
      .toLowerCase()
      .includes(search.toLowerCase()),
  );

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
          勾选 = 该线程的全部会话与侧边栏项目/目录在官方订阅下不可见；
          切换官方时自动迁移，切回第三方后自动恢复。同线程已合并为一条，点击查看最新一条。
        </p>
        <input
          className="search"
          placeholder="搜索标题 / 内容 / ID / 模型…"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
        {err && <p className="error">{err}</p>}
        {loading && <p className="hint">扫描中…</p>}
        {isolateStep && <p className="hint">隔离进度：{isolateStep}</p>}
        {!loading && filtered.length === 0 && (
          <p className="hint">没有会话 (在 codex 里用过之后才会生成)。</p>
        )}
        {filtered.map((s) => (
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
            <div className="row-card-actions">
              <label
                className={`isolate-check${s.isolated ? " checked" : ""}`}
                onClick={(e) => {
                  e.stopPropagation();
                }}
                title={
                  s.isolated
                    ? "取消隔离：官方订阅将可看到此线程的全部会话"
                    : "隔离：官方订阅不可见此线程的全部会话"
                }
              >
                <input
                  type="checkbox"
                  checked={s.isolated}
                  disabled={isolatingId === s.thread_id}
                  onChange={(e) => {
                    e.stopPropagation();
                    requestToggleIsolation(s, e.target.checked);
                  }}
                />
                <span>
                  {isolatingId === s.thread_id
                    ? "隔离中…"
                    : s.isolated
                      ? "已隔离"
                      : "隔离"}
                </span>
              </label>
            </div>
          </div>
        ))}
      </section>

      <section className="card session-detail">
        <h2>{selected ? truncate(selected.title, 80) : "会话内容"}</h2>
        {!selected && (
          <p className="hint">
            点左侧会话查看内容。会话跨 provider 共享；标记隔离的会话官方订阅不可见。
          </p>
        )}
        {selected && detailErr && <p className="error">{detailErr}</p>}
        {selected && !detailErr && detailLoading && (
          <p className="hint">加载中…</p>
        )}
        {selected && !detailErr && !detailLoading && detail.length === 0 && (
          <p className="hint">会话无可见内容。</p>
        )}
        <div className="session-lines">
          {detail.map((line, i) => {
            const item = line as { type?: string; payload?: Record<string, unknown> };
            const text =
              (item.payload?.text as string) ??
              (item.payload?.content as unknown) ??
              "";
            return (
              <div key={i} className="session-line">
                <span className="mono dim">{item.type ?? "?"}</span>
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
