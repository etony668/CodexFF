import { useEffect, useRef, useState } from "react";

export type FloatingToastKind = "info" | "warn" | "confirm";

export interface FloatingToastProps {
  title?: string;
  message: string;
  kind?: FloatingToastKind;
  /** 非确认提示自动消失时间, 默认 5s */
  durationMs?: number;
  confirmLabel?: string;
  cancelLabel?: string;
  /** 确认框里的额外选项按钮（渲染在主确认按钮之后、取消之前） */
  extraActions?: { label: string; onClick: () => void }[];
  onConfirm?: () => void;
  onClose: () => void;
}

export function FloatingToast({
  title,
  message,
  kind = "info",
  durationMs = 5000,
  confirmLabel = "确认",
  cancelLabel = "取消",
  extraActions,
  onConfirm,
  onClose,
}: FloatingToastProps) {
  const isConfirm = kind === "confirm";
  const onCloseRef = useRef(onClose);
  const [remainingSeconds, setRemainingSeconds] = useState(() =>
    Math.max(1, Math.ceil(durationMs / 1000)),
  );
  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  // 需要用户确认时取消倒计时, 由用户点击确认/取消关闭;
  // 普通提示统一展示倒计时并按 durationMs 自动消失。
  useEffect(() => {
    if (isConfirm) return;
    const startedAt = Date.now();
    setRemainingSeconds(Math.max(1, Math.ceil(durationMs / 1000)));
    const countdown = window.setInterval(() => {
      const remaining = Math.max(
        1,
        Math.ceil((durationMs - (Date.now() - startedAt)) / 1000),
      );
      setRemainingSeconds(remaining);
    }, 200);
    const timer = window.setTimeout(() => onCloseRef.current(), durationMs);
    return () => {
      window.clearInterval(countdown);
      window.clearTimeout(timer);
    };
  }, [isConfirm, durationMs, title, message]);

  return (
    <div
      className={`floating-toast ${kind}`}
      role={isConfirm ? "alertdialog" : "alert"}
    >
      {title && <span className="floating-toast-title">{title}</span>}
      <span className="floating-toast-message">
        {message}
        {!isConfirm && `（${remainingSeconds} 秒后自动关闭）`}
      </span>
      {isConfirm ? (
        <div className="floating-toast-actions">
          <button className="primary" onClick={onConfirm}>
            {confirmLabel}
          </button>
          {extraActions?.map((a) => (
            <button key={a.label} onClick={a.onClick}>
              {a.label}
            </button>
          ))}
          <button onClick={onClose}>{cancelLabel}</button>
        </div>
      ) : (
        <div
          className="floating-toast-progress"
          style={{ animationDuration: `${durationMs}ms` }}
          aria-hidden="true"
        />
      )}
    </div>
  );
}

/** 子页面请求 App 统一弹出悬浮提示的载荷 */
export interface ToastRequest {
  title: string;
  message: string;
  kind?: FloatingToastKind;
  confirmLabel?: string;
  cancelLabel?: string;
  extraActions?: { label: string; onClick: () => void }[];
  onConfirm?: () => void;
}
