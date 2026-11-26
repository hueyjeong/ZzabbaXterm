import { invoke, isTauri } from "@tauri-apps/api/core";
import { useRef, useState, type MouseEvent, type ReactNode } from "react";
import { ChevronDown, Maximize2, Minimize2, Minus, PanelLeft, PanelRight, Plus, Square, SquareTerminal, X } from "lucide-react";
import { IconButton, Menu, type MenuItem } from "../ui/controls";

const native = isTauri();
export const macTitleBar = /Macintosh/.test(navigator.userAgent);
const windowControls = [
  { action: "minimize", label: "최소화", icon: Minus },
  { action: "toggleMaximize", label: "최대화/복원", icon: Square },
  { action: "close", label: "닫기", icon: X },
] as const;
type WindowAction = (typeof windowControls)[number]["action"] | "drag";

export function TitleBar({ focused, sidebar, aiPanel, newTabItems, onNewTerminal, onToggle, onToggleSidebar, onToggleAi, onWindowError, children }: {
  focused: boolean;
  sidebar: boolean;
  aiPanel: boolean;
  newTabItems: readonly MenuItem[];
  onNewTerminal: () => void;
  onToggle: () => void;
  onToggleSidebar: () => void;
  onToggleAi: () => void;
  onWindowError: (message: string) => void;
  children?: ReactNode;
}) {
  const [menu, setMenu] = useState<{ x: number; y: number }>();
  const menuButton = useRef<HTMLButtonElement>(null);

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
    if (event.target !== event.currentTarget && (event.target as Element).closest("button, input")) return;
    event.preventDefault();
    if (event.detail === 1) void run("drag", "창 이동");
    else if (!macTitleBar) void run("toggleMaximize", "최대화/복원");
  }

  // 터미널·원격 화면의 키보드 포커스를 제목줄 단추 클릭으로 빼앗지 않는다.
  const keepFocus = (event: MouseEvent) => {
    if (event.button === 0 && document.activeElement?.closest(".terminal-screen, .remote-desktop-keyboard")) event.preventDefault();
  };

  return (
    <header className="workspace-titlebar">
      <div className="workspace-handle" onMouseDown={moveWindow}>
        {!focused && <><span className="workspace-logo" aria-hidden="true"><SquareTerminal /></span><span className="workspace-name">ZXT</span></>}
      </div>
      <div className="workspace-new-tab" role="group" aria-label="새 탭">
        <IconButton icon={Plus} label="새 로컬 터미널" onMouseDown={keepFocus} onClick={onNewTerminal} />
        <IconButton icon={ChevronDown} label="새 탭 종류" ref={menuButton} aria-haspopup="menu" aria-expanded={!!menu}
          className="workspace-new-tab-more" onMouseDown={keepFocus} onClick={(event) => {
            const rect = event.currentTarget.getBoundingClientRect();
            setMenu(current => current ? undefined : { x: rect.left - 28, y: rect.bottom + 4 });
          }} />
      </div>
      <div className="workspace-drag-gap" onMouseDown={moveWindow} />
      {children}
      <div className="workspace-drag-gap" onMouseDown={moveWindow} />
      <div className="workspace-view-actions" role="group" aria-label="보기">
        {!focused && <>
          <IconButton icon={PanelLeft} label="사이드바" aria-pressed={sidebar} onMouseDown={keepFocus} onClick={onToggleSidebar} />
          <IconButton icon={PanelRight} label="AI 패널" aria-pressed={aiPanel} onMouseDown={keepFocus} onClick={onToggleAi} />
        </>}
        <IconButton icon={focused ? Minimize2 : Maximize2} label={focused ? "기본 보기" : "집중 보기"} className="workspace-mode"
          onMouseDown={keepFocus} onClick={onToggle} />
      </div>
      {native && !macTitleBar && <div className="workspace-window-controls">
        {windowControls.map(({ action, label, icon: Icon }) => (
          <button type="button" className={`window-control ${action}`} key={action} aria-label={label} title={label}
            onClick={() => void run(action, label)}><Icon aria-hidden="true" /></button>
        ))}
      </div>}
      {menu && <Menu x={menu.x} y={menu.y} label="새 탭 종류" items={newTabItems} restoreFocus={menuButton.current}
        onClose={() => setMenu(undefined)} />}
    </header>
  );
}
