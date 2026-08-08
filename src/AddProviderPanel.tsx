import { useEffect, useMemo, useRef, useState } from "react";
import {
  RelayProfile,
  RelayProfileInput,
  RelayTestResult,
  errMsg,
  getCommonConfig,
  getDefaultConfigToml,
  getRelayKey,
  setCommonConfig,
  testRelay,
} from "./api";
import { CATEGORY_LABELS, CodexffPreset, codexffPresets } from "./presets";

interface Props {
  open: boolean;
  /** 编辑对象 (null = 添加) */
  editing: RelayProfile | null;
  onClose: () => void;
  /** 保存 (添加或更新) */
  onSave: (input: RelayProfileInput) => Promise<void>;
  onSaved: () => void;
}

interface Form {
  name: string;
  baseUrl: string;
  model: string;
  wireApi: string;
  key: string;
  reasoningEffort: string;
  disableStorage: boolean;
  /** 上下文窗口 (token) — 字符串输入, 空 = 不写 */
  modelContextWindow: string;
  /** 超限自动压缩阈值 — 空 = 不写 */
  autoCompactLimit: string;
  notes: string;
  websiteUrl: string;
  anthropicAuthField: string;
  authJson: string;
  configToml: string;
  useCommonConfig: boolean;
}

/** 官方模型默认上下文窗口 (GPT-5.6 Codex, 400K) — cc-switch 对齐 */
const OFFICIAL_DEFAULT_CTX = 400000;
/** 默认压缩阈值 = 90% 窗口 (cc-switch 同比例: 1000000/900000) */
function defaultCompactLimit(ctx: number) {
  return Math.round(ctx * 0.9);
}

const emptyForm: Form = {
  name: "",
  baseUrl: "",
  model: "",
  wireApi: "responses",
  key: "",
  reasoningEffort: "",
  disableStorage: true,
  // 默认填官方模型默认值 (用户要求)
  modelContextWindow: String(OFFICIAL_DEFAULT_CTX),
  autoCompactLimit: String(defaultCompactLimit(OFFICIAL_DEFAULT_CTX)),
  notes: "",
  websiteUrl: "",
  anthropicAuthField: "",
  authJson: "",
  configToml: "",
  useCommonConfig: false,
};

function fromProfile(p: RelayProfile): Form {
  return {
    name: p.name,
    baseUrl: p.base_url,
    model: p.model,
    wireApi: p.wire_api ?? "responses",
    key: "",
    reasoningEffort: p.model_reasoning_effort ?? "",
    disableStorage: p.disable_response_storage,
    modelContextWindow: p.model_context_window != null ? String(p.model_context_window) : "",
    autoCompactLimit:
      p.model_auto_compact_token_limit != null ? String(p.model_auto_compact_token_limit) : "",
    notes: p.notes ?? "",
    websiteUrl: p.website_url ?? "",
    anthropicAuthField: p.anthropic_auth_field ?? "",
    authJson: p.auth_json ?? "",
    configToml: p.config_toml ?? "",
    useCommonConfig: p.use_common_config,
  };
}

/** 数字输入 → Option<u64> (留空 → 0 表示清空/不写; 非法 → null 不修改) */
function numOrNull(s: string): number | null {
  if (!s.trim()) return 0;
  const n = Number(s.trim());
  return Number.isInteger(n) && n > 0 ? n : null;
}

