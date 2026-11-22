import { invoke, isTauri } from "@tauri-apps/api/core";
import type { MouseEvent, ReactNode } from "react";

const native = isTauri();
export const macTitleBar = /Macintosh/.test(navigator.userAgent);
const windowControls = [
  { action: "minimize", label: "최소화", icon: "−" },
  { action: "toggleMaximize", label: "최대화/복원", icon: "□" },
  { action: "close", label: "닫기", icon: "×" },
] as const;
type WindowAction = (typeof windowControls)[number]["action"] | "drag";

export function TitleBar({ focused, onToggle, onWindowError, children }: {
  focused: boolean;
  onToggle: () => void;
  onWindowError: (message: string) => void;
  children: ReactNode;
}) {
  async function run(action: WindowAction, label: string) {
    onWindowError("");
    try {
      await invoke("workspace_window", { action });
    } catch (error) {
      onWindowError(`${label} 실패: ${String(error)}`);
    }
  }

  function moveWindow(event: MouseEvent<HTMLDivElement>) {
    if (!native || event.button !== 0 || (event.detail !== 1 && event.detail !== 2)) return;
    event.preventDefault();
    if (event.detail === 1) void run("drag", "창 이동");
    else if (!macTitleBar) void run("toggleMaximize", "최대화/복원");
  }

  return (
    <header className="workspace-titlebar">
      <div className="workspace-handle" onMouseDown={moveWindow}>
        {!focused && "ZzabbaXterm"}
      </div>
      <nav className="workspace-tabs" aria-label="작업 탭">{children}</nav>
      <div className="workspace-drag-gap" onMouseDown={moveWindow} />
      <button className="workspace-mode" aria-label={focused ? "기본 보기" : "집중 보기"}
        onMouseDown={(event) => {
          if (event.button === 0 && document.activeElement?.closest(".terminal-screen")) event.preventDefault();
        }}
        onClick={onToggle}>{focused ? "기본 보기" : "집중 보기"}</button>
      {native && !macTitleBar && windowControls.map((control) => (
        <button className={`window-control ${control.action}`} key={control.action}
          aria-label={control.label} title={control.label} onClick={() => void run(control.action, control.label)}>{control.icon}</button>
      ))}
    </header>
  );
}
