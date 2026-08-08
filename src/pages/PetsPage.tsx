import { useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  PetFileInput,
  PetMeta,
  cancelPetCommandInstall,
  deletePet,
  errMsg,
  installPetFromCommand,
  importPetFolder,
  importPetZip,
  listPets,
} from "../api";
import type { ToastRequest } from "../FloatingToast";

interface Props {
  /** 请求 App 层统一弹出悬浮提示 */
  onToast?: (t: ToastRequest) => void;
}

/** 开源社区宠物仓库索引 (仅引流, 不提供下载/不承担版权责任) */
const COMMUNITY_REPOS: { repo: string; author: string; note: string }[] = [
  {
    repo: "petdex",
    author: "crafter-station",
    note: "社区宠物库聚合站，收录数千只宠物",
  },
  {
    repo: "awesome-codex-pet",
    author: "legeling",
    note: "精选 Codex 宠物合集，带完整动作预览",
  },
  {
    repo: "awesome_pets",
    author: "Nitrogen216",
    note: "精选宠物合集",
  },
  {
    repo: "codex-pets",
    author: "astandrik",
    note: "社区宠物画廊，支持公开生成请求与托管",
  },
  {
    repo: "pet",
    author: "lencx",
    note: "AI 编程伙伴宠物素材资源集",
  },
  {
    repo: "aemeath-codex-pet",
    author: "ChuyuZhong",
    note: "爱弥斯宠物包（像素风）",
  },
  {
    repo: "yuexinmiao-codex-pet",
    author: "WenNinghan",
    note: "月薪喵宠物包（v2 完整方向行）",
  },
  {
    repo: "hutchling",
    author: "FredHutch",
    note: "哈钦宠物包（Fred Hutch 主题）",
  },
  {
    repo: "pet",
    author: "debug-zhang",
    note: "Niuniu 宠物包示例（v2）",
  },
  {
    repo: "clawdex",
    author: "danielkempe",
    note: "Claude Code 宠物兼容层，内置宠物列表",
  },
];