export function AddProviderPanel({ open, editing, onClose, onSave, onSaved }: Props) {
  // 阶段: presets (选预设) → form (填表)
  const [stage, setStage] = useState<"presets" | "form">("presets");
  const [query, setQuery] = useState("");
  const [cat, setCat] = useState<"all" | "cn_official" | "aggregator" | "third_party">("all");
  // configToml 来自预设 (手写类) — 用户改字段时清掉回程序化生成
  const presetToml = useRef(false);

  const [form, setForm] = useState<Form>(emptyForm);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  // 测试连接
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<RelayTestResult | null>(null);

  // 公共配置片段
  const [commonOpen, setCommonOpen] = useState(false);
  const [commonSnippet, setCommonSnippet] = useState("");
  const [commonSaving, setCommonSaving] = useState(false);
  const [commonErr, setCommonErr] = useState<string | null>(null);

  // 打开时重置 + 编辑回填
  useEffect(() => {
    if (!open) return;
    setStage("presets");
    setQuery("");
    setCat("all");
    setErr(null);
    setTestResult(null);
    setForm(editing ? fromProfile(editing) : emptyForm);
    if (editing) {
      setStage("form");
      getRelayKey(editing.id)
        .then((k) => setForm((f) => (f.name === editing.name ? { ...f, key: k } : f)))
        .catch(() => {});
      // 旧快照 (通用段机制前的底稿, 只有 provider 段) → 以磁盘为底重建完整底稿,
      // 否则编辑面板还是看不到 notify/mcp_servers/plugins 等通用段
      const t = editing.config_toml ?? "";
      const hasCommon = /(^|\n)\[(marketplaces|plugins|mcp_servers|desktop|notify)/.test(t);
      if (t && !hasCommon) {
        getDefaultConfigToml(t)
          .then((full) =>
            setForm((f) => (f.name === editing.name ? { ...f, configToml: full } : f)),
          )
          .catch(() => {});
      }
    }
  }, [open, editing]);

  useEffect(() => {
    getCommonConfig()
      .then((s) => setCommonSnippet(s ?? ""))
      .catch((e) => setCommonErr(errMsg(e)));
  }, []);

  const visiblePresets = useMemo(() => {
    const q = query.trim().toLowerCase();
    return codexffPresets.filter(
      (p: CodexffPreset) =>
        (cat === "all" || p.category === cat) && (!q || p.name.toLowerCase().includes(q)),
    );
  }, [query, cat]);

  async function pickPreset(p: CodexffPreset) {
    presetToml.current = true;
    // 上下文窗口: 预设声明优先, 否则官方模型默认值; 压缩阈值 90% 窗口
    const ctx = p.contextWindow ?? OFFICIAL_DEFAULT_CTX;
    // config.toml 底稿: 磁盘通用段 (notify/mcp_servers/marketplaces 等) +
    // 预设 provider 段 — 与 cc-switch 完整 TOML 底稿语义一致
    let configToml = "";
    try {
      configToml = await getDefaultConfigToml(p.config ?? null);
    } catch (e) {
      setErr(`读取默认配置失败: ${errMsg(e)}`);
    }
    setForm({
      name: p.name,
      baseUrl: p.baseUrl ?? "",
      model: p.model ?? "",
      wireApi: p.wireApi ?? "responses",
      key: "",
      reasoningEffort: p.reasoningEffort ?? "",
      disableStorage: true,
      modelContextWindow: String(ctx),
      autoCompactLimit: String(defaultCompactLimit(ctx)),
      notes: "",
      websiteUrl: p.websiteUrl ?? "",
      anthropicAuthField: "",
      // 预设 auth = {OPENAI_API_KEY: ""}; 保存时空 key 用表单 key 填充
      authJson: JSON.stringify({ OPENAI_API_KEY: "" }),
      configToml,
      useCommonConfig: false,
    });
    setStage("form");
  }

  async function pickCustom() {
    presetToml.current = true;
    let configToml = "";
    try {
      configToml = await getDefaultConfigToml(null);
    } catch (e) {
      setErr(`读取默认配置失败: ${errMsg(e)}`);
    }
    setForm({ ...emptyForm, configToml });
    setStage("form");
  }

  async function saveCommon() {
    setCommonSaving(true);
    setCommonErr(null);
    try {
      await setCommonConfig(commonSnippet);
      setCommonOpen(false);
      onSaved();
    } catch (e) {
      setCommonErr(errMsg(e));
    } finally {
      setCommonSaving(false);
    }
  }

  async function runTest() {
    if (!form.baseUrl || !form.key) {
      setErr("先填 Base URL 和 API Key 再测试");
      return;
    }
    setTesting(true);
    setErr(null);
    try {
      setTestResult(await testRelay(form.baseUrl, form.key, form.wireApi || null));
    } catch (e) {
      setErr(errMsg(e));
    } finally {
      setTesting(false);
    }
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      // auth.json 里空 key (预设模板) → 用表单 key 填充
      let authJson = form.authJson;
      if (authJson.trim()) {
        try {
          const obj = JSON.parse(authJson);
          if (obj && typeof obj === "object" && obj.OPENAI_API_KEY === "" && form.key) {
            obj.OPENAI_API_KEY = form.key;
            authJson = JSON.stringify(obj);
          }
        } catch {
          /* 非法 JSON 留给后端校验 */
        }
      }
      const input: RelayProfileInput = {
        name: form.name,
        base_url: form.baseUrl,
        model: form.model,
        wire_api: form.wireApi || null,
        key: editing ? form.key || null : form.key,
        model_reasoning_effort: form.reasoningEffort || null,
        disable_response_storage: form.disableStorage,
        model_context_window: numOrNull(form.modelContextWindow),
        model_auto_compact_token_limit: numOrNull(form.autoCompactLimit),
        notes: form.notes || null,
        website_url: form.websiteUrl || null,
        auth_json: authJson || null,
        config_toml: form.configToml || null,
        anthropic_auth_field: form.anthropicAuthField || null,
        use_common_config: form.useCommonConfig,
      };
      await onSave(input);
      onSaved();
      onClose();
    } catch (e) {
      setErr(errMsg(e));
    } finally {
      setBusy(false);
    }
  }

  if (!open) return null;

  const set = (patch: Partial<Form>) =>
    setForm((f) => {
      const next = { ...f, ...patch };
      // 字段级修改且 configToml 来自预设 → 清空回程序化生成 (底稿已过时)
      if (presetToml.current && ("baseUrl" in patch || "model" in patch || "wireApi" in patch)) {
        presetToml.current = false;
        next.configToml = "";
      }
      return next;
    });

  return (
    <div className="panel-overlay" onClick={(e) => e.target === e.currentTarget && onClose()}>
      <div className="panel">
        <div className="panel-header">
          <button className="panel-back" onClick={() => (stage === "form" ? setStage("presets") : onClose())}>
            ←
          </button>
          <h2>
            {editing
              ? `编辑: ${editing.name}`
              : stage === "form"
                ? `添加供应商: ${form.name || "自定义"}`
                : "添加供应商"}
          </h2>
          <button className="panel-close" onClick={onClose}>
            ×
          </button>
        </div>

        {stage === "presets" ? (
          <div className="panel-body">
            <p className="hint">
              选择预设自动填入请求地址 / 模型 / 配置, 再填 API Key 即可。或选"自定义"手动配置。
            </p>
            <div className="preset-toolbar">
              <input
                className="search"
                placeholder="搜索供应商…"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
              />
            </div>
            <div className="preset-cats">
              {(
                [
                  ["all", "全部"],
                  ["cn_official", "国产官方"],
                  ["aggregator", "聚合服务"],
                  ["third_party", "第三方中转"],
                ] as const
              ).map(([k, label]) => (
                <button
                  key={k}
                  className={cat === k ? "chip active" : "chip"}
                  onClick={() => setCat(k)}
                >
                  {label}
                </button>
              ))}
            </div>
            <div className="preset-grid">
              <button className="preset-card custom" onClick={pickCustom}>
                <span className="preset-avatar custom">＋</span>
                <span className="preset-meta">
                  <span className="preset-name">自定义</span>
                  <span className="preset-cat">手动配置</span>
                </span>
              </button>
              {visiblePresets.map((p) => (
                <button key={p.name} className="preset-card" onClick={() => pickPreset(p)}>
                  <span
                    className="preset-avatar"
                    style={{ backgroundColor: p.iconColor ?? "#2a2f3d" }}
                  >
                    {p.name.slice(0, 1)}
                  </span>
                  <span className="preset-meta">
                    <span className="preset-name">{p.name}</span>
                    <span className="preset-cat">{CATEGORY_LABELS[p.category]}</span>
                  </span>
                </button>
              ))}
              {visiblePresets.length === 0 && (
                <p className="hint preset-empty">没有匹配的供应商。</p>
              )}
            </div>
          </div>
        ) : (
          <div className="panel-body">
            <form onSubmit={submit} className="form">
              <label>
                供应商名称
                <input value={form.name} onChange={(e) => set({ name: e.target.value })} placeholder="relay-a" required />
              </label>
              <label>
                官网链接 (可选)
                <input value={form.websiteUrl} onChange={(e) => set({ websiteUrl: e.target.value })} placeholder="https://relay.example.com" />
              </label>
              <label>
                备注 (可选)
                <input value={form.notes} onChange={(e) => set({ notes: e.target.value })} placeholder="月付 ¥20" />
              </label>
              <label>
                API 请求地址 (Base URL)
                <input value={form.baseUrl} onChange={(e) => set({ baseUrl: e.target.value })} placeholder="https://api.example.com/v1" required />
              </label>
              <label>
                默认模型 (可选, 写入 config 顶层)
                <input value={form.model} onChange={(e) => set({ model: e.target.value })} placeholder="gpt-5.6-codex" />
              </label>
              <label>
                Wire API 格式
                <select value={form.wireApi} onChange={(e) => set({ wireApi: e.target.value })}>
                  <option value="responses">Responses (openai_responses)</option>
                  <option value="chat">Chat Completions (openai_chat)</option>
                  <option value="anthropic">Anthropic (anthropic)</option>
                  <option value="">自动</option>
                </select>
              </label>
              {form.wireApi === "anthropic" && (
                <label>
                  Anthropic 认证字段
                  <select
                    value={form.anthropicAuthField}
                    onChange={(e) => set({ anthropicAuthField: e.target.value })}
                  >
                    <option value="">默认 (ANTHROPIC_AUTH_TOKEN)</option>
                    <option value="ANTHROPIC_AUTH_TOKEN">ANTHROPIC_AUTH_TOKEN</option>
                    <option value="ANTHROPIC_API_KEY">ANTHROPIC_API_KEY</option>
                  </select>
                </label>
              )}
              <label>
                Reasoning Effort (可选, 也可直接在下方 config.toml 里写)
                <input value={form.reasoningEffort} onChange={(e) => set({ reasoningEffort: e.target.value })} placeholder="low / medium / high / xhigh / max" />
              </label>
              <label>
                上下文窗口 (token, 默认官方模型 400000)
                <input
                  value={form.modelContextWindow}
                  onChange={(e) => set({ modelContextWindow: e.target.value })}
                  placeholder="400000"
                  inputMode="numeric"
                />
              </label>
              <label>
                自动压缩阈值 (token, 默认 90% 窗口 = 360000)
                <input
                  value={form.autoCompactLimit}
                  onChange={(e) => set({ autoCompactLimit: e.target.value })}
                  placeholder="360000"
                  inputMode="numeric"
                />
              </label>
              <label className="checkbox-label">
                <input
                  type="checkbox"
                  checked={form.disableStorage}
                  onChange={(e) => set({ disableStorage: e.target.checked })}
                />
                禁用响应存储 (disable_response_storage — 中转站通常要求)
              </label>
              <label>
                API Key {editing && <span className="dim">(留空 = 不修改)</span>}
                <input
                  type="password"
                  value={form.key}
                  onChange={(e) => set({ key: e.target.value })}
                  placeholder="sk-..."
                  required={!editing}
                />
              </label>

              <details>
                <summary>auth.json (JSON) — 自定义完整内容, 留空自动生成</summary>
                <p className="hint">
                  切换时整份写入 ~/.codex/auth.json。默认自动生成 {"{OPENAI_API_KEY}"};
                  自定义内容不得包含官方 ChatGPT 登录凭证 (中转模式隔离承诺)。
                </p>
                <textarea
                  value={form.authJson}
                  onChange={(e) => set({ authJson: e.target.value })}
                  placeholder='{"OPENAI_API_KEY": "sk-...", "其他字段": ...}'
                  rows={6}
                  spellCheck={false}
                />
              </details>

              <details>
                <summary>config.toml (TOML) — 自定义底稿, 留空自动生成</summary>
                <p className="hint">
                  切换时以它为底写入 ~/.codex/config.toml (强制 model_provider="custom" +
                  注入缺失的中转表)。手写类预设会带完整底稿, 改上方字段会自动清空回程序化生成。
                </p>
                <textarea
                  value={form.configToml}
                  onChange={(e) => {
                    presetToml.current = false;
                    set({ configToml: e.target.value });
                  }}
                  placeholder={'model = "gpt-5.6-codex"\nmodel_reasoning_effort = "max"'}
                  rows={8}
                  spellCheck={false}
                />
              </details>

              <label className="checkbox-label">
                <input
                  type="checkbox"
                  checked={form.useCommonConfig}
                  onChange={(e) => set({ useCommonConfig: e.target.checked })}
                />
                合并全局公共配置片段 (writeCommonConfig)
              </label>

              <div className="test-row">
                <button type="button" onClick={() => setCommonOpen((v) => !v)} disabled={commonSaving}>
                  {commonOpen ? "收起公共配置" : "编辑公共配置片段"}
                </button>
                {commonOpen && (
                  <div className="common-editor">
                    <textarea
                      value={commonSnippet}
                      onChange={(e) => setCommonSnippet(e.target.value)}
                      placeholder={'model_reasoning_effort = "high"'}
                      rows={6}
                      spellCheck={false}
                    />
                    <div className="test-row">
                      <button type="button" className="primary" onClick={saveCommon} disabled={commonSaving}>
                        {commonSaving ? "保存中…" : "保存片段"}
                      </button>
                      <span className="hint">保存后启用"合并全局公共配置片段"的 profile 自动跟随</span>
                    </div>
                  </div>
                )}
              </div>

              <div className="test-row">
                <button type="button" onClick={runTest} disabled={testing || !form.baseUrl || !form.key}>
                  {testing ? "测试中…" : "测试连接"}
                </button>
                {testResult && (
                  <span className={testResult.ok ? "ok" : "error"}>
                    {testResult.ok
                      ? `✓ 连接成功, ${testResult.model_count ?? 0} 个模型${testResult.models.length ? `: ${testResult.models.join(", ")}` : ""}`
                      : `✗ ${testResult.error}`}
                  </span>
                )}
              </div>
              {commonErr && <p className="error">{commonErr}</p>}
              {err && <p className="error">{err}</p>}
              <div className="form-actions">
                <button type="submit" className="primary" disabled={busy}>
                  {editing ? "保存修改" : "添加"}
                </button>
                <button type="button" onClick={() => setStage("presets")} disabled={busy}>
                  取消
                </button>
              </div>
            </form>
          </div>
        )}
      </div>
    </div>
  );
}
