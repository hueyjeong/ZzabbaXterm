import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import { createLocalConnectionIpc, type SavedConnections } from "../connections/connectionIpc";
import { LocalConnectionWizard } from "../connections/LocalConnectionWizard";
import type { ConnectionClose, LocalProfileDraft } from "../connections/profiles";
import { EditorTab, type EditorTabActions } from "../editor/EditorTab";
import type { ApprovedEditorFile, ConfirmEditorDiscard } from "../editor/contract";
import { closeLocalFile, localEditorError, localEditorTransport, openLocalFile } from "../editor/localEditorIpc";
import type { SsmTarget } from "../ssm/ssm";
import { StorageTab } from "../storage/StorageTab";
import { TerminalView } from "../terminal/TerminalView";
import type { TerminalStatus } from "../terminal/session";
import { LocalTree } from "./LocalTree";
import { Splitter } from "./Splitter";
import { macTitleBar, TitleBar } from "./TitleBar";
import "./workspace.css";

type CloseStatus = "closing" | "unconfirmed";
type Tab = { id: number; label: string } & (
  | { kind: "terminal"; targetId?: string; ssm?: SsmTarget; status: TerminalStatus; sessionId?: string; closeStatus?: CloseStatus }
  | { kind: "storage"; closeStatus?: CloseStatus }
  | { kind: "editor"; file: ApprovedEditorFile }
);
type ProfileLoad = { kind: "loading" | "ready"; profiles?: SavedConnections }
  | { kind: "error"; profiles?: SavedConnections; error: string };
const connections = createLocalConnectionIpc();
const confirmEditorDiscard: ConfirmEditorDiscard = async ({ label, reason }) => window.confirm(
  reason === "reload" ? `${label}: 디스크에서 다시 읽을까요? 현재 편집 내용은 사라집니다.` : `${label}: 미저장 편집 내용을 버리고 닫을까요?`,
);

function terminalStatus(status: TerminalStatus) {
  switch (status.kind) {
    case "opening": return "여는 중";
    case "connecting": return "연결 중";
    case "running": return "연결됨";
    case "exited": return `종료 코드 ${status.value}`;
    case "disconnected": return `단절: ${status.value}`;
    case "failed": return `열기 실패: ${status.value}`;
  }
}

