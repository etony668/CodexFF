import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  WorkflowAgentInfo,
  WorkflowAgentsResult,
  errMsg,
  installWorkflowPreset,
  listWorkflowAgents,
  listWorkflowModelSources,
  listWorkflowModels,
  resetWorkflowPresets,
  restoreWorkflowPreset,
  uninstallWorkflowPreset,
  updateWorkflowPreset,
} from "../api";
import type { WorkflowModelSource } from "../api";
import type { ToastRequest } from "../FloatingToast";

interface Props {
  onToast?: (t: ToastRequest) => void;
}

const PRESET_META: Record<string, { label: string; role: string; model: string }> = {
  "luna-worker": {
    label: "Luna 执行",
    role: "执行 / 编写 / 普通审查",
    model: "GPT-5.6 Luna · Max",
  },
  "sol-planner": {
    label: "Sol 规划",
    role: "复杂任务规划",
    model: "GPT-5.6 Sol · xHigh",
  },
  "sol-reviewer": {
    label: "Sol 审查",
    role: "复杂审查",
    model: "GPT-5.6 Sol · xHigh",
  },
};

const EFFORT_OPTIONS = ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

const DEFAULT_CONFIG: Record<string, { model: string; effort: string }> = {
  "luna-worker": { model: "gpt-5.6-luna", effort: "max" },
  "sol-planner": { model: "gpt-5.6-sol", effort: "xhigh" },
  "sol-reviewer": { model: "gpt-5.6-sol", effort: "xhigh" },
};

/** 模型目录缺失/为空时的常用模型兜底 */
const FALLBACK_MODELS = [
  "gpt-5.6-luna",
  "gpt-5.6-sol",
  "gpt-5.6-terra",
  "gpt-5.2-codex-mini",
  "gpt-5.2-codex",
  "deepseek-v4-flash",
  "deepseek-v4-pro",
];

const EFFORT_LABELS: Record<string, string> = {
  minimal: "Minimal",
  low: "Low",
  medium: "Medium",
  high: "High",
  xhigh: "xHigh",
  max: "Max",
  ultra: "Ultra",
};

