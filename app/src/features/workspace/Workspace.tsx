import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties, type DragEvent, type MouseEvent, type SyntheticEvent } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ChevronsDownUp, Columns2, Database, FileText, FolderTree, Maximize2, Monitor, MonitorSmartphone, PanelLeft, PanelRight, Plus, RefreshCw, Search, Server, SquareTerminal } from "lucide-react";
import { LocalAiPanel } from "../ai/LocalAiPanel";
import { createLocalConnectionIpc, type SavedConnections } from "../connections/connectionIpc";
import { LocalConnectionWizard } from "../connections/LocalConnectionWizard";
import type { ConnectionClose, InitialWizardDraft, SshTransport } from "../connections/profiles";
import { subscribeLocalCliOpen } from "../editor/cliOpen";
import { EditorTab, type EditorTabActions } from "../editor/EditorTab";
import { ElevationDialog } from "../elevation/ElevationDialog";
import { elevationActions, type ElevationRequest } from "../elevation/elevation";
import type { ApprovedEditorFile, ConfirmEditorDiscard, EditorTransport } from "../editor/contract";
import { closeLocalFile, localEditorError, localEditorTransport, openLocalFile } from "../editor/localEditorIpc";
import { subscribeRemoteNanoOpen, type RemoteNanoFile } from "../editor/nanoOpen";
import { RdpTab, type RdpTabActions } from "../rdp/RdpTab";
import { DesktopRouteError, openDesktopProfile, type DesktopPreset, type DesktopProfile } from "../remoteDesktop/desktopProfile";
import { SftpPanel, type SftpPanelActions } from "../sftp/SftpPanel";
import { sftpError } from "../sftp/sftpIpc";
import type { SsmTarget } from "../ssm/ssm";
import { SsoLoginAction, ssoExpired, useSsoLogin } from "../ssm/ssoLogin";
import { CompatibleStorageTab } from "../storage/CompatibleStorageTab";
import { StorageTab } from "../storage/StorageTab";
import { TerminalView } from "../terminal/TerminalView";
import type { TerminalStatus } from "../terminal/session";
import { TunnelTab } from "../tunnels/TunnelTab";
import { tunnelIpc, type DesktopTunnelSource } from "../tunnels/tunnelIpc";
import { IconButton, ProtocolIcon, type MenuItem, type ProtocolKind } from "../ui/controls";
import { VncView, type VncTabActions } from "../vnc/VncView";
import * as L from "./layout";
import { LocalTree } from "./LocalTree";
import { Splitter } from "./Splitter";
import { dragTabId, TabStrip, TAB_DRAG_TYPE, type TabView } from "./TabStrip";
import { QuickOpen, type QuickItem } from "./QuickOpen";
import { macTitleBar, TitleBar } from "./TitleBar";
import "./workspace.css";

type CloseStatus = "closing" | "unconfirmed";
type DesktopKind = "rdp" | "vnc";
const compatibleStorage = { kind: "compatible", targetId: "s3:compatible", label: "S3 호환 HTTPS" } as const;
type StorageTarget = { kind: "aws"; targetId: string; label: string } | typeof compatibleStorage;
type Tab = { id: number; label: string } & (
  | { kind: "terminal"; targetId?: string; remoteEditor?: true; elevation?: ElevationRequest; ssm?: SsmTarget; ssh?: SshTransport["kind"]; status: TerminalStatus; sessionId?: string; sftpSessionId?: string; closeStatus?: CloseStatus }
  | { kind: "editor"; file: ApprovedEditorFile; transport: EditorTransport; close: () => Promise<void>; source: EditorSource; attaching?: true; readOnly?: true; nano?: Pick<RemoteNanoFile, "sessionId" | "disconnect"> }
  | { kind: "tunnel"; handle: string; targetId: string; name: string; method: "ssh" | "ssm"; closeStatus?: CloseStatus; closeError?: string }
  | { kind: "rdp"; source?: DesktopTunnelSource; tlsOnly?: boolean; closeStatus?: CloseStatus; targetId?: string; preset?: DesktopPreset; autoStart?: true; release?: () => Promise<void> }
  | { kind: "vnc"; source?: DesktopTunnelSource; closeStatus?: CloseStatus; targetId?: string; preset?: DesktopPreset; autoStart?: true; release?: () => Promise<void> }
  | { kind: "storage"; storage: StorageTarget; closeStatus?: CloseStatus }
);
type ProfileLoad = { kind: "loading" | "ready"; profiles?: SavedConnections }
  | { kind: "error"; profiles?: SavedConnections; error: string };
type SideView = "connections" | "files";
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
    case "running": return status.notice ?? "연결됨";
    case "exited": return `종료 코드 ${status.value}`;
    case "disconnected": return `단절: ${status.value}`;
    case "failed": return `열기 실패: ${status.value}`;
  }
}

function tabIcon(tab: Tab): ProtocolKind {
  switch (tab.kind) {
    case "terminal": return tab.elevation ? "sudo" : tab.ssm ? "ssm" : tab.ssh ? "ssh" : "local";
    case "storage": return "s3";
    default: return tab.kind;
  }
}

function tabState(tab: Tab): TabView["state"] {
  const closeStatus = tab.kind === "editor" ? undefined : tab.closeStatus;
  if (closeStatus === "unconfirmed") return "error";
  if (closeStatus === "closing" || (tab.kind === "editor" && tab.attaching)) return "busy";
  if (tab.kind !== "terminal") return "idle";
  switch (tab.status.kind) {
    case "running": return "running";
    case "opening": case "connecting": return "busy";
    default: return "error";
  }
}

function GroupSplitter({ column, label, ratio, onChange }: { column: number; label: string; ratio: number; onChange: (ratio: number) => void }) {
  const drag = useRef<number | null>(null);
  function pairBounds(element: HTMLElement) {
    const strips = [...(element.closest(".workspace-center") ?? element.parentElement!).querySelectorAll<HTMLElement>(".workspace-group-head")];
    const left = strips.find(strip => Number(strip.style.gridColumnStart) === column - 1)?.getBoundingClientRect();
    const right = strips.find(strip => Number(strip.style.gridColumnStart) === column + 1)?.getBoundingClientRect();
    return left && right ? { start: left.left, width: right.right - left.left } : undefined;
  }
  return <div className="workspace-group-splitter" role="separator" tabIndex={0} aria-label={label} aria-orientation="vertical"
    aria-valuemin={15} aria-valuemax={85} aria-valuenow={Math.round(ratio * 100)} aria-valuetext={`${Math.round(ratio * 100)}%`}
    style={{ gridColumn: column }}
    onPointerDown={event => {
      if (event.button !== 0) return;
      event.preventDefault();
      event.currentTarget.focus();
      event.currentTarget.setPointerCapture(event.pointerId);
      drag.current = event.pointerId;
    }}
    onPointerMove={event => {
      if (drag.current !== event.pointerId) return;
      const bounds = pairBounds(event.currentTarget);
      if (bounds && bounds.width > 0) onChange((event.clientX - bounds.start) / bounds.width);
    }}
    onLostPointerCapture={() => { drag.current = null; }}
    onKeyDown={event => {
      const step = event.key === "ArrowLeft" ? -0.05 : event.key === "ArrowRight" ? 0.05 : 0;
      if (!step) return;
      event.preventDefault();
      onChange(ratio + step);
    }} />;
}

