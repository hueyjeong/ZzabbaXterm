import { useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import { LocalAiPanel } from "../ai/LocalAiPanel";
import { createLocalConnectionIpc, type SavedConnections } from "../connections/connectionIpc";
import { LocalConnectionWizard } from "../connections/LocalConnectionWizard";
import type { ConnectionClose, InitialWizardDraft, SshTransport } from "../connections/profiles";
import { EditorTab, type EditorTabActions } from "../editor/EditorTab";
import type { ApprovedEditorFile, ConfirmEditorDiscard, EditorTransport } from "../editor/contract";
import { closeLocalFile, localEditorError, localEditorTransport, openLocalFile } from "../editor/localEditorIpc";
import { SftpPanel, type SftpPanelActions } from "../sftp/SftpPanel";
import { sftpError } from "../sftp/sftpIpc";
import type { SsmTarget } from "../ssm/ssm";
import { StorageTab } from "../storage/StorageTab";
import { TerminalView } from "../terminal/TerminalView";
import type { TerminalStatus } from "../terminal/session";
import { LocalTree } from "./LocalTree";
import { Splitter } from "./Splitter";
import { macTitleBar, TitleBar } from "./TitleBar";
import "./workspace.css";

type CloseStatus = "closing" | "unconfirmed";
type Split = { left: number; right: number };
type Tab = { id: number; label: string } & (
  | { kind: "terminal"; targetId?: string; ssm?: SsmTarget; ssh?: SshTransport["kind"]; status: TerminalStatus; sessionId?: string; sftpSessionId?: string; closeStatus?: CloseStatus }
  | { kind: "editor"; file: ApprovedEditorFile; transport: EditorTransport; close: () => Promise<void>; source: EditorSource }
);
type StorageTarget = { targetId: string; label: string; closeStatus?: CloseStatus };
type ProfileLoad = { kind: "loading" | "ready"; profiles?: SavedConnections }
  | { kind: "error"; profiles?: SavedConnections; error: string };
const connections = createLocalConnectionIpc();
type EditorSource = "local" | "sftp" | "s3";
async function confirmEditorDiscard(source: EditorSource, { label, reason }: Parameters<ConfirmEditorDiscard>[0]) {
  return window.confirm(reason === "reload"
    ? `${label}: ${source === "sftp" ? "서버" : source === "s3" ? "S3" : "디스크"}에서 다시 읽을까요? 현재 편집 내용은 사라집니다.`
    : `${label}: 미저장 편집 내용을 버리고 닫을까요?`);
}
type WizardRequest = { draftId: string; initialDraft: InitialWizardDraft };

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
  const [split, setSplit] = useState<Split | null>(null);
  const liveSplit = useRef<Split | null>(null);
  const [splitPosition, setSplitPosition] = useState(50);
  const paneId = useId();
  const [showFiles, setShowFiles] = useState(false);
  const [pinnedStorage, setPinnedStorage] = useState("");
  const [storageTargets, setStorageTargets] = useState<StorageTarget[]>([]);
  const storageClosers = useRef(new Map<string, { confirmClose(): Promise<void> }>());
  const closingStorage = useRef(new Set<string>());
  const sftpPanels = useRef(new Map<string, SftpPanelActions>());
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
  const [windowError, setWindowError] = useState("");
  const [loadVersion, setLoadVersion] = useState(0);
  const [profileLoad, setProfileLoad] = useState<ProfileLoad>({ kind: "loading" });
  const [hostRegistration, setHostRegistration] = useState<{ kind: "status" | "alert"; message: string }>();
  const [wizard, setWizard] = useState<WizardRequest>();
  const [treeWidth, setTreeWidth] = useState(280);
  const [aiWidth, setAiWidth] = useState(360);
  const [bodyWidth, setBodyWidth] = useState(1200);
  const body = useRef<HTMLDivElement>(null);
  const activeButton = useRef<HTMLButtonElement>(null);
  const sideLimit = Math.min(640, Math.floor(bodyWidth * 0.32));
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

  async function registerSavedSshHost(targetId: string) {
    setHostRegistration({ kind: "status", message: "SSH 호스트 키 등록 중" });
    try {
      await connections.registerSavedSshHost(targetId);
      setHostRegistration({ kind: "status", message: "SSH 호스트 키 등록 완료" });
    } catch (error) {
      setHostRegistration({ kind: "alert", message: `SSH 호스트 키 등록 실패: ${error instanceof Error ? error.message : String(error)}` });
    }
  }

  useEffect(() => {
    window.addEventListener("focus", reloadProfiles);
    return () => window.removeEventListener("focus", reloadProfiles);
  }, []);

  function openWizard() {
    if (wizard) return;
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

  function retestStorageTarget(id: string) {
    if (wizard) return;
    const profiles = profileLoad.profiles;
    const target = profiles?.targets.find(({ profile }) => profile.id === id);
    const group = profiles?.groups.find(({ profile }) => profile.id === target?.profile.groupId);
    if (!target || !group || (target.profile.defaultMethod !== "ssm" && target.profile.defaultMethod !== "s3")) return;
    setWizard({
      draftId: crypto.randomUUID(), initialDraft: {
        version: 1, groupRevision: group.revision, targetRevision: target.revision,
        group: group.profile, target: target.profile,
      }
    });
  }

  function closeWizard(result: ConnectionClose) {
    setWizard(undefined);
    if (result.kind === "saved") reloadProfiles();
  }

  useLayoutEffect(() => {
    const element = body.current!;
    const measure = () => setBodyWidth(element.clientWidth);
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
    if (target?.defaultMethod === "s3") {
      openStorageTarget(target.id, target.displayName);
      return;
    }
    const id = nextTab.current++;
    setTabs((current) => [...current, {
      id, label: target?.displayName ?? `로컬 ${id}`, kind: "terminal", targetId: target?.id,
      ssm: target?.defaultMethod === "ssm" ? target.ssm : undefined,
      ssh: target?.ssh?.transport.kind, status: { kind: "opening" },
    }]);
    selectTab(id);
  }

  function openStorageTarget(targetId: string, label: string) {
    setStorageTargets((current) => current.some((target) => target.targetId === targetId)
      ? current : [...current, { targetId, label }]);
    setPinnedStorage(targetId);
    setShowFiles(true);
  }

  async function closeStorageTarget(targetId: string) {
    if (closingStorage.current.has(targetId)) return;
    closingStorage.current.add(targetId);
    setStorageTargets(current => current.map(target => target.targetId === targetId
      ? { ...target, closeStatus: "closing" } : target));
    try {
      const session = storageClosers.current.get(targetId);
      if (!session) throw new Error("세션 종료 상태를 확인할 수 없습니다");
      await session.confirmClose();
      setStorageTargets(current => current.filter(target => target.targetId !== targetId));
      setPinnedStorage(current => current === targetId ? "" : current);
    } catch {
      setStorageTargets(current => current.map(target => target.targetId === targetId
        ? { ...target, closeStatus: "unconfirmed" } : target));
    } finally {
      closingStorage.current.delete(targetId);
    }
  }

  function openEditorTab(file: ApprovedEditorFile, transport: EditorTransport, close: () => Promise<void>, source: EditorSource) {
    const id = nextTab.current++;
    setTabs((current) => [...current, { id, label: file.label, kind: "editor", file, transport, close, source }]);
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
      openEditorTab(file, localEditorTransport, () => closeLocalFile(file.handle), "local");
    } catch (error) {
      setFileError(`파일 열기 실패: ${localEditorError(error).message}`);
    } finally {
      openingFile.current = false;
      setFilePickerBusy(false);
    }
  }

  function selectTab(id: number, requestFocus = true) {
    if (id !== activeId.current) {
      editorActions.current.get(activeId.current)?.setActive(false);
      activeId.current = id;
      editorActions.current.get(id)?.setActive(true);
    }
    setActive(id);
    setFocusTab(requestFocus ? id : null);
  }

  function updateSplit(next: Split | null) {
    liveSplit.current = next;
    setSplit(next);
  }

  async function closeTab(id: number) {
    const tab = liveTabs.current.find(item => item.id === id);
    if (!tab || closingTabs.current.has(id)) return;
    if (tab.kind === "terminal" && (tab.ssm || tab.ssh)) {
      closingTabs.current.add(id);
      setTabs(current => current.map(item => item.id === id && item.kind === "terminal" ? { ...item, closeStatus: "closing" } : item));
      try {
        if (tab.sftpSessionId) {
          const sftp = sftpPanels.current.get(tab.sftpSessionId);
          try {
            await sftp?.confirmClose();
            if (sftp) setTabs(current => current.map(item => item.id === id && item.kind === "terminal"
              ? { ...item, sftpSessionId: undefined } : item));
          } catch {
            // SFTP 분리가 실패해도 부모 SSH의 종료로 해당 세션을 정리한다.
          }
        }
        const session = sessionClosers.current.get(id);
        if (!session) throw new Error("세션 종료 상태를 확인할 수 없습니다");
        await session.confirmClose();
      } catch {
        setTabs(current => current.map(item => item.id === id && item.kind === "terminal" ? { ...item, closeStatus: "unconfirmed" } : item));
        return;
      } finally {
        closingTabs.current.delete(id);
      }
    }
    const current = liveTabs.current;
    const index = current.findIndex(item => item.id === id);
    if (index < 0) return;
    const split = liveSplit.current;
    const partner = split?.left === id ? split.right : split?.right === id ? split.left : undefined;
    if (partner !== undefined) updateSplit(null);
    const remaining = current.filter((tab) => tab.id !== id);
    if (id === activeId.current) selectTab(partner ?? remaining[index]?.id ?? remaining[index - 1]?.id ?? 0);
    liveTabs.current = remaining;
    setTabs((current) => current.filter((tab) => tab.id !== id));
  }

  function updateStatus(id: number, status: TerminalStatus, sessionId?: string) {
    setTabs((current) => current.map((tab) => tab.id === id && tab.kind === "terminal"
      ? { ...tab, status, sessionId: sessionId ?? tab.sessionId,
        sftpSessionId: status.kind === "running" && (sessionId ?? tab.sessionId) === tab.sftpSessionId ? tab.sftpSessionId : undefined }
      : tab));
  }

  const current = tabs.find((tab) => tab.id === active);
  const visibleSplit = split && (active === split.left || active === split.right) ? split : null;
  const splitCandidates = !visibleSplit && current?.kind === "terminal" && !current.closeStatus &&
    (current.status.kind === "running" || current.status.kind === "opening" || current.status.kind === "connecting")
    ? tabs.filter((tab) => tab.kind === "terminal" && tab.id !== active && !tab.closeStatus &&
      (tab.status.kind === "running" || tab.status.kind === "opening" || tab.status.kind === "connecting"))
    : [];
  const activeSftp = showFiles && !focused && !pinnedStorage && current?.kind === "terminal" && current.ssh === "direct" &&
    !current.closeStatus && current.status.kind === "running" ? current.sessionId : undefined;
  const attachedSftp = current?.kind === "terminal" ? current.sftpSessionId : undefined;
  useLayoutEffect(() => {
    if (!activeSftp || attachedSftp === activeSftp) return;
    setTabs(current => current.map(tab => tab.id === active && tab.kind === "terminal" &&
      tab.status.kind === "running" && tab.sessionId === activeSftp && !tab.closeStatus
      ? { ...tab, sftpSessionId: activeSftp } : tab));
  }, [active, activeSftp, attachedSftp]);
  const selectedStorage = storageTargets.find(target => target.targetId === pinnedStorage);
  const savedStorageTargets = profileLoad.profiles?.targets.filter(({ profile }) =>
    (profile.defaultMethod === "s3" || profile.defaultMethod === "ssm") &&
    !storageTargets.some(target => target.targetId === profile.id)) ?? [];
  const layout = {
    "--tree-width": `${left}px`, "--ai-width": `${right}px`,
    "--center-left": `${splitPosition}fr`, "--center-right": `${100 - splitPosition}fr`,
  } as CSSProperties;

  return (
    <main className={`workspace${macTitleBar ? " workspace-mac" : ""}${focused ? " workspace-focused" : ""}`} style={layout}
      onFocusCapture={(event) => { if (!event.target.closest(".terminal-screen, .monaco-editor")) setFocusTab(null); }}>
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
      </TitleBar>
      <div className="workspace-body" ref={body}>
        <aside className="workspace-tree" hidden={focused}>
          <button className="connection-add" type="button" aria-label="연결 추가" onClick={openWizard}>+</button>
          <button className="local-file-open" type="button" disabled={filePickerBusy} onClick={() => void openEditor()}>파일 열기</button>
          {profileLoad.kind === "ready" && <button className="local-file-open" type="button" onClick={reloadProfiles}>새로고침</button>}
          {profileLoad.kind === "loading" && <div className="connection-status" role="status">저장된 연결 불러오는 중</div>}
          {profileLoad.kind === "ready" && profileLoad.profiles?.targets.length === 0 &&
            <div className="connection-status" role="status">저장된 연결 없음</div>}
          {profileLoad.kind === "error" && <div className="connection-status">
            <div role="alert">연결 불러오기 실패: {profileLoad.error}</div>
            <button onClick={reloadProfiles}>다시 불러오기</button>
          </div>}
          <LocalTree profiles={profileLoad.profiles} onOpen={openTab} onRegisterSshHost={(targetId) => void registerSavedSshHost(targetId)}
            onRetestS3={retestStorageTarget} />
        </aside>
        <div className={`workspace-center${visibleSplit ? " workspace-center-split" : ""}`}>
          <div className="workspace-center-actions" hidden={!visibleSplit && !splitCandidates.length}>
            {visibleSplit ? <button type="button" onClick={() => {
              updateSplit(null);
              selectTab(active);
            }}>분할 닫기</button> : splitCandidates.length > 0 && <label>
              오른쪽 터미널 <select value="" onChange={(event) => {
                const id = Number(event.target.value);
                if (!splitCandidates.some((tab) => tab.id === id)) return;
                updateSplit({ left: active, right: id });
                selectTab(active);
              }}>
                <option value="">선택</option>
                {splitCandidates.map(({ id, label }) => <option key={id} value={id}>{label}</option>)}
              </select>
            </label>}
          </div>
          {tabs.map(tab => {
            const { id, label } = tab;
            const bindClose = (handle: { confirmClose(): Promise<void> } | null) => {
              if (handle) sessionClosers.current.set(id, handle);
              else sessionClosers.current.delete(id);
            };
            const pane = id === visibleSplit?.left ? "left" : id === visibleSplit?.right ? "right" : undefined;
            const visible = id === active || pane !== undefined;
            return <div className="workspace-session" key={id} id={pane && `${paneId}-${pane}`} data-pane={pane}
              hidden={!visible} inert={!visible}
              onFocusCapture={(event) => {
                if (pane && id !== activeId.current && event.target.closest(".terminal-screen")) selectTab(id, false);
              }}>
              {tab.kind === "terminal" ? (
                <TerminalView active={visible} focusRequested={id === focusTab} label={label} targetId={tab.targetId}
                  remote={!!tab.ssm} ssh={!!tab.ssh} closing={!!tab.closeStatus} closeRef={tab.ssm || tab.ssh ? bindClose : undefined}
                  onStatus={(status, sessionId) => updateStatus(id, status, sessionId)} />
              ) : (
                <EditorTab file={tab.file} active={id === active} transport={tab.transport}
                  confirmDiscard={(request) => confirmEditorDiscard(tab.source, request)}
                  focusRequested={id === focusTab}
                  ref={(actions) => {
                    if (actions) editorActions.current.set(id, actions);
                    else editorActions.current.delete(id);
                  }}
                  onClose={(handle) => {
                    if (handle !== tab.file.handle) return;
                    void closeTab(id);
                    void tab.close().catch((error: unknown) => {
                      const message = tab.source === "local" ? localEditorError(error).message : tab.source === "sftp" ? sftpError(error).message
                        : error instanceof Error ? error.message : String(error);
                      setFileError(`파일 닫기 실패: ${message}`);
                    });
                  }} />
              )}
            </div>;
          })}
          {visibleSplit && <Splitter edge="center" label="중앙 터미널 분할" controls={`${paneId}-left`}
            value={splitPosition} min={20} max={80} onChange={setSplitPosition} />}
        </div>
        <aside className="workspace-ai" hidden={focused} aria-label="오른쪽 도구">
          <LocalAiPanel profilesVersion={loadVersion} topTabs={{
            item: {
              id: "files", label: "파일",
              content: <>
                <div className="workspace-file-connections">
                  <select aria-label="파일 연결" value={pinnedStorage} onChange={event => {
                    const targetId = event.target.value;
                    const saved = savedStorageTargets.find(({ profile }) => profile.id === targetId);
                    if (saved) openStorageTarget(targetId, saved.profile.displayName);
                    else setPinnedStorage(targetId);
                  }}>
                    <option value="">활성 SSH 따라가기</option>
                    {storageTargets.length > 0 && <optgroup label="열린 S3">
                      {storageTargets.map(({ targetId, label }) => <option key={targetId} value={targetId}>{label}</option>)}
                    </optgroup>}
                    {savedStorageTargets.length > 0 && <optgroup label="저장된 S3 연결 열기">
                      {savedStorageTargets.map(({ profile }) => <option key={profile.id} value={profile.id}>{profile.displayName}</option>)}
                    </optgroup>}
                  </select>
                  {selectedStorage && <button type="button" disabled={selectedStorage.closeStatus === "closing"}
                    aria-label={`${selectedStorage.label} ${selectedStorage.closeStatus === "unconfirmed" ? "종료 다시 확인" : "닫기"}`}
                    onClick={() => void closeStorageTarget(selectedStorage.targetId)}>×</button>}
                </div>
                {showFiles && !pinnedStorage && !activeSftp && <p className="workspace-file-status" role="status">활성 SSH 연결 없음</p>}
                {tabs.map((tab) => {
                  if (tab.kind !== "terminal" || !tab.sftpSessionId) return null;
                  const sessionId = tab.sftpSessionId;
                  const visible = tab.id === active && activeSftp === sessionId;
                  return <div className="workspace-sftp" key={sessionId} hidden={!visible} inert={!visible}>
                    <SftpPanel sshSessionId={sessionId} closing={!!tab.closeStatus}
                      closeRef={(actions) => {
                        if (actions) sftpPanels.current.set(sessionId, actions);
                        else sftpPanels.current.delete(sessionId);
                      }}
                      onEditFile={(file, transport, close) => openEditorTab(file, transport, close, "sftp")} />
                  </div>;
                })}
                {storageTargets.map(({ targetId, closeStatus }) => {
                  const visible = showFiles && !focused && targetId === pinnedStorage;
                  return <div className="workspace-storage" key={targetId} hidden={!visible} inert={!visible}>
                    {closeStatus && <p className="workspace-file-status" role={closeStatus === "closing" ? "status" : "alert"}>
                      {closeStatus === "closing" ? "종료 중" : "종료 확인 불가"}
                    </p>}
                    <div inert={!!closeStatus}>
                      <StorageTab profiles={profileLoad.profiles} targetId={targetId} active={visible} closing={!!closeStatus}
                        closeRef={(actions) => {
                          if (actions) storageClosers.current.set(targetId, actions);
                          else storageClosers.current.delete(targetId);
                        }}
                        onRetest={retestStorageTarget}
                        onEditFile={({ file, transport, close }) => openEditorTab(file, transport, close, "s3")} />
                    </div>
                  </div>;
                })}
              </>,
            },
            selectedId: showFiles ? "files" : null,
            onSelect: id => setShowFiles(id === "files"),
          }} />
        </aside>
        {!focused && <>
          <Splitter edge="left" label="트리 너비" value={left} preferred={treeWidth} min={Math.min(160, sideLimit)} max={sideLimit} onChange={setTreeWidth} />
          <Splitter edge="right" label="AI 열 너비" value={right} preferred={aiWidth} min={Math.min(200, sideLimit)} max={sideLimit} onChange={setAiWidth} />
        </>}
      </div>
      <footer className="workspace-status">
        <span role="status">{current?.kind === "terminal"
          ? [current.targetId ? current.label : "내 PC", current.ssm ?
            `SSM · ${current.ssm.profile} / ${current.ssm.region} / ${current.ssm.instanceId}` : current.ssh ? "SSH" : "로컬", current.sessionId,
          !current.closeStatus && terminalStatus(current.status)].filter(Boolean).join(" · ")
          : current ? `${current.source === "local" ? "내 PC · 로컬 파일" : current.source === "sftp" ? "SFTP" : "S3"} · ${current.label}` : "연결 없음"}</span>
        {windowError && <span role="alert">{windowError}</span>}
        {fileError && <span role="alert">{fileError}</span>}
        {hostRegistration && <span role={hostRegistration.kind}>{hostRegistration.message}</span>}
      </footer>
      {wizard && <LocalConnectionWizard {...wizard} groups={profileLoad.profiles?.groups ?? []}
        verifySshHost={connections.verifySshHost}
        test={connections.test} cancelTest={connections.cancelTest} save={connections.save} onClose={closeWizard} />}
    </main>
  );
}