export function WorkflowPage({ onToast }: Props) {
  const [result, setResult] = useState<WorkflowAgentsResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [editModel, setEditModel] = useState("");
  const [editEffort, setEditEffort] = useState("high");
  const [modelOptions, setModelOptions] = useState<string[]>([]);
  const [modelSources, setModelSources] = useState<WorkflowModelSource[]>([]);
  /** 编辑时模型来源: "current" = 当前供应商, 否则为供应商/官方 id */
  const [editSource, setEditSource] = useState("current");

  async function refresh() {
    try {
      setResult(await listWorkflowAgents());
    } catch (e) {
      onToast?.({ title: "加载失败", message: errMsg(e) });
    }
  }

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 加载当前供应商模型目录作为下拉选项
  useEffect(() => {
    listWorkflowModels()
      .then((m) => setModelOptions(m.filter(Boolean)))
      .catch(() => {});
    listWorkflowModelSources()
      .then(setModelSources)
      .catch(() => {});
  }, []);

  // 切换供应商后重新读取当前供应商的模型 (下拉不再残留上一个供应商)
  useEffect(() => {
    const unlisten = listen("provider-changed", () => {
      void refresh();
      listWorkflowModels()
        .then((m) => setModelOptions(m.filter(Boolean)))
        .catch(() => {});
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  async function install(kind: string) {
    if (busy) return;
    setBusy(true);
    try {
      const info = await installWorkflowPreset(kind);
      await refresh();
      onToast?.({
        title: "已启用",
        message: `「${PRESET_META[kind]?.label ?? info.name}」已写入 ${info.path}。重新打开 Codex 或新开会话后即可在 Agent 中调用。`,
      });
    } catch (e) {
      onToast?.({ title: "启用失败", message: errMsg(e) });
    } finally {
      setBusy(false);
    }
  }

  function requestRemove(kind: string) {
    if (busy) return;
    onToast?.({
      kind: "confirm",
      title: "移除预设？",
      message: `「${PRESET_META[kind]?.label ?? kind}」将从 ~/.codex/agents 移入备份（可一键恢复），Codex 中将不再看到该 Agent。`,
      confirmLabel: "移除",
      cancelLabel: "取消",
      onConfirm: () => {
        void remove(kind);
      },
    });
  }

  async function remove(kind: string) {
    setBusy(true);
    try {
      const outcome = await uninstallWorkflowPreset(kind);
      await refresh();
      onToast?.({
        title: outcome.removed ? "已移除" : "未找到文件",
        message: outcome.removed
          ? "已移入备份目录，需要时可在本页点击“恢复”。"
          : "Agent 文件本就不存在，无需移除。",
      });
    } catch (e) {
      onToast?.({ title: "移除失败", message: errMsg(e) });
    } finally {
      setBusy(false);
    }
  }

  async function restore(kind: string) {
    if (busy) return;
    setBusy(true);
    try {
      const info = await restoreWorkflowPreset(kind);
      await refresh();
      onToast?.({
        title: "已恢复",
        message: `「${PRESET_META[kind]?.label ?? info.name}」已从备份还原到 ${info.path}。`,
      });
    } catch (e) {
      onToast?.({ title: "恢复失败", message: errMsg(e) });
    } finally {
      setBusy(false);
    }
  }

  function startEdit(a: WorkflowAgentInfo) {
    setEditing(a.id);
    setEditModel(a.model ?? DEFAULT_CONFIG[a.id]?.model ?? "");
    setEditEffort(a.reasoning_effort ?? DEFAULT_CONFIG[a.id]?.effort ?? "high");
    setEditSource("current");
  }

  async function saveEdit(kind: string) {
    if (busy) return;
    setBusy(true);
    try {
      const info = await updateWorkflowPreset(kind, editModel, editEffort);
      await refresh();
      setEditing(null);
      onToast?.({
        title: "已保存",
        message: `「${PRESET_META[kind]?.label ?? info.name}」已更新为 ${info.model} · ${EFFORT_LABELS[info.reasoning_effort ?? ""] ?? info.reasoning_effort}。重新打开 Codex 或新开会话后生效。`,
      });
    } catch (e) {
      onToast?.({ title: "保存失败", message: errMsg(e) });
    } finally {
      setBusy(false);
    }
  }

  function requestResetAll() {
    if (busy) return;
    onToast?.({
      kind: "confirm",
      title: "恢复默认设置？",
      message: "三个预设的模型与思考档位将恢复为默认值，当前自定义内容会先移入备份（可单独恢复）。",
      confirmLabel: "恢复默认",
      cancelLabel: "取消",
      onConfirm: () => {
        void resetAll();
      },
    });
  }

  async function resetAll() {
    setBusy(true);
    try {
      const infos = await resetWorkflowPresets();
      await refresh();
      onToast?.({
        title: "已恢复默认",
        message: `已重置 ${infos.length} 个预设为默认模型与档位。重新打开 Codex 或新开会话后生效。`,
      });
    } catch (e) {
      onToast?.({ title: "恢复失败", message: errMsg(e) });
    } finally {
      setBusy(false);
    }
  }

  async function installAll() {
    if (busy) return;
    setBusy(true);
    const kinds = Object.keys(PRESET_META);
    const done: string[] = [];
    const failed: string[] = [];
    for (const kind of kinds) {
      try {
        await installWorkflowPreset(kind);
        done.push(PRESET_META[kind].label);
      } catch (e) {
        failed.push(`${PRESET_META[kind].label}: ${errMsg(e)}`);
      }
    }
    await refresh();
    if (failed.length === 0) {
      onToast?.({
        title: "全部启用成功",
        message: `${done.join(" / ")} 已写入 ~/.codex/agents。重新打开 Codex 或新开会话后即可在 Agent 中调用。`,
      });
    } else {
      onToast?.({
        title: done.length > 0 ? "部分启用成功" : "启用失败",
        message: [done.length > 0 ? `已启用：${done.join(" / ")}` : "", ...failed].join("\n"),
      });
    }
    setBusy(false);
  }

  function requestRemoveAll() {
    if (busy) return;
    onToast?.({
      kind: "confirm",
      title: "移除全部预设？",
      message: "三个预设（Luna 执行 / Sol 规划 / Sol 审查）都将移入备份，可随时恢复。",
      confirmLabel: "全部移除",
      cancelLabel: "取消",
      onConfirm: () => {
        void removeAll();
      },
    });
  }

  async function removeAll() {
    setBusy(true);
    const removed: string[] = [];
    const failed: string[] = [];
    for (const kind of Object.keys(PRESET_META)) {
      try {
        const outcome = await uninstallWorkflowPreset(kind);
        if (outcome.removed) removed.push(PRESET_META[kind].label);
      } catch (e) {
        failed.push(`${PRESET_META[kind].label}: ${errMsg(e)}`);
      }
    }
    await refresh();
    if (failed.length === 0) {
      onToast?.({
        title: "已全部移除",
        message: removed.length > 0 ? `${removed.join(" / ")} 已移入备份。` : "未发现已安装的预设文件。",
      });
    } else {
      onToast?.({
        title: "部分移除失败",
        message: failed.join("\n"),
      });
    }
    setBusy(false);
  }

  const presets = result?.agents.filter((a) => a.preset) ?? [];
  const others = result?.agents.filter((a) => !a.preset && a.installed) ?? [];

  return (
    <div className="page">
      <section className="card wf-card">
        <div className="wf-head">
          <div className="wf-copy">
            <h2>高效工作流</h2>
            <p className="hint">
              Sol 规划 + Luna 执行：主任务用 Sol 拆解与派发，边界清晰的执行交给 Luna
              Max，复杂审查再用 Sol 把关。安装后无需写配置文件，直接可用。
            </p>
          </div>
          <div className="wf-actions">
            <button className="primary" onClick={() => void installAll()} disabled={busy}>
              一键启用全部
            </button>
            <button onClick={requestRemoveAll} disabled={busy}>
              移除全部
            </button>
            <button
              className="wf-icon-btn"
              onClick={requestResetAll}
              disabled={busy}
              title="恢复默认设置"
              aria-label="恢复默认设置"
            >
              <svg
                width="15"
                height="15"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M3 12a9 9 0 1 0 3-6.7L3 8" />
                <path d="M3 3v5h5" />
              </svg>
            </button>
            <button className="link-btn" onClick={() => void refresh()} disabled={busy}>
              刷新
            </button>
          </div>
        </div>
        {result && (
          <p className="hint wf-dir">
            Agent 目录：<span className="mono">{result.agents_dir}</span>
          </p>
        )}
        <div className="wf-rows">
          {presets.map((a) => {
            const meta = PRESET_META[a.id];
            const status = a.installed
              ? a.customized
                ? "custom"
                : "installed"
              : a.backup_exists
                ? "backup"
                : "missing";
            const modelText =
              a.model && a.reasoning_effort
                ? `${a.model} · ${EFFORT_LABELS[a.reasoning_effort] ?? a.reasoning_effort}`
                : meta?.model ?? "";
            const isEditing = editing === a.id;
            const sourceModels =
              editSource === "current"
                ? modelOptions
                : (modelSources.find((s) => s.id === editSource)?.models ?? []);
            const modelSelectOptions = Array.from(
              new Set(
                [
                  ...(sourceModels.length > 0 ? sourceModels : FALLBACK_MODELS),
                  editModel,
                  DEFAULT_CONFIG[a.id]?.model,
                ].filter((m): m is string => !!m),
              ),
            );
            return (
              <div key={a.id} className="wf-row">
                <div className="wf-row-main">
                  <div className="wf-row-title">
                    <strong>{meta?.label ?? a.name}</strong>
                    <span className={`wf-badge ${status}`}>
                      {a.installed
                        ? a.customized
                          ? "已自定义"
                          : "已启用"
                        : a.backup_exists
                          ? "可恢复"
                          : "未启用"}
                    </span>
                  </div>
                  <span className="hint">{meta?.role ?? a.description}</span>
                  {isEditing ? (
                    <div className="wf-edit-row">
                      <select
                        className="wf-source-input"
                        value={editSource}
                        onChange={(e) => setEditSource(e.target.value)}
                        disabled={busy}
                        title="模型来源：选择即将切换的目标供应商或官方订阅"
                      >
                        <option value="current">当前供应商</option>
                        {modelSources.map((s) => (
                          <option key={s.id} value={s.id}>
                            {s.name}
                          </option>
                        ))}
                      </select>
                      <select
                        className="wf-model-input"
                        value={editModel}
                        onChange={(e) => setEditModel(e.target.value)}
                        disabled={busy}
                      >
                        {modelSelectOptions.map((m) => (
                          <option key={m} value={m}>
                            {m}
                          </option>
                        ))}
                      </select>
                      <select
                        className="wf-effort-select"
                        value={editEffort}
                        onChange={(e) => setEditEffort(e.target.value)}
                        disabled={busy}
                      >
                        {EFFORT_OPTIONS.map((e) => (
                          <option key={e} value={e}>
                            {EFFORT_LABELS[e]}
                          </option>
                        ))}
                      </select>
                    </div>
                  ) : (
                    <span className="mono dim">{modelText}</span>
                  )}
                </div>
                <div className="wf-row-actions">
                  {isEditing ? (
                    <>
                      <button
                        className="primary"
                        onClick={() => void saveEdit(a.id)}
                        disabled={busy || !editModel.trim()}
                      >
                        保存
                      </button>
                      <button onClick={() => setEditing(null)} disabled={busy}>
                        取消
                      </button>
                    </>
                  ) : a.installed ? (
                    <>
                      <button className="link-btn" onClick={() => startEdit(a)} disabled={busy}>
                        自定义
                      </button>
                      <button onClick={() => requestRemove(a.id)} disabled={busy}>
                        移除
                      </button>
                    </>
                  ) : (
                    <>
                      <button className="primary" onClick={() => void install(a.id)} disabled={busy}>
                        启用
                      </button>
                      {a.backup_exists && (
                        <button className="link-btn" onClick={() => void restore(a.id)} disabled={busy}>
                          恢复
                        </button>
                      )}
                    </>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      </section>

      <section className="card wf-card">
        <h2>使用说明</h2>
        <ol className="wf-steps">
          <li>
            启用预设后，重新打开 Codex 或新开会话，在 Agent 选择中即可看到{" "}
            <span className="mono">luna_worker</span> / <span className="mono">sol_planner</span>{" "}
            / <span className="mono">sol_reviewer</span>。
          </li>
          <li>
            主任务用 Sol（medium / high）派发与规划，简单任务直接做；复杂任务交给{" "}
            <span className="mono">@sol_planner</span> 拆解，边界清晰的实现交给{" "}
            <span className="mono">@luna_worker</span>，复杂审查用{" "}
            <span className="mono">@sol_reviewer</span>。
          </li>
          <li>
            Luna 的 Max 档位需要在 Codex 设置 → General → Model features → Available
            reasoning efforts 中勾选 Max 后才会生效（桌面版与 CLI 一致）。
          </li>
          <li>
            主窗口使用哪个模型与思考档位由 Codex 界面自己选择，本功能只负责把 Agent
            配置文件写好；Luna Max 与 Sol 规划都会消耗额度，建议按任务复杂度搭配使用。
          </li>
          <li>
            每个预设可点击「自定义」修改模型与思考档位（改动前自动备份）；想回到初始配置，
            点右上角的恢复默认图标即可，自定义内容也会先移入备份。
          </li>
        </ol>
      </section>

      {others.length > 0 && (
        <section className="card wf-card">
          <h2>本机其他自定义 Agent</h2>
          <p className="hint">以下文件不会被改动，仅用于查看当前 ~/.codex/agents 下的已有内容。</p>
          <div className="wf-rows">
            {others.map((a) => (
              <div key={a.id} className="wf-row">
                <div className="wf-row-main">
                  <div className="wf-row-title">
                    <strong>{a.name}</strong>
                  </div>
                  <span className="hint">{a.description || "（无描述）"}</span>
                  <span className="mono dim">{a.path}</span>
                </div>
                <div className="wf-row-actions">
                  <span className="wf-badge installed">已存在</span>
                </div>
              </div>
            ))}
          </div>
        </section>
      )}
    </div>
  );
}
