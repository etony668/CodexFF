import { useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  AppStatus,
  DnsLeakResult,
  checkIp,
  errMsg,
  fmtResolverIps,
} from "../api";
import type { ToastRequest } from "../FloatingToast";

interface Props {
  status: AppStatus | null;
  onChanged: () => Promise<void>;
  /** DNS 泄露检测结果 (由 App 层统一持有, 与顶部 banner 同步) */
  dnsLeak: DnsLeakResult | null;
  dnsChecking: boolean;
  onRunDnsCheck: () => void;
  onToast?: (toast: ToastRequest) => void;
}

export function SettingsPage({
  status,
  onChanged,
  dnsLeak,
  dnsChecking,
  onRunDnsCheck,
  onToast,
}: Props) {
  const [checking, setChecking] = useState(false);
  const [ip, setIp] = useState(status?.ip ?? null);

  // status 刷新后同步 — 切官方/刷新会更新基线, 页面不能停在旧值
  useEffect(() => {
    setIp(status?.ip ?? null);
  }, [status?.ip]);

  async function recheck() {
    setChecking(true);
    try {
      setIp(await checkIp());
    } catch (e) {
      onToast?.({
        title: "出口 IP 检测失败",
        message: errMsg(e),
        kind: "warn",
      });
    } finally {
      setChecking(false);
    }
  }

  const safe = ip && !ip.unknown && !ip.changed;
  const risk = ip && ip.changed;

  return (
    <div className="page">
      <section className="card">
        <h2>网络出口守护</h2>
        <p>
          封号主因之一: 官方账号活跃 IP 频繁变化。
          每次切到官方时记录出口 IP, 变了就警告。
        </p>

        <div className={`ip-status ${risk ? "risk" : safe ? "safe" : ""}`}>
          <div>
            当前出口: <code>{ip?.current_ip ?? "检测中…"}</code>
          </div>
          <div>
            上次官方: <code>{ip?.last_official_ip ?? "无记录"}</code>
          </div>
          {risk && (
            <p className="error">
              <svg
                width="14"
                height="14"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                style={{ verticalAlign: "-2px", marginRight: 6 }}
              >
                <path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
                <line x1="12" y1="9" x2="12" y2="13" />
                <line x1="12" y1="17" x2="12.01" y2="17" />
              </svg>
              IP 已变! 官方账号从新 IP 访问有封号风险。
              请固定网络出口后再切官方。
            </p>
          )}
          {safe && <p className="ok">✓ 出口 IP 与上次官方一致</p>}
          {ip?.unknown && (
            <p className="hint">
              首次检测或无基线 — 切一次官方模式后建立基线。
            </p>
          )}
        </div>
        <button onClick={recheck} disabled={checking} className="primary">
          {checking ? "检测中…" : "重新检测"}
        </button>
      </section>

      <section className="card">
        <h2>DNS 泄露检测</h2>
        <p>
          DNS 泄露检测: 系统解析器查唯一子域名, 对方权威 DNS 记录解析器出口
          IP。
        </p>

        <div className="ip-status">
          <div>
            解析器出口:{" "}
            <code title={(dnsLeak?.resolver_ips ?? []).join(", ")}>
              {fmtResolverIps(dnsLeak?.resolver_ips ?? [], 4, ", ") || "未检测"}
            </code>
          </div>
          <div>
            当前出口: <code>{dnsLeak?.current_ip ?? "未检测"}</code>
          </div>
          {dnsLeak?.leaking === true && (
            <p className="warn">
              <svg
                width="14"
                height="14"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                style={{ verticalAlign: "-2px", marginRight: 6 }}
              >
                <path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
                <line x1="12" y1="9" x2="12" y2="13" />
                <line x1="12" y1="17" x2="12.01" y2="17" />
              </svg>
              DNS 泄露! 解析器出口 ≠ 当前出口 — DNS 未与网络流量走同一出口。
            </p>
          )}
          {dnsLeak?.leaking === false && (
            <p className="ok">✓ 无泄露 — 解析器与出口一致</p>
          )}
          {dnsLeak?.leaking === null && dnsLeak && (
            <p className="hint">{dnsLeak.error ?? "无法判定"}</p>
          )}
        </div>
        <div className="dns-check-actions">
          <button onClick={onRunDnsCheck} disabled={dnsChecking} className="primary">
            {dnsChecking ? "检测中…" : "检测 DNS 泄露"}
          </button>
          <button
            type="button"
            className="link-btn"
            onClick={() => void openUrl("https://ip.net.coffee/dns/")}
          >
            第三方权威检测 ↗
          </button>
        </div>
      </section>

      <section className="card">
        <h2>凭证金库</h2>
        <p>
          官方登录凭证在 vault 里:{" "}
          <code>~/Library/Application Support/codexff/vault/</code>
        </p>
        <p>
          中转模式下官方凭证从 <code>~/.codex/auth.json</code> 物理移除,
          即使配置被篡改也无法发给第三方。
        </p>
        <p>
          官方凭证已保存:{" "}
          {status?.official_login_present ? (
            <span className="ok">✓ 是</span>
          ) : (
            <span className="warn">否 (切官方后运行 codex login)</span>
          )}
        </p>
      </section>

      <section className="card">
        <h2>使用建议</h2>
        <ul>
          <li>官方模式: 固定一个网络出口，避免出口自动变化</li>
          <li>官方模式: 少开多端 (手机/网页/桌面同时活跃会叠加风控)</li>
          <li>官方模式: 避免 24/7 自动化脚本特征 (高频/整点间隔请求)</li>
          <li>中转模式: 流量走中转站出口, 与官方账号零关联</li>
          <li>中转 key 只买中转站发的独立 key, 别填自己的官方凭证</li>
        </ul>
        <h3 style={{ fontSize: 13, marginTop: 14 }}>账号风控 (2026 版)</h3>
        <ul>
          <li>官方账号活跃 IP 频繁变化 = 第一大封号信号 — 切官方前 CodexFF 会硬检查 IP 基线</li>
          <li>共享出口节点 (数百人共用一个出口) 风险高, 一人违规可能连带全池标记 — 建议专属节点/家宽</li>
          <li>数据中心/机房 IP 访问官方 = 高危 (批量账号池特征), 官方卡片会显示出口类型警告</li>
          <li>付款: 实体卡、卡区与账号区一致、一卡一号; 虚拟卡/礼品卡/一卡多号触发风控</li>
          <li>不要买卖/共用账号 — 前任的风险关联会传染到账号</li>
          <li>短时间频繁切换官方↔中转也构成出口抖动信号, CodexFF 会提示</li>
        </ul>
        <button onClick={onChanged}>刷新状态</button>
      </section>
    </div>
  );
}
