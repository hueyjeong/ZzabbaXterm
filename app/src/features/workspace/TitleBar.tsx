import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { ReactNode } from "react";

const native = isTauri();
export const macTitleBar = /Macintosh/.test(navigator.userAgent);
const windowControls = [
  { action: "minimize", label: "최소화", icon: "−" },
  { action: "toggleMaximize", label: "최대화/복원", icon: "□" },
  { action: "close", label: "닫기", icon: "×" },
] as const;

export function TitleBar({ focused, onToggle, onWindowError, children }: {
  focused: boolean;
  onToggle: () => void;
  onWindowError: (message: string) => void;
  children: ReactNode;
}) {
  async function run({ action, label }: (typeof windowControls)[number]) {
    onWindowError("");
    try {
      await getCurrentWindow()[action]();
    } catch (error) {
      onWindowError(`${label} 실패: ${String(error)}`);
    }
  }

  return (
    <header className="workspace-titlebar">
      <div className="workspace-handle" data-tauri-drag-region={native ? "" : undefined}>
        {!focused && "ZzabbaXterm"}
      </div>
      <nav className="workspace-tabs" aria-label="로컬 터미널">{children}</nav>
      <div className="workspace-drag-gap" data-tauri-drag-region={native ? "" : undefined} />
      <button className="workspace-mode" aria-label={focused ? "기본 보기" : "집중 보기"}
        onMouseDown={(event) => { if (event.button === 0) event.preventDefault(); }}
        onClick={onToggle}>{focused ? "기본 보기" : "집중 보기"}</button>
      {native && !macTitleBar && windowControls.map((control) => (
        <button className={`window-control ${control.action}`} key={control.action}
          aria-label={control.label} title={control.label} onClick={() => void run(control)}>{control.icon}</button>
      ))}
    </header>
  );
}
