import { useState } from "react";
import { quitApp } from "./api";
import privacyDarkImg from "./assets/firstrun-privacy-dark.png";
import privacyLightImg from "./assets/firstrun-privacy-light.png";
interface Props {
  onDone: () => void;
  /** 当前实际生效的主题 (auto 模式下跟随系统) */
  dark: boolean;
}

const PAGES = [
  {
    title: "隐私说明",
    body: "你的登录凭证、密钥等只保存在这台 Mac 上并进行加密存储，App 不会上传、不会收集任何使用数据。切换供应商、会话与历史记录也全部由本地管理，你可以随时查看或删除。",
    img: { light: privacyLightImg, dark: privacyDarkImg },
    caption: "隐私保护示意图",
  },
] as const;

export function FirstRunGuide({ onDone, dark }: Props) {
  const [page, setPage] = useState(0);
  const current = PAGES[page];

  return (
    <div className="firstrun-overlay" role="dialog" aria-modal="true">
      <div className="firstrun-modal">
        <div className="firstrun-head">
          <h2 className="firstrun-title">{current.title}</h2>
          <div className="firstrun-head-right">
            <span className="firstrun-step">{page + 1} / {PAGES.length}</span>
            <button
              className="firstrun-close"
              onClick={() => void quitApp()}
              title="退出应用"
              aria-label="退出应用"
            >
              ×
            </button>
          </div>
        </div>
        <p className="firstrun-copy">{current.body}</p>
        {current.img && (
          <div className="firstrun-img">
            <img
              src={dark ? current.img.dark : current.img.light}
              alt={current.caption ?? ""}
            />
          </div>
        )}
        <div className="firstrun-nav">
          <button
            onClick={() => setPage((p) => Math.max(0, p - 1))}
            disabled={page === 0}
          >
            上一步
          </button>
          <div className="firstrun-dots">
            {PAGES.map((_, i) => (
              <span
                key={i}
                className={`firstrun-dot${i === page ? " active" : ""}`}
              />
            ))}
          </div>
          {page < PAGES.length - 1 ? (
            <button className="primary" onClick={() => setPage((p) => p + 1)}>
              下一步
            </button>
          ) : (
            <button className="primary" onClick={onDone}>
              我知道了，开始使用
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