export function Workspace() {
  const nextTab = useRef(2);
  const [tabs, setTabs] = useState<Tab[]>([{ id: 1, label: "로컬 1", kind: "terminal", status: { kind: "opening" } }]);
  const [active, setActive] = useState(1);
  const activeId = useRef(1);
  const editorActions = useRef(new Map<number, EditorTabActions>());
  const openingFile = useRef(false);
  const [filePickerBusy, setFilePickerBusy] = useState(false);
  const [fileError, setFileError] = useState("");
  const [focusTab, setFocusTab] = useState<number | null>(1);
  const liveTabs = useRef(tabs);
  const closingTabs = useRef(new Set<number>());
  const sessionClosers = useRef(new Map<number, { confirmClose(): Promise<void> }>());
  useLayoutEffect(() => { liveTabs.current = tabs; }, [tabs]);
  const [focused, setFocused] = useState(false);
  const [draft, setDraft] = useState("");
  const [windowError, setWindowError] = useState("");
  const [loadVersion, setLoadVersion] = useState(0);
  const [profileLoad, setProfileLoad] = useState<ProfileLoad>({ kind: "loading" });
  const [wizard, setWizard] = useState<{ draftId: string; initialDraft: LocalProfileDraft }>();
  const [treeWidth, setTreeWidth] = useState(280);
  const [aiWidth, setAiWidth] = useState(360);
  const [chatHeight, setChatHeight] = useState(50);
  const [area, setArea] = useState({ width: 1200, height: 600 });
  const body = useRef<HTMLDivElement>(null);
  const activeButton = useRef<HTMLButtonElement>(null);
  const sideLimit = Math.min(640, Math.floor(area.width * 0.32));
  const left = Math.min(treeWidth, sideLimit);
  const right = Math.min(aiWidth, sideLimit);

  useEffect(() => {
    let active = true;
    void connections.load().then((profiles) => {
      if (active) setProfileLoad({ kind: "ready", profiles });
    }, (error: unknown) => {
      if (active) setProfileLoad(({ profiles }) => ({ kind: "error", profiles, error: String(error) }));
    });
    return () => { active = false; };
  }, [loadVersion]);

  function reloadProfiles() {
    setProfileLoad(({ profiles }) => ({ kind: "loading", profiles }));
    setLoadVersion((version) => version + 1);
  }

  function openWizard() {
    const groupId = crypto.randomUUID();
    setWizard({
      draftId: crypto.randomUUID(), initialDraft: {
        version: 1, groupRevision: null, targetRevision: null,
        group: { id: groupId, displayName: "" },
        target: {
          id: crypto.randomUUID(), groupId, displayName: "", defaultMethod: "local",
          local: { shell: /Windows/.test(navigator.userAgent) ? "powerShell" : "bash", startDirectory: "" },
        },
      }
    });
  }

  function closeWizard(result: ConnectionClose) {
    setWizard(undefined);
    if (result.kind === "saved") reloadProfiles();
  }

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
    const reveal = () => {
      const bounds = list.getBoundingClientRect();
      const tab = button.parentElement!.getBoundingClientRect();
      const selected = tab.width <= bounds.width ? tab : button.getBoundingClientRect();
      if (selected.left < bounds.left) list.scrollLeft += selected.left - bounds.left;
      else if (selected.right > bounds.right) list.scrollLeft += selected.right - bounds.right;
    };
    reveal();
    const observer = new ResizeObserver(reveal);
    observer.observe(list);
    return () => observer.disconnect();
  }, [active, left, focused]);

  function openTab(target?: SavedConnections["targets"][number]["profile"]) {
    const id = nextTab.current++;
    setTabs((current) => [...current, {
      id, label: target?.displayName ?? `로컬 ${id}`, kind: "terminal", targetId: target?.id,
      ssm: target?.defaultMethod === "ssm" ? target.ssm : undefined, status: { kind: "opening" },
    }]);
    selectTab(id);
  }

  function openStorageTab() {
    const id = nextTab.current++;
    setTabs(current => [...current, { kind: "storage", id, label: `S3 ${id}` }]);
    selectTab(id);
  }

  async function openEditor() {
    if (openingFile.current) return;
    openingFile.current = true;
    setFilePickerBusy(true);
    setFileError("");
    try {
      const file = await openLocalFile();
      if (!file) return;
      const id = nextTab.current++;
      setTabs((current) => [...current, { id, label: file.label, kind: "editor", file }]);
      selectTab(id);
    } catch (error) {
      setFileError(`파일 열기 실패: ${localEditorError(error).message}`);
    } finally {
      openingFile.current = false;
      setFilePickerBusy(false);
    }
  }

  function selectTab(id: number) {
    if (id !== activeId.current) {
      editorActions.current.get(activeId.current)?.setActive(false);
      activeId.current = id;
      editorActions.current.get(id)?.setActive(true);
    }
    setActive(id);
    setFocusTab(id);
  }

  async function closeTab(id: number) {
    const tab = liveTabs.current.find(item => item.id === id);
    if (!tab || closingTabs.current.has(id)) return;
    if (tab.kind === "storage" || (tab.kind === "terminal" && tab.ssm)) {
      closingTabs.current.add(id);
      setTabs(current => current.map(item => item.id === id && item.kind !== "editor" ? { ...item, closeStatus: "closing" } : item));
      try {
        const session = sessionClosers.current.get(id);
        if (!session) throw new Error("세션 종료 상태를 확인할 수 없습니다");
        await session.confirmClose();
      } catch {
        setTabs(current => current.map(item => item.id === id && item.kind !== "editor" ? { ...item, closeStatus: "unconfirmed" } : item));
        return;
      } finally {
        closingTabs.current.delete(id);
      }
    }
    const current = liveTabs.current;
    const index = current.findIndex(item => item.id === id);
    if (index < 0) return;
    if (id === activeId.current) selectTab(current[index + 1]?.id ?? current[index - 1]?.id ?? 0);
    setTabs((current) => current.filter((tab) => tab.id !== id));
  }

  function updateStatus(id: number, status: TerminalStatus, sessionId?: string) {
    setTabs((current) => current.map((tab) => tab.id === id && tab.kind === "terminal"
      ? { ...tab, status, sessionId: sessionId ?? tab.sessionId }
      : tab));
  }

  const current = tabs.find((tab) => tab.id === active);
  const layout = { "--tree-width": `${left}px`, "--ai-width": `${right}px`, "--chat-height": `${chatHeight}%` } as CSSProperties;

  return (
    <main className={`workspace${macTitleBar ? " workspace-mac" : ""}${focused ? " workspace-focused" : ""}`} style={layout}
      onFocusCapture={(event) => { if (!event.target.closest(".terminal-screen")) setFocusTab(null); }}>
      <TitleBar focused={focused} onToggle={() => setFocused(!focused)} onWindowError={setWindowError}>
        {tabs.map((tab) => {
          const { id, label, kind } = tab;
          const closeStatus = kind === "editor" ? undefined : tab.closeStatus;
          return <div className="workspace-tab" key={id}>
            <button ref={id === active ? activeButton : undefined} aria-pressed={id === active} onClick={() => selectTab(id)}>{label}</button>
            <button aria-label={`${label} ${closeStatus === "unconfirmed" ? "종료 다시 확인" : "닫기"}`}
              disabled={closeStatus === "closing"} onClick={() => {
                if (kind === "editor") void editorActions.current.get(id)?.requestClose();
                else void closeTab(id);
              }}>×</button>
            {closeStatus && <span role={closeStatus === "closing" ? "status" : "alert"}>
              {closeStatus === "closing" ? "종료 중" : "종료 확인 불가"}
            </span>}
          </div>
        })}
        <button type="button" className="storage-add" onClick={openStorageTab}>스토리지 탭 추가</button>
      </TitleBar>
      <div className="workspace-body" ref={body}>
        <aside className="workspace-tree" hidden={focused}>
          <button className="connection-add" type="button" aria-label="연결 추가" onClick={openWizard}>+</button>
          <button className="local-file-open" type="button" disabled={filePickerBusy} onClick={() => void openEditor()}>파일 열기</button>
          {profileLoad.kind === "loading" && <div className="connection-status" role="status">저장된 연결 불러오는 중</div>}
          {profileLoad.kind === "ready" && profileLoad.profiles?.targets.length === 0 &&
            <div className="connection-status" role="status">저장된 연결 없음</div>}
          {profileLoad.kind === "error" && <div className="connection-status">
            <div role="alert">연결 불러오기 실패: {profileLoad.error}</div>
            <button onClick={reloadProfiles}>다시 불러오기</button>
          </div>}
          <LocalTree profiles={profileLoad.profiles} onOpen={openTab} />
        </aside>
        <div className="workspace-center">
          {tabs.map(tab => {
            const { id, label } = tab;
            const bindClose = (handle: { confirmClose(): Promise<void> } | null) => {
              if (handle) sessionClosers.current.set(id, handle);
              else sessionClosers.current.delete(id);
            };
            return <div className="workspace-session" key={id} hidden={id !== active} inert={id !== active || (tab.kind === "storage" && !!tab.closeStatus)}>
              {tab.kind === "storage" ? (
                <StorageTab profiles={profileLoad.profiles} closing={!!tab.closeStatus} closeRef={bindClose} />
              ) : tab.kind === "terminal" ? (
                <TerminalView active={id === active} focusRequested={id === focusTab} label={label} targetId={tab.targetId}
                  remote={!!tab.ssm} closing={!!tab.closeStatus} closeRef={tab.ssm ? bindClose : undefined}
                  onStatus={(status, sessionId) => updateStatus(id, status, sessionId)} />
              ) : (
                <EditorTab file={tab.file} active={id === active} transport={localEditorTransport} confirmDiscard={confirmEditorDiscard}
                  ref={(actions) => {
                    if (actions) editorActions.current.set(id, actions);
                    else editorActions.current.delete(id);
                  }}
                  onClose={(handle) => {
                    if (handle !== tab.file.handle) return;
                    void closeTab(id);
                    void closeLocalFile(handle).catch((error) => setFileError(`파일 닫기 실패: ${localEditorError(error).message}`));
                  }} />
              )}
            </div>;
          })}
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
          <Splitter edge="chat" label="AI 채팅 높이" value={chatHeight}
            min={20} max={80} units={100 / Math.max(1, area.height)} onChange={setChatHeight} />
        </aside>
        {!focused && <>
          <Splitter edge="left" label="트리 너비" value={left} preferred={treeWidth} min={Math.min(160, sideLimit)} max={sideLimit} units={1} onChange={setTreeWidth} />
          <Splitter edge="right" label="AI 열 너비" value={right} preferred={aiWidth} min={Math.min(200, sideLimit)} max={sideLimit} units={1} onChange={setAiWidth} />
        </>}
      </div>
      <footer className="workspace-status">
        <span role="status">{current?.kind === "terminal"
          ? [current.targetId ? current.label : "내 PC", current.ssm ?
            `SSM · ${current.ssm.profile} / ${current.ssm.region} / ${current.ssm.instanceId}` : "로컬", current.sessionId,
          !current.closeStatus && terminalStatus(current.status)].filter(Boolean).join(" · ")
          : current?.kind === "storage" ? `${current.label} · 스토리지`
            : current ? `내 PC · 로컬 파일 · ${current.label}` : "연결 없음"}</span>
        {windowError && <span role="alert">{windowError}</span>}
        {fileError && <span role="alert">{fileError}</span>}
        <span>AI 전용 셸 · 대기</span>
      </footer>
      {wizard && <LocalConnectionWizard {...wizard} groups={profileLoad.profiles?.groups ?? []}
        test={connections.test} cancelTest={connections.cancelTest} save={connections.save} onClose={closeWizard} />}
    </main>
  );
}