export function Workspace() {
  const nextTab = useRef(2);
  const [tabs, setTabs] = useState<Tab[]>([{ id: 1, label: "로컬 1", kind: "terminal", status: { kind: "opening" } }]);
  const [layout, setLayout] = useState<L.Layout>(() => L.initialLayout(1));
  const liveLayout = useRef(layout);
  const activeId = useRef(1);
  const [dragging, setDragging] = useState<number | null>(null);
  const [dropTarget, setDropTarget] = useState<{ group: number; side: "left" | "right" | "center" }>();
  const [sideView, setSideView] = useState<SideView>("connections");
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [aiOpen, setAiOpen] = useState(true);
  const [quickOpen, setQuickOpen] = useState(false);
  const [desktopOpening, setDesktopOpening] = useState<{ id: string; label: string; controller: AbortController }>();
  // Windows Terminal과 같은 Ctrl+Shift+P로 빠른 열기를 연다. 셸이 쓰는 Ctrl+K·Ctrl+P는 가로채지 않는다.
  useEffect(() => {
    const open = (event: KeyboardEvent) => {
      if (!event.ctrlKey || !event.shiftKey || event.altKey || event.metaKey || event.isComposing ||
        (event.code !== "KeyP" && event.key.toLowerCase() !== "p")) return;
      // 모달 대화상자(마법사·승격) 밖의 문서는 inert이므로 그 위에 열지 않는다.
      if (document.querySelector("dialog[open]")) return;
      event.preventDefault();
      event.stopPropagation();
      if (!closingWindow.current) setQuickOpen(true);
    };
    window.addEventListener("keydown", open, true);
    // 어디에 놓든, 끌기가 어떻게 끝나든 드롭 영역을 걷어 낸다. drop은 React 루트의 처리(이동·분할)가 끝난 뒤인
    // window 버블 단계에서 받는다. 캡처에서 지우면 드롭 영역이 먼저 사라져 onDrop이 실행되지 않는다.
    const endDrag = () => { setDragging(null); setDropTarget(undefined); };
    window.addEventListener("drop", endDrag);
    window.addEventListener("dragend", endDrag, true);
    return () => {
      window.removeEventListener("keydown", open, true);
      window.removeEventListener("drop", endDrag);
      window.removeEventListener("dragend", endDrag, true);
    };
  }, []);
  const sftpPanels = useRef(new Map<string, SftpPanelActions>());
  const editorActions = useRef(new Map<number, EditorTabActions>());
  const desktopActions = useRef(new Map<number, RdpTabActions | VncTabActions>());
  /** 경로를 다시 여는 중인 화면 탭이다. 같은 탭의 두 번째 요청은 받지 않는다. */
  const reopeningRoutes = useRef(new Set<number>());
  const cliMounts = useRef(new Map<number, (mounted: boolean) => void>());
  const pendingEditorMounts = useRef(new Set<number>());
  const closingWindow = useRef(false);
  const openingFile = useRef(false);
  const [filePickerBusy, setFilePickerBusy] = useState(false);
  const [fileError, setFileError] = useState("");
  const [focusTab, setFocusTab] = useState<number | null>(1);
  // 포커스가 문서 밖으로 빠져도 focusTab은 그대로라 같은 값 설정은 렌더를 만들지 않는다. 요청마다 올린다.
  const [focusSeq, setFocusSeq] = useState(0);
  const liveTabs = useRef(tabs);
  const closingTabs = useRef(new Set<number>());
  const sessionClosers = useRef(new Map<number, { confirmClose(): Promise<void> }>());
  const [closeBlock, setCloseBlock] = useState<{ kind: "picker" | "cli" | "mount"; message: string }
    | { kind: "editor"; id: number; message: string }>();
  const active = L.focusedTab(layout) ?? 0;
  const visibleSet = L.visibleTabs(layout);
  useLayoutEffect(() => {
    liveTabs.current = tabs;
    const mountingEditors = pendingEditorMounts.current.size;
    for (const tab of tabs) {
      if (tab.kind !== "editor") continue;
      if (tab.nano) {
        const { sessionId, disconnect } = tab.nano;
        const parent = tabs.find(item => item.kind === "terminal" && item.remoteEditor && item.sessionId === sessionId);
        if (parent?.kind !== "terminal" || parent.closeStatus ||
          parent.status.kind === "exited" || parent.status.kind === "disconnected" || parent.status.kind === "failed") {
          disconnect("부모 SSH 연결을 사용할 수 없습니다");
        }
      }
      pendingEditorMounts.current.delete(tab.id);
      if (tab.attaching) cliMounts.current.get(tab.id)?.(editorActions.current.has(tab.id));
    }
    if (mountingEditors && !pendingEditorMounts.current.size) {
      setCloseBlock(current => current?.kind === "mount" ? undefined : current);
    }
  }, [tabs]);
  // 보이는 편집기만 활성 상태로 둔다. 그룹마다 활성 탭이 하나씩 보인다.
  useLayoutEffect(() => {
    const visible = L.visibleTabs(layout);
    for (const [id, actions] of editorActions.current) {
      const tab = tabs.find(item => item.id === id);
      actions.setActive(visible.has(id) && !(tab?.kind === "editor" && tab.attaching));
    }
  }, [layout, tabs]);
  const [focused, setFocused] = useState(false);
  const [windowError, setWindowError] = useState("");
  // SSO 만료로 열지 못한 원격 화면 프로필이다. 로그인에 성공하면 같은 위치로 다시 연다.
  const [desktopRetry, setDesktopRetry] = useState<{ target: DesktopProfile; split?: Placement; error: string }>();
  // 로그인을 시작할 때의 재시도 대상이다. 성공하면 이 대상만 다시 연다.
  const desktopLoginRetry = useRef<typeof desktopRetry>(undefined);
  const desktopLogin = useSsoLogin(desktopRetry?.target.id, targetId => {
    const retry = desktopLoginRetry.current;
    desktopLoginRetry.current = undefined;
    if (retry?.target.id === targetId) void openDesktopTarget(retry.target, retry.split);
  });
  const cancelDesktopLogin = desktopLogin.cancel;
  // 다른 열기 시도가 재시도 대상을 바꾸면 진행 중인 로그인을 취소한다.
  useEffect(() => {
    desktopLoginRetry.current = undefined;
    cancelDesktopLogin();
  }, [desktopRetry, cancelDesktopLogin]);
  const [closeError, setCloseError] = useState("");
  const [loadVersion, setLoadVersion] = useState(0);
  const [profileLoad, setProfileLoad] = useState<ProfileLoad>({ kind: "loading" });
  const [hostRegistration, setHostRegistration] = useState<{ kind: "status" | "alert"; message: string }>();
  const [wizard, setWizard] = useState<WizardRequest>();
  const [elevationContext, setElevationContext] = useState<Readonly<{ targetId?: string; target?: string; account?: string }>>();
  const [storedAuth, setStoredAuth] = useState(false);
  const elevation = elevationActions(storedAuth, navigator.userAgent);
  const [treeWidth, setTreeWidth] = useState(280);
  const [aiWidth, setAiWidth] = useState(360);
  const [bodyWidth, setBodyWidth] = useState(1200);
  const workspace = useRef<HTMLElement>(null);
  const body = useRef<HTMLDivElement>(null);
  const sideLimit = Math.min(640, Math.floor(bodyWidth * 0.32));
  const left = Math.min(treeWidth, sideLimit);
  const right = Math.min(aiWidth, sideLimit);

  function updateLayout(change: (current: L.Layout) => L.Layout) {
    const next = change(liveLayout.current);
    if (next === liveLayout.current) return;
    liveLayout.current = next;
    activeId.current = L.focusedTab(next) ?? 0;
    setLayout(next);
  }

  useEffect(() => {
    if (closeBlock?.kind !== "editor") return;
    // 숨은 편집 탭의 처리 상태도 부모 렌더링과 독립적으로 바뀐다.
    const timer = window.setInterval(() => {
      const tab = liveTabs.current.find(tab => tab.id === closeBlock.id);
      if (!tab || (tab.kind === "editor" && !tab.attaching && editorActions.current.get(tab.id)?.closeState()?.pending === false)) {
        setCloseBlock(current => current === closeBlock ? undefined : current);
      }
    }, 100);
    return () => window.clearInterval(timer);
  }, [closeBlock]);

  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let answeredId: string | undefined;
    const target = { kind: "Webview", label: "main" } as const;
    const registration = Promise.resolve().then(() => {
      if (disposed) return;
      return listen<{ requestId: string }>("workspace-close-requested", ({ payload }) => {
        const requestId = payload?.requestId;
        if (typeof requestId === "string" && requestId) void respond(requestId);
      }, { target });
    });

    async function respond(requestId: string) {
      if (disposed || closingWindow.current || requestId === answeredId) return;
      answeredId = requestId;
      setCloseError("");
      let blocked: typeof closeBlock = openingFile.current ? { kind: "picker", message: "파일 선택 중 · 창 닫기 보류" } :
        cliMounts.current.size ? { kind: "cli", message: "CLI 편집 탭 연결 중 · 창 닫기 보류" } :
          pendingEditorMounts.current.size ? { kind: "mount", message: "편집 탭 여는 중 · 창 닫기 보류" } : undefined;
      const element = blocked ? undefined : workspace.current;
      // showModal()로 열린 대화상자는 상위 요소의 inert를 상속하지 않는다.
      const dialog = element?.querySelector<HTMLDialogElement>("dialog[open]:not([inert])");
      const focusedElement = element ? document.activeElement : null;
      const blockCancel = (event: Event) => {
        event.preventDefault();
        event.stopImmediatePropagation();
      };
      let locked = false;
      let destroyQueued = false;
      function unlock() {
        if (!locked) return;
        locked = false;
        closingWindow.current = false;
        element?.removeAttribute("inert");
        dialog?.removeAttribute("inert");
        dialog?.removeEventListener("cancel", blockCancel, { capture: true });
        if (!disposed && focusedElement instanceof HTMLElement && element?.contains(focusedElement)) focusedElement.focus();
      }
      try {
        if (!blocked) {
          locked = true;
          closingWindow.current = true;
          if (focusedElement instanceof HTMLElement && element?.contains(focusedElement)) focusedElement.blur();
          element?.setAttribute("inert", "");
          dialog?.setAttribute("inert", "");
          dialog?.addEventListener("cancel", blockCancel, { capture: true });
        }
        // 포커스를 해제할 때 확정된 입력까지 닫기 판정에 포함한다.
        const discard: string[] = [];
        if (!blocked) {
          for (const tab of liveTabs.current) {
            if (tab.kind !== "editor") continue;
            const state = editorActions.current.get(tab.id)?.closeState();
            if (tab.attaching || !state || state.pending) {
              blocked = { kind: "editor", id: tab.id, message: `${tab.label}: 편집 작업 중 · 창 닫기 보류` };
              break;
            }
            if (state.dirty || state.uncertain) discard.push(tab.label);
          }
        }
        setCloseBlock(blocked);
        if (blocked) unlock();
        const decision = blocked ? { kind: "blocked" } : discard.length ? { kind: "discard", files: discard } : { kind: "ready" };
        destroyQueued = await invoke<boolean>("workspace_window", { action: "respondToClose", requestId, decision });
      } catch (error) {
        if (answeredId === requestId) {
          if (!disposed) setCloseError(`닫기 실패: ${String(error)}`);
        }
      } finally {
        if (!destroyQueued) unlock();
      }
    }
    void registration.then(async () => {
      if (disposed) return;
      const requestId = await invoke<string | null>("workspace_window", { action: "pendingClose" });
      if (typeof requestId === "string" && requestId) void respond(requestId);
    }).catch((error: unknown) => {
      if (!disposed) setCloseError(`창 닫기 연결 실패: ${String(error)}`);
    });
    return () => {
      disposed = true;
      void registration.then(unlisten => unlisten?.(), () => { });
    };
  }, []);

  useEffect(() => {
    const attachingHandles = new Set<ApprovedEditorFile["handle"]>();
    const reportLocal = (error: unknown) => setFileError(`CLI 파일 열기 실패: ${localEditorError(error).message}`);
    const reportNano = (error: unknown) => setFileError(`nano 편집 연결 확인 불가: ${sftpError(error).message}`);
    async function attach(file: ApprovedEditorFile, transport: EditorTransport, close: () => Promise<void>,
      source: "local" | "sftp", acknowledge: () => Promise<"mounted" | "unconfirmed" | "unavailable">, signal: AbortSignal,
      nano?: Pick<RemoteNanoFile, "sessionId" | "disconnect">) {
      // 오퍼에 기록된 백엔드 세션은 활성 탭이나 SFTP 패널과 독립적이다.
      if (closingWindow.current || signal.aborted || attachingHandles.has(file.handle) || liveTabs.current.some(tab =>
        tab.kind === "editor" && tab.source === source && tab.file.handle === file.handle)) return;
      attachingHandles.add(file.handle);
      const id = nextTab.current++;
      const removePreview = () => {
        setTabs(current => current.filter(tab => tab.id !== id));
        updateLayout(current => L.removeTab(current, id));
      };
      const { promise: mounted, resolve } = Promise.withResolvers<boolean>();
      cliMounts.current.set(id, resolve);
      const abort = () => {
        resolve(false);
        removePreview();
      };
      signal.addEventListener("abort", abort, { once: true });
      setTabs(current => [...current, { id, label: file.label, kind: "editor", file, source, transport, close, attaching: true, nano }]);
      updateLayout(current => L.appendTab(current, id));
      try {
        const committed = await mounted;
        if (signal.aborted) return;
        if (!committed) throw new Error("편집기 탭을 부착할 수 없습니다");
        const result = await acknowledge();
        if (result === "unavailable") { removePreview(); return; }
        if (signal.aborted) return;
        setTabs(current => current.map(tab => tab.id === id && tab.kind === "editor"
          ? { ...tab, attaching: undefined, readOnly: result === "unconfirmed" ? true : undefined } : tab));
        selectTab(id);
      } catch (error) {
        removePreview();
        throw error;
      } finally {
        cliMounts.current.delete(id);
        if (!cliMounts.current.size) setCloseBlock(current => current?.kind === "cli" ? undefined : current);
        attachingHandles.delete(file.handle);
        signal.removeEventListener("abort", abort);
      }
    }
    const local = subscribeLocalCliOpen((file, respond, signal) =>
      attach(file, localEditorTransport, () => closeLocalFile(file.handle), "local", async () => {
        await respond({ attached: true });
        return "mounted";
      }, signal).catch(error => { if (!signal.aborted) reportLocal(error); throw error; }), reportLocal);
    const nano = subscribeRemoteNanoOpen((file, acknowledge, signal) => {
      if (!liveTabs.current.some(tab => tab.kind === "terminal" && tab.remoteEditor && tab.sessionId === file.sessionId &&
        !tab.closeStatus && !closingTabs.current.has(tab.id) &&
        tab.status.kind !== "exited" && tab.status.kind !== "disconnected" && tab.status.kind !== "failed")) {
        return Promise.resolve();
      }
      return attach(file, file.transport, file.close, "sftp", async () => {
        const result = await acknowledge();
        if (result.kind === "unconfirmed" && !signal.aborted) reportNano(result.error);
        return result.kind === "aborted" ? "unavailable" : result.kind;
      }, signal, file).catch(error => { if (!signal.aborted) reportNano(error); throw error; });
    }, reportNano);
    const unsubscribe = () => { local(); nano(); };
    window.addEventListener("pagehide", unsubscribe);
    return () => {
      window.removeEventListener("pagehide", unsubscribe);
      unsubscribe();
    };
  }, []);

  useEffect(() => {
    let active = true;
    void connections.capabilities().then((value) => {
      if (active) setStoredAuth(value.storedAuth === true);
    }, () => { /* 비 Tauri 미리보기와 구 빌드에서는 승격 메뉴를 숨긴다. */ });
    return () => { active = false; };
  }, []);

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
    if (!target || (target.profile.defaultMethod !== "ssm" && target.profile.defaultMethod !== "s3")) return;
    if (!group) {
      setWindowError("그룹 없는 세션은 연결 재시험을 할 수 없습니다");
      return;
    }
    setWizard({
      draftId: crypto.randomUUID(), initialDraft: {
        version: 1, groupRevision: group.revision, targetRevision: target.revision,
        group: group.profile, target: { ...target.profile, groupId: group.profile.id },
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

  type Placement = { group: number; side: "left" | "right" };

  /** 새 탭을 포커스 그룹(또는 지정한 그룹 옆의 새 그룹)에 놓고 선택한다. */
  function placeTab(id: number, split?: Placement) {
    updateLayout(current => split ? L.addTabInSplit(current, id, split.group, split.side) : L.addTab(current, id));
    setFocusTab(id);
  }

  function openTab(target?: SavedConnections["targets"][number]["profile"], options?: { readonly remoteEditor?: true }, split?: Placement) {
    if (target?.defaultMethod === "s3") {
      openStorageTarget({ kind: "aws", targetId: target.id, label: target.displayName });
      return;
    }
    if (target?.defaultMethod === "rdp" || target?.defaultMethod === "vnc") {
      void openDesktopTarget(target, split);
      return;
    }
    const id = nextTab.current++;
    setTabs((current) => [...current, {
      id, label: target?.displayName ?? `로컬 ${id}`, kind: "terminal", targetId: target?.id,
      remoteEditor: options?.remoteEditor,
      ssm: target?.defaultMethod === "ssm" ? target.ssm : undefined,
      ssh: target?.ssh?.transport.kind, status: { kind: "opening" },
    }]);
    placeTab(id, split);
  }

  function duplicateTab(id: number, split: boolean) {
    const tab = liveTabs.current.find(item => item.id === id);
    if (tab?.kind !== "terminal" || tab.elevation) return;
    const group = L.groupOf(liveLayout.current, id);
    const placement = split && group ? { group: group.id, side: "right" as const } : undefined;
    if (!tab.targetId) {
      openTab(undefined, undefined, placement);
      return;
    }
    const target = profileLoad.profiles?.targets.find(({ profile }) => profile.id === tab.targetId)?.profile;
    if (target) openTab(target, tab.remoteEditor ? { remoteEditor: true } : undefined, placement);
  }

  /** 저장된 RDP·VNC 프로필의 경로를 연 뒤 화면 탭을 만든다. 탭은 준비된 경로로 바로 연결을 시작한다. */
  async function openDesktopTarget(target: DesktopProfile, split?: Placement) {
    if (desktopOpening?.id === target.id) return;
    setWindowError("");
    setDesktopRetry(undefined);
    const pending = { id: target.id, label: target.displayName, controller: new AbortController() };
    desktopOpening?.controller.abort();
    setDesktopOpening(pending);
    try {
      const opened = await openDesktopProfile(target, { signal: pending.controller.signal });
      if (closingWindow.current || pending.controller.signal.aborted) {
        await opened.close();
        return;
      }
      const id = nextTab.current++;
      setTabs(current => [...current, {
        id, label: opened.label, kind: opened.kind, source: opened.source, targetId: target.id, preset: opened.preset, autoStart: true, release: opened.close,
      }]);
      placeTab(id, split);
    } catch (error) {
      if (error instanceof DesktopRouteError) keepUnclosedRoute(error.handle, error.targetId, target.displayName, error.message, error.method);
      if (!pending.controller.signal.aborted || error instanceof DesktopRouteError) {
        const message = error instanceof Error ? error.message : String(error);
        const failure = `${target.displayName} 열기 실패: ${message}`;
        setWindowError(failure);
        if (ssoExpired(message)) setDesktopRetry({ target, split, error: failure });
      }
    } finally {
      setDesktopOpening(current => current === pending ? undefined : current);
    }
  }

  /** 끊긴 프로필 경로를 닫고 같은 프로필로 다시 열어 화면 탭의 source만 바꾼다. 연결은 탭이 새 source로 시작한다. */
  async function reopenDesktopRoute(id: number): Promise<boolean> {
    const tab = liveTabs.current.find(item => item.id === id);
    if ((tab?.kind !== "rdp" && tab?.kind !== "vnc") || !tab.targetId || closingTabs.current.has(id)) return false;
    const target = profileLoad.profiles?.targets.find(({ profile }) => profile.id === tab.targetId)?.profile;
    if (target?.defaultMethod !== "rdp" && target?.defaultMethod !== "vnc") return false;
    if (desktopOpening?.id === target.id || reopeningRoutes.current.has(id)) return false;
    reopeningRoutes.current.add(id);
    setWindowError("");
    const pending = { id: target.id, label: target.displayName, controller: new AbortController() };
    desktopOpening?.controller.abort();
    setDesktopOpening(pending);
    const previous = tab.source;
    const release = tab.release;
    setTabs(current => current.map(item => item.id === id && item.kind === tab.kind ? { ...item, release: undefined } : item));
    try {
      await release?.().catch(error => {
        setWindowError(`${tab.label} 경로 종료 실패: ${String(error)}`);
        if (previous) keepUnclosedRoute(previous.tabHandle, previous.targetId, tab.label, String(error), previous.targetId === tab.targetId ? "ssm" : "ssh");
      });
      const opened = await openDesktopProfile(target, { signal: pending.controller.signal });
      if (closingWindow.current || pending.controller.signal.aborted || !liveTabs.current.some(item => item.id === id) || closingTabs.current.has(id)) {
        await opened.close();
        return false;
      }
      setTabs(current => current.map(item => item.id === id && item.kind === tab.kind
        ? { ...item, source: opened.source, release: opened.close } : item));
      return true;
    } catch (error) {
      if (error instanceof DesktopRouteError) keepUnclosedRoute(error.handle, error.targetId, target.displayName, error.message, error.method);
      if (!pending.controller.signal.aborted || error instanceof DesktopRouteError) {
        setWindowError(`${target.displayName} 경로 다시 열기 실패: ${error instanceof Error ? error.message : String(error)}`);
      }
      return false;
    } finally {
      reopeningRoutes.current.delete(id);
      setDesktopOpening(current => current === pending ? undefined : current);
    }
  }

  /** 종료를 확인하지 못한 경로 터널은 보이는 터널 탭으로 남겨 같은 핸들로 다시 닫게 한다. */
  function keepUnclosedRoute(handle: string, targetId: string, name: string, error: string, method: "ssh" | "ssm") {
    const id = nextTab.current++;
    setTabs(current => [...current, { kind: "tunnel", id, label: `${name} · 경로`, handle, targetId, name, method, closeStatus: "unconfirmed", closeError: error }]);
    updateLayout(current => L.appendTab(current, id));
  }

  function openElevation(request: ElevationRequest) {
    const id = nextTab.current++;
    const name = request.program.split("/").filter(Boolean).pop() ?? request.program;
    setTabs((current) => [...current, {
      id, label: `승격 · ${name}`, kind: "terminal", elevation: request, status: { kind: "opening" },
    }]);
    placeTab(id);
  }

  function openDesktop(kind: DesktopKind, source?: DesktopTunnelSource) {
    const id = nextTab.current++;
    setTabs(current => [...current, { id, label: `${kind.toUpperCase()} ${id}`, kind, source }]);
    placeTab(id);
  }

  async function openTunnel(targetId: string, name: string, method: "ssh" | "ssm", share?: { tab: string; id: string }) {
    const id = nextTab.current++;
    const label = `${name} · 터널 ${id}`;
    const handle = await tunnelIpc.open(targetId, label);
    const tab = { kind: "tunnel", id, label, handle, targetId, name, method } as const;
    if (share) {
      try {
        await tunnelIpc.share(share.tab, handle, share.id);
      } catch (error) {
        try {
          await tunnelIpc.close(handle);
        } catch (closeError) {
          setTabs(current => [...current, { ...tab, closeStatus: "unconfirmed", closeError: String(closeError) }]);
          updateLayout(current => L.appendTab(current, id));
        }
        throw error;
      }
    }
    setTabs(current => [...current, tab]);
    placeTab(id);
  }

  function openStorageTarget(target: StorageTarget) {
    const existing = liveTabs.current.find(tab => tab.kind === "storage" && tab.storage.targetId === target.targetId);
    if (existing) {
      selectTab(existing.id);
      return;
    }
    const id = nextTab.current++;
    setTabs(current => [...current, { id, label: target.label, kind: "storage", storage: target }]);
    placeTab(id);
  }

  function releaseEditorFile(source: EditorSource, close: () => Promise<void>) {
    void close().catch((error: unknown) => {
      const message = source === "local" ? localEditorError(error).message : source === "sftp" ? sftpError(error).message
        : error instanceof Error ? error.message : String(error);
      setFileError(`파일 닫기 실패: ${message}`);
    });
  }

  function openEditorTab(file: ApprovedEditorFile, transport: EditorTransport, close: () => Promise<void>, source: EditorSource) {
    if (closingWindow.current) {
      releaseEditorFile(source, close);
      return;
    }
    const id = nextTab.current++;
    pendingEditorMounts.current.add(id);
    setTabs((current) => [...current, { id, label: file.label, kind: "editor", file, transport, close, source }]);
    placeTab(id);
  }

  async function openEditor() {
    if (openingFile.current || closingWindow.current) return;
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
      setCloseBlock(current => current?.kind === "picker" ? undefined : current);
    }
  }

  function selectTab(id: number, requestFocus = true) {
    updateLayout(current => L.selectTab(current, id));
    setFocusTab(requestFocus ? id : null);
    if (requestFocus) setFocusSeq(seq => seq + 1);
  }

  async function closeTab(id: number) {
    const tab = liveTabs.current.find(item => item.id === id);
    if (!tab || closingTabs.current.has(id)) return;
    if (tab.kind === "rdp" || tab.kind === "vnc") {
      const actions = desktopActions.current.get(id);
      if (!actions) return;
      closingTabs.current.add(id);
      setTabs(current => current.map(item => item.id === id && (item.kind === "rdp" || item.kind === "vnc")
        ? { ...item, closeStatus: "closing" } : item));
      const confirmed = await actions.requestClose().catch(() => false);
      closingTabs.current.delete(id);
      if (confirmed !== true) {
        setTabs(current => current.map(item => item.id === id && (item.kind === "rdp" || item.kind === "vnc")
          ? { ...item, closeStatus: "unconfirmed" } : item));
        selectTab(id);
        return;
      }
      // 프로필 경로로 연 전달은 화면 탭과 함께 닫는다.
      const source = tab.source;
      void tab.release?.().catch(error => {
        setWindowError(`${tab.label} 경로 종료 실패: ${String(error)}`);
        // SSM 경로는 원격 화면 프로필 자신을 경로 대상으로 연다.
        if (source) keepUnclosedRoute(source.tabHandle, source.targetId, tab.label, String(error), source.targetId === tab.targetId ? "ssm" : "ssh");
      });
    }
    if (tab.kind === "tunnel" || tab.kind === "storage" || (tab.kind === "terminal" && (tab.ssm || tab.ssh))) {
      closingTabs.current.add(id);
      setTabs(current => current.map(item => item.id === id && (item.kind === "terminal" || item.kind === "tunnel" || item.kind === "storage")
        ? { ...item, closeStatus: "closing" } : item));
      try {
        if (tab.kind === "tunnel") {
          await tunnelIpc.close(tab.handle);
        } else {
          if (tab.kind === "terminal" && tab.sftpSessionId) {
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
        }
      } catch (error) {
        setTabs(current => current.map(item => {
          if (item.id !== id || (item.kind !== "terminal" && item.kind !== "tunnel" && item.kind !== "storage")) return item;
          return item.kind === "tunnel" ? { ...item, closeStatus: "unconfirmed", closeError: String(error) }
            : { ...item, closeStatus: "unconfirmed" };
        }));
        return;
      } finally {
        closingTabs.current.delete(id);
      }
    }
    if (!liveTabs.current.some(item => item.id === id)) return;
    const wasFocused = id === activeId.current;
    updateLayout(current => L.removeTab(current, id));
    if (wasFocused) setFocusTab(L.focusedTab(liveLayout.current));
    liveTabs.current = liveTabs.current.filter(item => item.id !== id);
    setTabs((current) => current.filter((tab) => tab.id !== id));
  }

  function requestClose(id: number) {
    const tab = liveTabs.current.find(item => item.id === id);
    if (tab?.kind === "editor") void editorActions.current.get(id)?.requestClose();
    else void closeTab(id);
  }

  function updateStatus(id: number, status: TerminalStatus, sessionId?: string) {
    setTabs((current) => current.map((tab) => tab.id === id && tab.kind === "terminal"
      ? {
        ...tab, status, sessionId: sessionId ?? tab.sessionId,
        sftpSessionId: status.kind === "running" && (sessionId ?? tab.sessionId) === tab.sftpSessionId ? tab.sftpSessionId : undefined
      }
      : tab));
  }

  const current = tabs.find((tab) => tab.id === active);
  const aiSessions = tabs.flatMap(tab => tab.kind === "terminal" && tab.ssm && tab.sessionId &&
    tab.status.kind === "running" && !tab.closeStatus
    ? [{ id: tab.sessionId, label: tab.label, region: tab.ssm.region, node: tab.ssm.instanceId }]
    : []);
  const sidebarVisible = sidebarOpen && !focused;
  const sftpCandidate = current?.kind === "terminal" && current.ssh === "direct" &&
    !current.closeStatus && current.status.kind === "running" ? current.sessionId : undefined;
  const activeSftp = sidebarVisible && sideView === "files" ? sftpCandidate : undefined;
  const attachedSftp = current?.kind === "terminal" ? current.sftpSessionId : undefined;
  useLayoutEffect(() => {
    if (!activeSftp || attachedSftp === activeSftp) return;
    setTabs(current => current.map(tab => tab.id === active && tab.kind === "terminal" &&
      tab.status.kind === "running" && tab.sessionId === activeSftp && !tab.closeStatus
      ? { ...tab, sftpSessionId: activeSftp } : tab));
  }, [active, activeSftp, attachedSftp]);
  const openCounts = new Map<string, number>();
  for (const tab of tabs) {
    const key = tab.kind === "terminal" && !tab.elevation ? tab.targetId ?? "" : tab.kind === "storage" ? tab.storage.targetId
      : (tab.kind === "rdp" || tab.kind === "vnc") ? tab.targetId : undefined;
    if (key !== undefined) openCounts.set(key, (openCounts.get(key) ?? 0) + 1);
  }
  const sideWidth = sidebarOpen ? left : 0;
  const style = {
    "--tree-width": `${sideWidth}px`, "--ai-width": `${aiOpen ? right : 0}px`,
    "--side-width": `calc(var(--rail-width) + ${sideWidth}px)`,
  } as CSSProperties;
  const columns = layout.groups.map(group => `minmax(0, ${group.size}fr)`).join(" 5px ");
  const groupColumn = new Map(layout.groups.map((group, index) => [group.id, index * 2 + 1]));
  const tabColumn = (id: number) => groupColumn.get(L.groupOf(layout, id)?.id ?? -1) ?? 1;

  function blockClosingInput(event: SyntheticEvent) {
    if (!closingWindow.current) return;
    event.preventDefault();
    event.stopPropagation();
  }

  function toggleSide(view: SideView) {
    if (sidebarOpen && sideView === view) setSidebarOpen(false);
    else {
      setSideView(view);
      setSidebarOpen(true);
    }
  }

  function dropSide(event: DragEvent<HTMLElement>): "left" | "right" | "center" {
    const rect = event.currentTarget.getBoundingClientRect();
    const x = (event.clientX - rect.left) / Math.max(1, rect.width);
    return x < 0.3 ? "left" : x > 0.7 ? "right" : "center";
  }

  const newTabItems: MenuItem[] = [
    { label: "로컬 터미널", icon: SquareTerminal, onSelect: () => openTab() },
    { label: "파일 열기…", icon: FileText, disabled: filePickerBusy, onSelect: () => void openEditor() },
    { kind: "separator" },
    { label: "RDP 연결", icon: Monitor, onSelect: () => openDesktop("rdp") },
    { label: "VNC 연결", icon: MonitorSmartphone, onSelect: () => openDesktop("vnc") },
    { label: "S3 호환 스토리지", icon: Database, onSelect: () => openStorageTarget(compatibleStorage) },
    { kind: "separator" },
    { label: "새 연결 프로필…", icon: Plus, disabled: !!wizard, onSelect: openWizard },
  ];

  const quickItems: QuickItem[] = [
    ...tabs.map(tab => ({
      id: `tab:${tab.id}`, section: "열린 탭", label: tab.label, icon: <ProtocolIcon kind={tabIcon(tab)} />,
      detail: layout.groups.length > 1 ? `그룹 ${layout.groups.findIndex(group => group.tabs.includes(tab.id)) + 1}` : undefined,
      run: () => selectTab(tab.id),
    })),
    { id: "target:local", section: "연결", label: "내 PC", detail: "로컬", icon: <ProtocolIcon kind="local" />, run: () => openTab() },
    ...(profileLoad.profiles?.targets ?? []).map(({ profile }) => ({
      id: `target:${profile.id}`, section: "연결", label: profile.displayName,
      detail: [profileLoad.profiles?.groups.find(group => group.profile.id === profile.groupId)?.profile.displayName,
        profile.defaultMethod === "ssm" ? `SSM · ${profile.ssm.instanceId}` : profile.defaultMethod === "ssh" ? `SSH · ${profile.ssh.account}@${profile.ssh.host}`
          : profile.defaultMethod === "s3" ? `S3 · ${profile.s3.bucket}`
            : profile.defaultMethod === "rdp" ? `RDP · ${profile.rdp.host || profile.rdp.route.kind}`
              : profile.defaultMethod === "vnc" ? `VNC · ${profile.vnc.host || profile.vnc.route.kind}` : "로컬"].filter(Boolean).join(" · "),
      icon: <ProtocolIcon kind={profile.defaultMethod === "local" ? "local" : profile.defaultMethod as ProtocolKind} />,
      run: () => openTab(profile),
    })),
    ...newTabItems.flatMap(item => item.kind ? [] : item.disabled ? [] : [{
      id: `command:${item.label}`, section: "명령", label: item.label, icon: item.icon ? <item.icon className="proto-icon" aria-hidden="true" /> : null, run: item.onSelect,
    }]),
    { id: "command:sidebar", section: "명령", label: sidebarOpen ? "사이드바 숨기기" : "사이드바 보이기", icon: <PanelLeft className="proto-icon" aria-hidden="true" />, run: () => setSidebarOpen(!sidebarOpen) },
    { id: "command:ai", section: "명령", label: aiOpen ? "AI 패널 숨기기" : "AI 패널 보이기", icon: <PanelRight className="proto-icon" aria-hidden="true" />, run: () => setAiOpen(!aiOpen) },
    { id: "command:focus", section: "명령", label: focused ? "기본 보기" : "집중 보기", icon: <Maximize2 className="proto-icon" aria-hidden="true" />, run: () => setFocused(!focused) },
    ...(layout.groups.length > 1 ? [{ id: "command:merge", section: "명령", label: "모든 그룹 합치기", icon: <Columns2 className="proto-icon" aria-hidden="true" />, run: () => updateLayout(L.mergeGroups) }] : []),
  ];

  return (
    <main ref={workspace} className={`workspace${macTitleBar ? " workspace-mac" : ""}${focused ? " workspace-focused" : ""}`} style={style}
      onClickCapture={blockClosingInput} onMouseDownCapture={blockClosingInput}
      onKeyDownCapture={blockClosingInput} onPointerMoveCapture={blockClosingInput}
      onFocusCapture={(event) => { if (!event.target.closest(".terminal-screen, .monaco-editor")) setFocusTab(null); }}>
      <TitleBar focused={focused} sidebar={sidebarOpen} aiPanel={aiOpen} newTabItems={newTabItems}
        onNewTerminal={() => openTab()} onToggle={() => setFocused(!focused)}
        onToggleSidebar={() => setSidebarOpen(!sidebarOpen)} onToggleAi={() => setAiOpen(!aiOpen)} onWindowError={setWindowError}>
        <button type="button" className="workspace-search" aria-label="빠른 열기" title="빠른 열기 (Ctrl+Shift+P)"
          onMouseDown={event => { if (document.activeElement?.closest(".terminal-screen")) event.preventDefault(); }}
          onClick={() => setQuickOpen(true)}>
          <Search aria-hidden="true" /><span>연결, 명령, 탭 검색</span><kbd>Ctrl+Shift+P</kbd>
        </button>
      </TitleBar>
      {quickOpen && <QuickOpen items={quickItems} onClose={() => setQuickOpen(false)} />}
      <div className="workspace-body" ref={body}>
        <nav className="workspace-rail" aria-label="사이드바 보기" hidden={focused}>
          <IconButton icon={Server} label="연결" aria-pressed={sidebarOpen && sideView === "connections"} onClick={() => toggleSide("connections")} />
          <IconButton icon={FolderTree} label="SFTP 파일" aria-pressed={sidebarOpen && sideView === "files"} onClick={() => toggleSide("files")}>
            {sftpCandidate && <span className="workspace-rail-badge" aria-hidden="true" />}
          </IconButton>
        </nav>
        <aside className="workspace-sidebar workspace-tree" hidden={!sidebarVisible} aria-label={sideView === "connections" ? "연결" : "SFTP 파일"}>
          <div hidden={sideView !== "connections"} className="workspace-side-body">
            <div className="workspace-side-head">
              <h2>연결</h2>
              <IconButton icon={Plus} label="연결 추가" disabled={!!wizard} onClick={openWizard} />
              <IconButton icon={RefreshCw} label="연결 새로고침" disabled={profileLoad.kind === "loading"} onClick={reloadProfiles} />
            </div>
            {profileLoad.kind === "loading" && <div className="connection-status" role="status">저장된 연결 불러오는 중</div>}
            {profileLoad.kind === "error" && <div className="connection-status">
              <div role="alert">연결 불러오기 실패: {profileLoad.error}</div>
              <button className="ui-button" onClick={reloadProfiles}>다시 불러오기</button>
            </div>}
            <LocalTree profiles={profileLoad.profiles} openCounts={openCounts} onOpen={openTab}
              onLocalElevate={elevation.local ? () => setElevationContext({}) : undefined}
              onElevate={elevation.remote ? (target) => {
                if (target?.defaultMethod !== "ssh") return;
                setElevationContext({
                  targetId: target.id,
                  target: `${target.ssh.account}@${target.ssh.host}:${target.ssh.port}`,
                  account: target.ssh.account,
                });
              } : undefined}
              onRegisterSshHost={(targetId) => void registerSavedSshHost(targetId)}
              onRetestS3={retestStorageTarget} onOpenTunnel={(targetId, name, method) => {
                setWindowError("");
                void openTunnel(targetId, name, method).catch(error => setWindowError(String(error)));
              }}
              onOpenStorage={(targetId, label) => openStorageTarget({ kind: "aws", targetId, label })} />
          </div>
          <div hidden={sideView !== "files"} className="workspace-side-body">
            <div className="workspace-side-head">
              <h2>{current?.kind === "terminal" && sftpCandidate ? `SFTP · ${current.label}` : "SFTP"}</h2>
              <IconButton icon={ChevronsDownUp} label="사이드바 닫기" onClick={() => setSidebarOpen(false)} />
            </div>
            {!activeSftp && <p className="workspace-file-status" role="status">활성 SSH 연결 없음</p>}
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
          </div>
        </aside>
        <div className="workspace-center" style={{ gridTemplateColumns: columns }}>
          {layout.groups.map((group, index) => {
            const views = group.tabs.flatMap(id => {
              const tab = tabs.find(item => item.id === id);
              if (!tab) return [];
              const closeStatus = tab.kind === "editor" ? undefined : tab.closeStatus;
              return [{
                id, label: tab.label, icon: tabIcon(tab), state: tabState(tab), closeStatus,
                locked: tab.kind === "editor" && !!tab.attaching,
                duplicable: tab.kind === "terminal" && !tab.elevation,
              } satisfies TabView];
            });
            return <div key={group.id} className="workspace-tabstrip-slot" style={{ display: "contents" }}>
              <div className="workspace-group-head" style={{ gridColumnStart: index * 2 + 1, gridRow: 1 }}>
                <TabStrip tabs={views} active={group.active} focused={group.id === layout.focused} reveal={[left, focused]}
                  groupIndex={index} groupCount={layout.groups.length} canAddGroup={layout.groups.length < L.MAX_GROUPS}
                  onSelect={id => selectTab(id)} onClose={requestClose}
                  onMove={(id, at) => updateLayout(current => L.moveTab(current, id, group.id, at))}
                  onSplit={(id, side) => updateLayout(current => L.splitTab(current, id, group.id, side))}
                  onMoveToNeighbor={(id, direction) => {
                    const neighbor = layout.groups[index + direction];
                    if (neighbor) updateLayout(current => L.moveTab(current, id, neighbor.id));
                  }}
                  onDuplicate={duplicateTab} onMerge={() => updateLayout(L.mergeGroups)}
                  onDragState={id => { setDragging(id); if (id === null) setDropTarget(undefined); }} />
              </div>
              {index > 0 && <GroupSplitter column={index * 2} label={`탭 그룹 ${index}·${index + 1} 경계`}
                ratio={layout.groups[index - 1].size / (layout.groups[index - 1].size + group.size)}
                onChange={ratio => updateLayout(current => L.resizeGroups(current, index - 1, ratio))} />}
              {!group.tabs.length && <div className="workspace-empty" style={{ gridColumn: index * 2 + 1 }}>
                <SquareTerminal width={40} height={40} strokeWidth={1.25} aria-hidden="true" />
                <div className="workspace-empty-actions">
                  <button type="button" className="ui-button" onClick={() => openTab()}><SquareTerminal aria-hidden="true" />로컬 터미널</button>
                  <button type="button" className="ui-button" disabled={!!wizard} onClick={openWizard}><Plus aria-hidden="true" />연결 추가</button>
                  <button type="button" className="ui-button" disabled={filePickerBusy} onClick={() => void openEditor()}><FileText aria-hidden="true" />파일 열기</button>
                </div>
              </div>}
              {dragging !== null && <div className="workspace-drop-zone" style={{ gridColumn: index * 2 + 1 }}
                data-side={dropTarget?.group === group.id && dropTarget.side !== "center" ? dropTarget.side : undefined}
                onDragOver={event => {
                  if (!event.dataTransfer.types.includes(TAB_DRAG_TYPE)) return;
                  event.preventDefault();
                  const raw = dropSide(event);
                  // 그룹 상한에서는 분할 대신 이동만 한다.
                  const side = raw !== "center" && layout.groups.length >= L.MAX_GROUPS ? "center" : raw;
                  setDropTarget(current => current?.group === group.id && current.side === side ? current : { group: group.id, side });
                }}
                onDragLeave={event => {
                  if (!event.currentTarget.contains(event.relatedTarget as Node)) setDropTarget(undefined);
                }}
                onDrop={event => {
                  const id = dragTabId(event);
                  const raw = dropSide(event);
                  const side = raw !== "center" && layout.groups.length >= L.MAX_GROUPS ? "center" : raw;
                  setDragging(null);
                  setDropTarget(undefined);
                  if (id === undefined) return;
                  event.preventDefault();
                  // 같은 그룹 가운데에 놓으면 순서를 바꾸지 않는다.
                  if (side === "center" && group.tabs.includes(id)) return;
                  updateLayout(current => side === "center" ? L.moveTab(current, id, group.id) : L.splitTab(current, id, group.id, side));
                }} />}
            </div>;
          })}
          {tabs.map(tab => {
            const { id, label } = tab;
            const bindClose = (handle: { confirmClose(): Promise<void> } | null) => {
              if (handle) sessionClosers.current.set(id, handle);
              else sessionClosers.current.delete(id);
            };
            const bindDesktopClose = (actions: RdpTabActions | VncTabActions | null) => {
              if (actions) desktopActions.current.set(id, actions);
              else desktopActions.current.delete(id);
            };
            const visible = visibleSet.has(id) && !(tab.kind === "editor" && tab.attaching && id !== active);
            const desktop = tab.kind === "rdp" || tab.kind === "vnc";
            // 여러 그룹이 보일 때 화면 출력은 유지하되, 사용자 입력은 포커스된 터미널에만 전달한다.
            const terminalHasFocus = (element: HTMLElement) =>
              id === activeId.current && element.contains(element.ownerDocument.activeElement?.closest(".terminal-screen") ?? null);
            const blockTerminalInput = (event: SyntheticEvent<HTMLDivElement>) => {
              if (terminalHasFocus(event.currentTarget)) return;
              event.preventDefault();
              event.stopPropagation();
            };
            const terminalInput = tab.kind === "terminal" ? {
              onKeyDownCapture: blockTerminalInput, onKeyPressCapture: blockTerminalInput, onKeyUpCapture: blockTerminalInput,
              onBeforeInputCapture: blockTerminalInput, onInputCapture: blockTerminalInput,
              onCompositionStartCapture: blockTerminalInput, onCompositionUpdateCapture: blockTerminalInput,
              onCompositionEndCapture: blockTerminalInput, onPasteCapture: blockTerminalInput, onDropCapture: blockTerminalInput,
              onMouseDownCapture: (event: MouseEvent<HTMLDivElement>) => {
                if (terminalHasFocus(event.currentTarget)) return;
                blockTerminalInput(event);
                // 우클릭도 탭을 고르고 포커스를 옮긴다. contextmenu는 그 뒤에 와서 복사·붙여넣기를 한다.
                if (visible && (event.button === 0 || event.button === 2)) selectTab(id);
              },
              onMouseUpCapture: blockTerminalInput, onMouseMoveCapture: blockTerminalInput,
              // 휠 리스너는 passive라 기본 동작을 막을 수 없다. 전파만 막는다.
              onWheelCapture: (event: SyntheticEvent<HTMLDivElement>) => { if (!terminalHasFocus(event.currentTarget)) event.stopPropagation(); },
              onAuxClickCapture: blockTerminalInput, onContextMenuCapture: blockTerminalInput,
            } : {
              onMouseDownCapture: () => { if (visible && id !== activeId.current) selectTab(id, false); },
            };
            return <div className={`workspace-session${desktop ? " workspace-desktop" : ""}`} key={id}
              style={{ gridColumn: tabColumn(id) }} data-tab={id}
              hidden={!visible} inert={!visible} {...terminalInput}
              onFocusCapture={(event) => {
                if (visible && id !== activeId.current && (tab.kind !== "terminal" || event.target.closest(".terminal-screen"))) selectTab(id, false);
              }}>
              {tab.kind === "terminal" ? (
                <TerminalView active={visible} focusRequested={id === focusTab} focusSeq={id === focusTab ? focusSeq : 0} label={label} targetId={tab.targetId}
                  elevation={tab.elevation} reconnect={!tab.elevation}
                  remote={!!tab.ssm} ssh={!!tab.ssh} remoteEditor={tab.remoteEditor} closing={!!tab.closeStatus} closeRef={tab.ssm || tab.ssh ? bindClose : undefined}
                  onStatus={(status, sessionId) => updateStatus(id, status, sessionId)} />
              ) : tab.kind === "tunnel" ? (
                <TunnelTab tab={tab.handle} targetId={tab.targetId} method={tab.method}
                  closeState={tab.closeStatus === "closing" ? { kind: "closing" } : tab.closeStatus === "unconfirmed"
                    ? { kind: "unconfirmed", error: tab.closeError ?? "종료 확인 불가" } : undefined}
                  onShare={(targetId, tabHandle, tunnelId) => openTunnel(targetId, tab.name, tab.method, { tab: tabHandle, id: tunnelId })}
                  onOpenDesktop={openDesktop} />
              ) : tab.kind === "rdp" ? (
                <RdpTab ref={bindDesktopClose} active={visible} label={label} source={tab.source} preset={tab.preset} autoStart={tab.autoStart}
                  onReopenRoute={tab.targetId && tab.source ? () => reopenDesktopRoute(id) : undefined}
                  onSecurityMode={(tlsOnly) => setTabs(current => current.map(item => item.id === id && item.kind === "rdp"
                    ? { ...item, tlsOnly } : item))} />
              ) : tab.kind === "vnc" ? (
                <VncView ref={bindDesktopClose} active={visible} source={tab.source} preset={tab.preset} autoStart={tab.autoStart} />
              ) : tab.kind === "storage" ? (
                <div className="workspace-storage">
                  <div inert={!!tab.closeStatus}>
                    {tab.storage.kind === "compatible" ? <CompatibleStorageTab closing={!!tab.closeStatus} closeRef={bindClose} /> :
                      <StorageTab profiles={profileLoad.profiles} targetId={tab.storage.targetId} active={visible} closing={!!tab.closeStatus}
                        closeRef={bindClose} onRetest={retestStorageTarget}
                        onEditFile={({ file, transport, close }) => openEditorTab(file, transport, close, "s3")} />}
                  </div>
                </div>
              ) : (
                <EditorTab file={tab.file} active={visible && !tab.attaching} transport={tab.transport} readOnly={tab.readOnly}
                  confirmDiscard={(request) => confirmEditorDiscard(tab.source, request)}
                  focusRequested={id === focusTab} focusSeq={id === focusTab ? focusSeq : 0}
                  ref={(actions) => {
                    if (actions) editorActions.current.set(id, actions);
                    else editorActions.current.delete(id);
                  }}
                  onClose={(handle) => {
                    if (tab.attaching || handle !== tab.file.handle) return;
                    void closeTab(id);
                    releaseEditorFile(tab.source, tab.close);
                  }} />
              )}
            </div>;
          })}
        </div>
        <aside className="workspace-ai" hidden={focused || !aiOpen} aria-label="AI">
          <LocalAiPanel profilesVersion={loadVersion} sessionCandidates={aiSessions} focused={focused || !aiOpen} />
        </aside>
        {!focused && <>
          {sidebarOpen && <Splitter edge="left" label="사이드바 너비" value={left} preferred={treeWidth} min={Math.min(160, sideLimit)} max={sideLimit} onChange={setTreeWidth} />}
          {aiOpen && <Splitter edge="right" label="AI 열 너비" value={right} preferred={aiWidth} min={Math.min(200, sideLimit)} max={sideLimit} onChange={setAiWidth} />}
        </>}
      </div>
      <footer className="workspace-status">
        <span role="status" className="workspace-status-main">{current && <ProtocolIcon kind={tabIcon(current)} />}{current?.kind === "terminal"
          ? [current.targetId ? current.label : "내 PC", current.ssm ?
            `SSM · ${current.ssm.profile} / ${current.ssm.region} / ${current.ssm.instanceId}` : current.ssh ? "SSH" : current.elevation ? "sudo 승격" : "로컬", current.sessionId,
          !current.closeStatus && terminalStatus(current.status)].filter(Boolean).join(" · ")
          : current?.kind === "tunnel" ? current.label
            : current?.kind === "storage" ? `${current.label} · ${current.storage.kind === "compatible" ? "S3 호환" : "S3"}`
              : current?.kind === "rdp" || current?.kind === "vnc"
                ? `${current.label} · ${current.source ? (current.kind === "rdp" ? `SSH 터널 · ${current.tlsOnly ? "TLS 전용(표준 RDP 보안 없음)" : "NLA"}` : "SSH 터널") : (current.kind === "rdp" ? `직접 · ${current.tlsOnly ? "TLS 전용(표준 RDP 보안 없음)" : "NLA"}` : "직접 · TLS")}`
                : current ? (current.source === "local" ? "내 PC · 로컬 파일" : current.source === "sftp" ? "SFTP" : "S3") : "연결 없음"}</span>
        {closeBlock && <span role="status">{closeBlock.message}</span>}
        {closeError && <span role="alert">{closeError}</span>}
        {windowError && <span role="alert">{windowError}</span>}
        {desktopRetry?.error === windowError && <SsoLoginAction login={desktopLogin} onStart={() => { desktopLoginRetry.current = desktopRetry; }} />}
        {fileError && <span role="alert">{fileError}</span>}
        {hostRegistration && <span role={hostRegistration.kind}>{hostRegistration.message}</span>}
        {desktopOpening && <span role="status" className="workspace-status-pending">{desktopOpening.label} 경로 여는 중
          <button type="button" className="ui-button ghost" onClick={() => desktopOpening.controller.abort()}>취소</button></span>}
      </footer>
      {elevationContext && <ElevationDialog context={elevationContext} onClose={() => setElevationContext(undefined)} onSubmit={(request) => {
        setElevationContext(undefined);
        openElevation(request);
      }} />}
      {wizard && <LocalConnectionWizard {...wizard} groups={profileLoad.profiles?.groups ?? []} targets={profileLoad.profiles?.targets}
        capabilities={connections.capabilities}
        verifySshHost={connections.verifySshHost}
        test={connections.test} cancelTest={connections.cancelTest} save={connections.save} onClose={closeWizard} />}
    </main>
  );
}
