import { CodexEnvironmentCard } from "./CodexEnvironmentCard";
import type { ToastRequest } from "../FloatingToast";

interface Props {
  onToast?: (toast: ToastRequest) => void;
}

export function SettingsTab({ onToast }: Props) {
  return (
    <div className="page">
      <CodexEnvironmentCard onToast={onToast} />
    </div>
  );
}