export function PetsPage({ onToast }: Props) {
  const [pets, setPets] = useState<PetMeta[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [installCmd, setInstallCmd] = useState("");
  const [installLog, setInstallLog] = useState<string[]>([]);
  const logRef = useRef<HTMLPreElement>(null);
  const zipInput = useRef<HTMLInputElement>(null);
  const dirInput = useRef<HTMLInputElement>(null);

  async function refresh() {
    try {
      setPets(await listPets());
    } catch (e) {
      onToast?.({ title: "加载失败", message: errMsg(e) });
    }
  }

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<string>("pet-install-output", (event) => {
      if (disposed || !event.payload) return;
      setInstallLog((prev) => [...prev.slice(-199), event.payload]);
    }).then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [installLog]);

  function fileToBase64(file: File): Promise<string> {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => {
        const result = reader.result as string;
        const comma = result.indexOf(",");
        resolve(comma >= 0 ? result.slice(comma + 1) : result);
      };
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(file);
    });
  }

  async function handleZip(file: File | undefined) {
    if (!file || busy) return;
    setBusy(true);
    try {
      const dataBase64 = await fileToBase64(file);
      const pet = await importPetZip(file.name, dataBase64);
      onToast?.({
        title: "导入成功",
        message: `宠物「${pet.name}」已安装，到 Codex 设置 → 外观 → Pets 中选择生效。`,
      });
      await refresh();
    } catch (e) {
      onToast?.({ title: "导入失败", message: errMsg(e) });
    } finally {
      setBusy(false);
      if (zipInput.current) zipInput.current.value = "";
    }
  }

  async function handleDir(files: FileList | null) {
    if (!files || files.length === 0 || busy) return;
    setBusy(true);
    try {
      const inputs: PetFileInput[] = [];
      for (const f of Array.from(files)) {
        if (!f.webkitRelativePath) continue;
        inputs.push({ path: f.webkitRelativePath, dataBase64: await fileToBase64(f) });
      }
      const pet = await importPetFolder(inputs);
      onToast?.({
        title: "导入成功",
        message: `宠物「${pet.name}」已安装，到 Codex 设置 → 外观 → Pets 中选择生效。`,
      });
      await refresh();
    } catch (e) {
      onToast?.({ title: "导入失败", message: errMsg(e) });
    } finally {
      setBusy(false);
      if (dirInput.current) dirInput.current.value = "";
    }
  }

  function requestDelete(pet: PetMeta) {
    if (busy) return;
    onToast?.({
      kind: "confirm",
      title: "删除宠物？",
      message: `「${pet.name}」将从 ~/.codex/pets 移除并移入金库回收区（可找回），Codex 中会恢复默认宠物。`,
      confirmLabel: "删除",
      cancelLabel: "取消",
      onConfirm: () => {
        void doDelete(pet);
      },
    });
  }

  async function doDelete(pet: PetMeta) {
    setBusy(true);
    try {
      await deletePet(pet.id);
      await refresh();
      onToast?.({
        title: "已删除",
        message: `「${pet.name}」已移入金库回收区。`,
      });
    } catch (e) {
      onToast?.({ title: "删除失败", message: errMsg(e) });
    } finally {
      setBusy(false);
    }
  }

  function handleCommandInstall() {
    const command = installCmd.trim();
    if (!command || busy || installing) return;
    onToast?.({
      kind: "confirm",
      title: "执行安装命令？",
      message: `将直接运行以下命令，请只粘贴可信来源的命令：\n\n${command}`,
      confirmLabel: "安装",
      cancelLabel: "取消",
      onConfirm: () => {
        void doCommandInstall(command);
      },
    });
  }

  async function doCommandInstall(command: string) {
    setInstalling(true);
    setInstallLog([]);
    try {
      const installed = await installPetFromCommand(command);
      await refresh();
      // 命令执行成功即清空输入框, 避免误以为还能再点一次
      setInstallCmd("");
      if (installed.length === 0) {
        onToast?.({
          title: "命令执行成功",
          message: "未发现新宠物，可能已安装过或命令只是更新了现有宠物。",
        });
      } else if (installed.length === 1) {
        onToast?.({
          title: "安装成功",
          message: `宠物「${installed[0].name}」已安装，到 Codex 设置 → 外观 → Pets 中选择生效。`,
        });
      } else {
        onToast?.({
          title: "安装成功",
          message: `已安装 ${installed.length} 只宠物，到 Codex 设置 → 外观 → Pets 中选择生效。`,
        });
      }
    } catch (e) {
      onToast?.({ title: "安装失败", message: errMsg(e) });
    } finally {
      setInstalling(false);
    }
  }

  async function doCancelCommandInstall() {
    try {
      await cancelPetCommandInstall();
    } catch (e) {
      onToast?.({ title: "取消失败", message: errMsg(e) });
    }
  }

  function fmtSize(bytes: number) {
    if (bytes >= 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + " MB";
    if (bytes >= 1024) return (bytes / 1024).toFixed(1) + " KB";
    return bytes + " B";
  }

  return (
    <div className="page">
      <section className="card pets-card">
        <div className="pets-head">
          <div className="pets-copy">
            <h2>宠物管理</h2>
            <p className="hint">
              导入完成后，在 Codex 设置 → 外观 → Pets 中选择生效。
              支持导入社区宠物包（ZIP 或文件夹），导入时会校验图集格式。
            </p>
          </div>
          <div className="pets-actions">
            <button onClick={() => zipInput.current?.click()} disabled={busy}>
              导入 ZIP
            </button>
            <button onClick={() => dirInput.current?.click()} disabled={busy}>
              导入文件夹
            </button>
            <button className="link-btn" onClick={() => void refresh()} disabled={busy}>
              刷新
            </button>
          </div>
          <input
            ref={zipInput}
            type="file"
            accept=".zip,application/zip"
            hidden
            onChange={(e) => void handleZip(e.target.files?.[0])}
          />
          <input
            ref={dirInput}
            type="file"
            hidden
            multiple
            {...({ webkitdirectory: "" } as Record<string, unknown>)}
            onChange={(e) => void handleDir(e.target.files)}
          />
        </div>

        {busy && <p className="hint">处理中…</p>}
        {pets && pets.length === 0 && !busy && (
          <p className="hint empty-pets">
            还没有自定义宠物。导入一个宠物包试试。
          </p>
        )}
        {pets && pets.length > 0 && (
          <div className="pets-grid">
            {pets.map((pet) => (
              <div key={pet.id} className={`pet-card${pet.valid ? "" : " invalid"}`}>
                <div className="pet-preview">
                  <img src={convertFileSrc(pet.spritesheet_path)} alt={pet.name} loading="lazy" />
                </div>
                <div className="pet-info">
                  <div className="pet-name-row">
                    <strong>{pet.name}</strong>
                    <span className={`pet-badge${pet.valid ? "" : " bad"}`}>
                      V{pet.sprite_version}
                    </span>
                  </div>
                  {pet.description && <p className="pet-desc">{pet.description}</p>}
                  <p className={`pet-status${pet.valid ? " ok" : " warn"}`}>
                    {pet.valid ? "✓ 可用" : `⚠ ${pet.validation}`}
                  </p>
                  <div className="pet-meta">
                    <span className="mono dim">{pet.id}</span>
                    <span className="dim">{fmtSize(pet.size_bytes)}</span>
                  </div>
                  <div className="pet-actions">
                    <button
                      className="link-btn"
                      onClick={() => void revealItemInDir(pet.spritesheet_path)}
                    >
                      在 Finder 中显示
                    </button>
                    <button
                      className="link-btn danger-link"
                      onClick={() => requestDelete(pet)}
                      disabled={busy}
                    >
                      删除
                    </button>
                  </div>
                </div>
              </div>
            ))}
          </div>
        )}
      </section>

      <section className="card pets-card">
        <h2>终端命令安装</h2>
        <p className="hint">
          部分社区仓库提供一键安装命令（例如{" "}
          <span className="mono">npx petdex install boba</span>
          ）。把命令粘贴到下方，点击“安装”即可自动下载并安装到
          ~/.codex/pets，无需打开终端；多行命令也可以直接整段粘贴。
        </p>
        <div className="cmd-install-row">
          <textarea
            className="cmd-install-input"
            value={installCmd}
            onChange={(e) => setInstallCmd(e.target.value)}
            placeholder={"例如：\nnpx petdex install boba\n或：\ncurl -fsSL https://lencx.me/pet/install.sh | sh -s -- kerno"}
            disabled={installing}
            spellCheck={false}
            rows={3}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && !installing) {
                handleCommandInstall();
              }
            }}
          />
          <div className="cmd-install-btns">
            <button
              className="primary"
              onClick={handleCommandInstall}
              disabled={installing || busy || !installCmd.trim()}
            >
              安装
            </button>
            {installing && (
              <button onClick={() => void doCancelCommandInstall()} disabled={busy}>
                取消
              </button>
            )}
          </div>
        </div>
        {installing && <p className="hint">正在下载并安装…（可点击“取消”终止）</p>}
        {installLog.length > 0 && (
          <pre ref={logRef} className="cmd-install-log">
            {installLog.join("\n")}
          </pre>
        )}
      </section>

      <section className="card pets-card">
        <h2>社区宠物来源</h2>
        <p className="hint">
          以下为开源社区仓库索引（作者 / 仓库名），仅作参考不提供下载；请自行确认各仓库的许可条款后再下载，
          下载后通过上方“导入 ZIP / 导入文件夹”安装。
        </p>
        <div className="repo-rows">
          {COMMUNITY_REPOS.map((r) => (
            <div key={`${r.author}/${r.repo}`} className="repo-row">
              <div className="repo-name">
                <span className="mono">{r.repo}</span>
                <span className="dim">· {r.author}</span>
              </div>
              <span className="repo-note">{r.note}</span>
            </div>
          ))}
        </div>
      </section>
    </div>
  );
}
