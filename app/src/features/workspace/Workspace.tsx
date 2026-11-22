import { useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import { TerminalView } from "../terminal/TerminalView";
import type { TerminalStatus } from "../terminal/session";
import { LocalTree } from "./LocalTree";
import { Splitter } from "./Splitter";
import { macTitleBar, TitleBar } from "./TitleBar";
import "./workspace.css";

type Tab = { id: number; status: TerminalStatus; sessionId?: string };

function terminalStatus(status: TerminalStatus) {
  switch (status.kind) {
    case "opening": return "여는 중";
    case "running": return "연결됨";
    case "exited": return `종료 코드 ${status.value}`;
    case "disconnected": return `단절: ${status.value}`;
    case "failed": return `열기 실패: ${status.value}`;
  }
}

export function Workspace() {
  const nextTab = useRef(2);
  const [tabs, setTabs] = useState<Tab[]>([{ id: 1, status: { kind: "opening" } }]);
  const [active, setActive] = useState(1);
  const [focused, setFocused] = useState(false);
  const [draft, setDraft] = useState("");
  const [windowError, setWindowError] = useState("");
  const [treeWidth, setTreeWidth] = useState(280);
  const [aiWidth, setAiWidth] = useState(360);
  const [chatHeight, setChatHeight] = useState(50);
  const [area, setArea] = useState({ width: 1200, height: 600 });
  const body = useRef<HTMLDivElement>(null);
  const activeButton = useRef<HTMLButtonElement>(null);

  useLayoutEffect(() => {
    const element = body.current!;
    const measure = () => {
      const { clientWidth: width, clientHeight: height } = element;
      setArea((previous) => previous.width === width && previous.height === height ? previous : { width, height });
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useLayoutEffect(() => {
    const button = activeButton.current;
    const list = button?.closest("nav");
    if (!button || !list) return;
    const { left, right } = button.parentElement!.getBoundingClientRect();
    const bounds = list.getBoundingClientRect();
    if (left < bounds.left) list.scrollLeft += left - bounds.left;
    else if (right > bounds.right) list.scrollLeft += right - bounds.right;
  }, [active]);

  function openTab() {
    const id = nextTab.current++;
    setTabs((current) => [...current, { id, status: { kind: "opening" } }]);
    setActive(id);
  }

  function closeTab(id: number) {
    const index = tabs.findIndex((tab) => tab.id === id);
    if (id === active) setActive(tabs[index + 1]?.id ?? tabs[index - 1]?.id ?? 0);
    setTabs((current) => current.filter((tab) => tab.id !== id));
  }

  function updateStatus(id: number, status: TerminalStatus, sessionId?: string) {
    setTabs((current) => current.some((tab) => tab.id === id) ? current.map((tab) => tab.id === id
      ? { id, status, sessionId: sessionId ?? tab.sessionId }
      : tab) : current);
  }

  const sideLimit = Math.floor(area.width * 0.32);
  const left = Math.min(treeWidth, sideLimit);
  const right = Math.min(aiWidth, sideLimit);
  const current = tabs.find((tab) => tab.id === active);
  const layout = { "--tree-width": `${left}px`, "--ai-width": `${right}px`, "--chat-height": `${chatHeight}%` } as CSSProperties;

  return (
    <main className={`workspace${macTitleBar ? " workspace-mac" : ""}${focused ? " workspace-focused" : ""}`} style={layout}>
      <TitleBar focused={focused} onToggle={() => setFocused(!focused)} onWindowError={setWindowError}>
        {tabs.map(({ id }) => (
          <div className="workspace-tab" key={id}>
            <button ref={id === active ? activeButton : undefined} aria-pressed={id === active} onClick={() => setActive(id)}>로컬 {id}</button>
            <button aria-label={`로컬 ${id} 닫기`} onClick={() => closeTab(id)}>×</button>
          </div>
        ))}
      </TitleBar>
      <div className="workspace-body" ref={body}>
        <aside className="workspace-tree" hidden={focused}>
          <LocalTree onOpen={openTab} />
        </aside>
        <div className="workspace-center">
          {tabs.map(({ id }) => (
            <div className="workspace-session" key={id} hidden={id !== active} inert={id !== active}>
              <TerminalView active={id === active} label={`로컬 ${id}`} onStatus={(status, sessionId) => updateStatus(id, status, sessionId)} />
            </div>
          ))}
        </div>
        <aside className="workspace-ai" hidden={focused} aria-label="AI">
          <section className="workspace-chat">
            <h2>AI 채팅</h2>
            <textarea aria-label="메시지 입력" placeholder="메시지 입력" value={draft} onChange={(event) => setDraft(event.target.value)} />
          </section>
          <section className="workspace-ai-terminal">
            <h2>AI 터미널 / 전용 셸</h2>
            <span>대기</span>
          </section>
          <Splitter edge="chat" label="AI 채팅 높이" value={chatHeight} preferred={chatHeight}
            min={20} max={80} units={100 / Math.max(1, area.height)} onChange={setChatHeight} />
        </aside>
        {!focused && <>
          <Splitter edge="left" label="트리 너비" value={left} preferred={treeWidth} min={160} max={640} units={1} onChange={setTreeWidth} />
          <Splitter edge="right" label="AI 열 너비" value={right} preferred={aiWidth} min={200} max={640} units={1} onChange={setAiWidth} />
        </>}
      </div>
      <footer className="workspace-status">
        <span role="status">{current ? ["내 PC", "로컬", current.sessionId, terminalStatus(current.status)].filter(Boolean).join(" · ") : "연결 없음"}</span>
        {windowError && <span role="alert">{windowError}</span>}
        <span>AI 전용 셸 · 대기</span>
      </footer>
    </main>
  );
}
