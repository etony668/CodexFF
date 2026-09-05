import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  CodexInstallStatus,
  errMsg,
  getCodexInstallStatus,
  installCodex,
  updateCodexCli,
  updateCodexDesktop,
} from "../api";
import type { ToastRequest } from "../FloatingToast";

interface Props {
  onToast?: (toast: ToastRequest) => void;
}

export function CodexEnvironmentCard({ onToast }: Props) {
  const [codexStatus, setCodexStatus] = useState<CodexInstallStatus | null>(null);
  const [codexChecking, setCodexChecking] = useState(false);
  const [codexInstalling, setCodexInstalling] = useState(false);
  const [codexProgress, setCodexProgress] = useState<{
    component: string;
    phase: string;
    percent: number;
    message: string;
  } | null>(null);

  const showError = (title: string, message: string) =>
    onToast?.({ title, message, kind: "warn" });

  async function refreshCodexStatus(checkLatest = false) {
    if (checkLatest) setCodexChecking(true);
    try {
      setCodexStatus(await getCodexInstallStatus(checkLatest));
    } catch (e) {
      if (checkLatest) showError("检查版本失败", errMsg(e));
    } finally {
      if (checkLatest) setCodexChecking(false);
    }
  }

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void getCodexInstallStatus(false)
      .then((next) => {
        if (!disposed) setCodexStatus(next);
      })
      .catch(() => {});
    void listen<{
      component: string;
      phase: string;
      percent: number;
      message: string;
    }>("codex-install-progress", (e) => {
      if (disposed || !e.payload) return;
      setCodexProgress(e.payload);
    }).then((fn) => {
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
      const next = await getCodexInstallStatus(true);
      setCodexStatus(next);
      const ready = next.desktop.installed && next.cli.installed;
      setCodexProgress({
        component: "all",
        phase: ready ? "完成" : "等待安装",
        percent: ready ? 100 : -1,
        message: ready
          ? "Codex 桌面端与 CLI 已准备完成"
          : "CLI 已处理；请完成桌面端安装后点击“检测更新”",
      });
    } catch (e) {
      showError("安装失败", errMsg(e));
    } finally {
      setCodexInstalling(false);
    }
  }

  async function doUpdateCodex(component: "desktop" | "cli") {
    if (codexInstalling) return;
    setCodexInstalling(true);
    setCodexProgress(null);
    try {
      if (component === "desktop") await updateCodexDesktop();
      else await updateCodexCli();
      await refreshCodexStatus(true);
    } catch (e) {
      showError(component === "desktop" ? "桌面端更新失败" : "CLI 更新失败", errMsg(e));
    } finally {
      setCodexInstalling(false);
    }
  }

  if (!codexStatus) {
    return (
      <section className="card">
        <div className="settings-card-heading">
          <h2>Codex 运行环境</h2>
          <button
            type="button"
            className="ghost"
            onClick={() => void refreshCodexStatus(true)}
            disabled={codexChecking}
          >
            {codexChecking ? "检测中…" : "检测更新"}
          </button>
        </div>
        <p className="hint">正在读取 Codex 桌面端与 CLI 状态…</p>
      </section>
    );
  }

  return (
    <section className="card">
      <div className="settings-card-heading">
        <h2>Codex 运行环境</h2>
        <button
          type="button"
          className="ghost"
          onClick={() => void refreshCodexStatus(true)}
          disabled={codexInstalling || codexChecking}
        >
          {codexChecking ? "检测中…" : "检测更新"}
        </button>
      </div>
      <div className="router-card codex-components-card">
        <div className="router-copy codex-components-copy">
          <strong>Codex 桌面端与 CLI</strong>
          <span className="hint">
            一键补齐官方桌面端和 CLI；版本检测仅访问 OpenAI 与 npm 官方更新源。
          </span>
          <div className="codex-component-list">
            {([
              ["desktop", "Codex 桌面端", codexStatus.desktop],
              ["cli", "Codex CLI", codexStatus.cli],
            ] as const).map(([kind, label, component]) => (
              <div className="codex-component-row" key={kind}>
                <div className="codex-component-main">
                  <span className={`codex-component-dot ${component.installed ? "ok" : "missing"}`} />
                  <span>{label}</span>
                  <span className="hint">
                    {component.installed
                      ? `已安装${component.current_version ? ` · v${component.current_version}` : ""}`
                      : "未安装"}
                    {component.latest_version ? ` · 最新 v${component.latest_version}` : ""}
                  </span>
                  {component.error && <span className="hint warn">· {component.error}</span>}
                </div>
                {(component.update_available || !component.installed) && (
                  <button
                    type="button"
                    className="ghost codex-component-action"
                    disabled={codexInstalling}
                    onClick={() =>
                      component.installed
                        ? void doUpdateCodex(kind)
                        : kind === "desktop"
                          ? void doInstallCodex()
                          : void doUpdateCodex("cli")
                    }
                  >
                    {component.installed ? "更新" : "安装"}
                  </button>
                )}
              </div>
            ))}
          </div>
          {codexProgress && (
            <div className="codex-install-progress">
              <div className="quota-label">
                <span>
                  {codexProgress.component === "cli"
                    ? "Codex CLI"
                    : codexProgress.component === "desktop"
                      ? "Codex 桌面端"
                      : codexProgress.phase}
                </span>
                <span>
                  {codexProgress.percent >= 0
                    ? `${Math.round(codexProgress.percent)}%`
                    : "处理中…"}
                </span>
              </div>
              <div className="quota-bar">
                <div
                  className={`quota-fill${codexProgress.percent < 0 ? " indeterminate" : ""}`}
                  style={{
                    width: codexProgress.percent >= 0 ? `${Math.min(100, codexProgress.percent)}%` : "20%",
                  }}
                />
              </div>
              <span className="hint">{codexProgress.message}</span>
            </div>
          )}
        </div>
        {(!codexStatus.desktop.installed || !codexStatus.cli.installed) && (
          <div className="router-actions codex-environment-actions">
            <button type="button" className="primary" onClick={() => void doInstallCodex()} disabled={codexInstalling}>
              {codexInstalling ? "安装中…" : "一键下载安装"}
            </button>
          </div>
        )}
      </div>
    </section>
  );
}
