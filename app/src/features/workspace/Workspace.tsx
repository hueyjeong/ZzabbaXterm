import { useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties, type MouseEvent, type SyntheticEvent } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { LocalAiPanel } from "../ai/LocalAiPanel";
import { createLocalConnectionIpc, type SavedConnections } from "../connections/connectionIpc";
import { LocalConnectionWizard } from "../connections/LocalConnectionWizard";
import type { ConnectionClose, InitialWizardDraft, SshTransport } from "../connections/profiles";
import { subscribeLocalCliOpen } from "../editor/cliOpen";
import { EditorTab, type EditorTabActions } from "../editor/EditorTab";
import { ElevationDialog } from "../elevation/ElevationDialog";
import type { ElevationRequest } from "../elevation/elevation";
import type { ApprovedEditorFile, ConfirmEditorDiscard, EditorTransport } from "../editor/contract";
import { closeLocalFile, localEditorError, localEditorTransport, openLocalFile } from "../editor/localEditorIpc";
import { subscribeRemoteNanoOpen, type RemoteNanoFile } from "../editor/nanoOpen";
import { RdpTab, type RdpTabActions } from "../rdp/RdpTab";
import { SftpPanel, type SftpPanelActions } from "../sftp/SftpPanel";
import { sftpError } from "../sftp/sftpIpc";
import type { SsmTarget } from "../ssm/ssm";
import { CompatibleStorageTab } from "../storage/CompatibleStorageTab";
import { StorageTab } from "../storage/StorageTab";
import { TerminalView } from "../terminal/TerminalView";
import type { TerminalStatus } from "../terminal/session";
import { TunnelTab } from "../tunnels/TunnelTab";
import { tunnelIpc, type DesktopTunnelSource } from "../tunnels/tunnelIpc";
import { VncView, type VncTabActions } from "../vnc/VncView";
import { LocalTree } from "./LocalTree";
import { Splitter } from "./Splitter";
import { macTitleBar, TitleBar } from "./TitleBar";
import "./workspace.css";

type CloseStatus = "closing" | "unconfirmed";
type Split = { left: number; right: number };
type DesktopKind = "rdp" | "vnc";
type Tab = { id: number; label: string } & (
  | { kind: "terminal"; targetId?: string; remoteEditor?: true; elevation?: ElevationRequest; ssm?: SsmTarget; ssh?: SshTransport["kind"]; status: TerminalStatus; sessionId?: string; sftpSessionId?: string; closeStatus?: CloseStatus }
  | { kind: "editor"; file: ApprovedEditorFile; transport: EditorTransport; close: () => Promise<void>; source: EditorSource; attaching?: true; readOnly?: true; nano?: Pick<RemoteNanoFile, "sessionId" | "disconnect"> }
  | { kind: "tunnel"; handle: string; targetId: string; name: string; method: "ssh" | "ssm"; closeStatus?: CloseStatus; closeError?: string }
  | { kind: "rdp"; source?: DesktopTunnelSource; closeStatus?: CloseStatus }
  | { kind: "vnc"; source?: DesktopTunnelSource; closeStatus?: CloseStatus }
);
const compatibleStorage = { kind: "compatible", targetId: "s3:compatible", label: "S3 호환 HTTPS" } as const;
type StorageTarget = { closeStatus?: CloseStatus } & (
  | { kind: "aws"; targetId: string; label: string }
  | typeof compatibleStorage
);
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
    case "running": return status.notice ?? "연결됨";
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
  const desktopActions = useRef(new Map<number, RdpTabActions | VncTabActions>());
  const cliMounts = useRef(new Map<number, (mounted: boolean) => void>());
  const pendingEditorMounts = useRef(new Set<number>());
  const closingWindow = useRef(false);
  const openingFile = useRef(false);
  const [filePickerBusy, setFilePickerBusy] = useState(false);
  const [fileError, setFileError] = useState("");
  const [focusTab, setFocusTab] = useState<number | null>(1);
  const liveTabs = useRef(tabs);
  const closingTabs = useRef(new Set<number>());
  const sessionClosers = useRef(new Map<number, { confirmClose(): Promise<void> }>());
  const [closeBlock, setCloseBlock] = useState<{ kind: "picker" | "cli" | "mount"; message: string }
    | { kind: "editor"; id: number; message: string }>();
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
  const [focused, setFocused] = useState(false);
  const [windowError, setWindowError] = useState("");
  const [closeError, setCloseError] = useState("");
  const [loadVersion, setLoadVersion] = useState(0);
  const [profileLoad, setProfileLoad] = useState<ProfileLoad>({ kind: "loading" });
  const [hostRegistration, setHostRegistration] = useState<{ kind: "status" | "alert"; message: string }>();
  const [wizard, setWizard] = useState<WizardRequest>();
  const [elevationContext, setElevationContext] = useState<Readonly<{ targetId?: string; target?: string; account?: string }>>();
  const [storedAuth, setStoredAuth] = useState(false);
  const [treeWidth, setTreeWidth] = useState(280);
  const [aiWidth, setAiWidth] = useState(360);
  const [bodyWidth, setBodyWidth] = useState(1200);
  const workspace = useRef<HTMLElement>(null);
  const body = useRef<HTMLDivElement>(null);
  const activeButton = useRef<HTMLButtonElement>(null);
  const sideLimit = Math.min(640, Math.floor(bodyWidth * 0.32));
  const left = Math.min(treeWidth, sideLimit);
  const right = Math.min(aiWidth, sideLimit);

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
      const removePreview = () => setTabs(current => current.filter(tab => tab.id !== id));
      const { promise: mounted, resolve } = Promise.withResolvers<boolean>();
      cliMounts.current.set(id, resolve);
      const abort = () => {
        resolve(false);
        removePreview();
      };
      signal.addEventListener("abort", abort, { once: true });
      setTabs(current => [...current, { id, label: file.label, kind: "editor", file, source, transport, close, attaching: true, nano }]);
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

  function openTab(target?: SavedConnections["targets"][number]["profile"], options?: { readonly remoteEditor?: true }) {
    if (target?.defaultMethod === "s3") {
      openStorageTarget({ kind: "aws", targetId: target.id, label: target.displayName });
      return;
    }
    const id = nextTab.current++;
    setTabs((current) => [...current, {
      id, label: target?.displayName ?? `로컬 ${id}`, kind: "terminal", targetId: target?.id,
      remoteEditor: options?.remoteEditor,
      ssm: target?.defaultMethod === "ssm" ? target.ssm : undefined,
      ssh: target?.ssh?.transport.kind, status: { kind: "opening" },
    }]);
    selectTab(id);
  }

  function openElevation(request: ElevationRequest) {
    const id = nextTab.current++;
    const name = request.program.split("/").filter(Boolean).pop() ?? request.program;
    setTabs((current) => [...current, {
      id, label: `승격 · ${name}`, kind: "terminal", elevation: request, status: { kind: "opening" },
    }]);
    selectTab(id);
  }

  function openDesktop(kind: DesktopKind, source?: DesktopTunnelSource) {
    const id = nextTab.current++;
    setTabs(current => [...current, { id, label: `${kind.toUpperCase()} ${id}`, kind, source }]);
    selectTab(id);
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
        }
        throw error;
      }
    }
    setTabs(current => [...current, tab]);
    selectTab(id);
  }

  function openStorageTarget(target: StorageTarget) {
    setStorageTargets(current => current.some(open => open.targetId === target.targetId)
      ? current : [...current, target]);
    setPinnedStorage(target.targetId);
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
    selectTab(id);
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
    }
    if (tab.kind === "tunnel" || (tab.kind === "terminal" && (tab.ssm || tab.ssh))) {
      closingTabs.current.add(id);
      setTabs(current => current.map(item => item.id === id && (item.kind === "terminal" || item.kind === "tunnel")
        ? { ...item, closeStatus: "closing" } : item));
      try {
        if (tab.kind === "tunnel") {
          await tunnelIpc.close(tab.handle);
        } else {
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
        }
      } catch (error) {
        setTabs(current => current.map(item => {
          if (item.id !== id || (item.kind !== "terminal" && item.kind !== "tunnel")) return item;
          return item.kind === "tunnel" ? { ...item, closeStatus: "unconfirmed", closeError: String(error) }
            : { ...item, closeStatus: "unconfirmed" };
        }));
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
  const openAwsStorage = storageTargets.filter(target => target.kind === "aws");
  const savedStorageTargets = profileLoad.profiles?.targets.filter(({ profile }) =>
    (profile.defaultMethod === "s3" || profile.defaultMethod === "ssm") &&
    !storageTargets.some(target => target.targetId === profile.id)) ?? [];
  const layout = {
    "--tree-width": `${left}px`, "--ai-width": `${right}px`,
    "--center-left": `${splitPosition}fr`, "--center-right": `${100 - splitPosition}fr`,
  } as CSSProperties;

  function blockClosingInput(event: SyntheticEvent) {
    if (!closingWindow.current) return;
    event.preventDefault();
    event.stopPropagation();
  }

  return (
    <main ref={workspace} className={`workspace${macTitleBar ? " workspace-mac" : ""}${focused ? " workspace-focused" : ""}`} style={layout}
      onClickCapture={blockClosingInput} onMouseDownCapture={blockClosingInput}
      onKeyDownCapture={blockClosingInput} onPointerMoveCapture={blockClosingInput}
      onFocusCapture={(event) => { if (!event.target.closest(".terminal-screen, .monaco-editor")) setFocusTab(null); }}>
      <TitleBar focused={focused} onToggle={() => setFocused(!focused)} onWindowError={setWindowError}
        onOpenRdp={() => openDesktop("rdp")} onOpenVnc={() => openDesktop("vnc")}>
        {tabs.map((tab) => {
          const { id, label, kind } = tab;
          const closeStatus = kind === "editor" ? undefined : tab.closeStatus;
          return <div className="workspace-tab" key={id}>
            <button ref={id === active ? activeButton : undefined} aria-pressed={id === active}
              disabled={kind === "editor" && tab.attaching} onClick={() => selectTab(id)}>{label}</button>
            <button aria-label={`${label} ${closeStatus === "unconfirmed" ? "종료 다시 확인" : "닫기"}`}
              disabled={closeStatus === "closing" || (kind === "editor" && tab.attaching)} onClick={() => {
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
          {profileLoad.kind === "error" && <div className="connection-status">
            <div role="alert">연결 불러오기 실패: {profileLoad.error}</div>
            <button onClick={reloadProfiles}>다시 불러오기</button>
          </div>}
          <LocalTree profiles={profileLoad.profiles} onOpen={openTab}
            onLocalElevate={storedAuth ? () => setElevationContext({}) : undefined}
            onElevate={storedAuth ? (target) => {
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
            }} />
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
            const bindDesktopClose = (actions: RdpTabActions | VncTabActions | null) => {
              if (actions) desktopActions.current.set(id, actions);
              else desktopActions.current.delete(id);
            };
            const pane = id === visibleSplit?.left ? "left" : id === visibleSplit?.right ? "right" : undefined;
            const visible = id === active || pane !== undefined;
            const desktop = tab.kind === "rdp" || tab.kind === "vnc";
            // 분할 상태에서 화면 출력은 유지하되, 사용자 입력은 포커스된 터미널에만 전달한다.
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
                if (visible && event.button === 0) selectTab(id);
              },
              onMouseUpCapture: blockTerminalInput, onMouseMoveCapture: blockTerminalInput,
              onWheelCapture: blockTerminalInput, onAuxClickCapture: blockTerminalInput, onContextMenuCapture: blockTerminalInput,
            } : undefined;
            return <div className={`workspace-session${desktop ? " workspace-desktop" : ""}`} key={id}
              id={pane && `${paneId}-${pane}`} data-pane={pane} hidden={!visible} inert={!visible} {...terminalInput}
              onFocusCapture={(event) => {
                if (pane && id !== activeId.current && event.target.closest(".terminal-screen")) selectTab(id, false);
              }}>
              {tab.kind === "terminal" ? (
                <TerminalView active={visible} focusRequested={id === focusTab} label={label} targetId={tab.targetId}
                  elevation={tab.elevation}
                  remote={!!tab.ssm} ssh={!!tab.ssh} remoteEditor={tab.remoteEditor} closing={!!tab.closeStatus} closeRef={tab.ssm || tab.ssh ? bindClose : undefined}
                  onStatus={(status, sessionId) => updateStatus(id, status, sessionId)} />
              ) : tab.kind === "tunnel" ? (
                <TunnelTab tab={tab.handle} targetId={tab.targetId} method={tab.method}
                  closeState={tab.closeStatus === "closing" ? { kind: "closing" } : tab.closeStatus === "unconfirmed"
                    ? { kind: "unconfirmed", error: tab.closeError ?? "종료 확인 불가" } : undefined}
                  onShare={(targetId, tabHandle, tunnelId) => openTunnel(targetId, tab.name, tab.method, { tab: tabHandle, id: tunnelId })}
                  onOpenDesktop={openDesktop} />
              ) : tab.kind === "rdp" ? (
                <RdpTab ref={bindDesktopClose} active={id === active} label={label} source={tab.source} />
              ) : tab.kind === "vnc" ? (
                <VncView ref={bindDesktopClose} active={id === active} source={tab.source} />
              ) : (
                <EditorTab file={tab.file} active={id === active && !tab.attaching} transport={tab.transport} readOnly={tab.readOnly}
                  confirmDiscard={(request) => confirmEditorDiscard(tab.source, request)}
                  focusRequested={id === focusTab}
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
          {split && <Splitter edge="center" label="중앙 터미널 분할" controls={`${paneId}-left`}
            value={splitPosition} min={20} max={80} onChange={setSplitPosition} />}
        </div>
        <aside className="workspace-ai" hidden={focused} aria-label="오른쪽 도구">
          <LocalAiPanel profilesVersion={loadVersion} sessionCandidates={aiSessions} focused={focused} topTabs={{
            item: {
              id: "files", label: "파일",
              content: <>
                <div className="workspace-file-connections">
                  <select aria-label="파일 연결" value={pinnedStorage} onChange={event => {
                    const targetId = event.target.value;
                    if (targetId === compatibleStorage.targetId) {
                      openStorageTarget(compatibleStorage);
                      return;
                    }
                    const saved = savedStorageTargets.find(({ profile }) => profile.id === targetId);
                    if (saved) openStorageTarget({ kind: "aws", targetId, label: saved.profile.displayName });
                    else setPinnedStorage(targetId);
                  }}>
                    <option value="">활성 SSH 따라가기</option>
                    <option value={compatibleStorage.targetId}>{compatibleStorage.label}</option>
                    {openAwsStorage.length > 0 && <optgroup label="열린 S3">
                      {openAwsStorage.map(({ targetId, label }) => <option key={targetId} value={targetId}>{label}</option>)}
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
                {storageTargets.map(({ kind, targetId, closeStatus }) => {
                  const visible = showFiles && !focused && targetId === pinnedStorage;
                  const bindClose = (actions: { confirmClose(): Promise<void> } | null) => {
                    if (actions) storageClosers.current.set(targetId, actions);
                    else storageClosers.current.delete(targetId);
                  };
                  return <div className="workspace-storage" key={targetId} hidden={!visible} inert={!visible}>
                    {closeStatus && <p className="workspace-file-status" role={closeStatus === "closing" ? "status" : "alert"}>
                      {closeStatus === "closing" ? "종료 중" : "종료 확인 불가"}
                    </p>}
                    <div inert={!!closeStatus}>
                      {kind === "compatible" ? <CompatibleStorageTab closing={!!closeStatus} closeRef={bindClose} /> :
                        <StorageTab profiles={profileLoad.profiles} targetId={targetId} active={visible} closing={!!closeStatus}
                          closeRef={bindClose} onRetest={retestStorageTarget}
                          onEditFile={({ file, transport, close }) => openEditorTab(file, transport, close, "s3")} />}
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
            `SSM · ${current.ssm.profile} / ${current.ssm.region} / ${current.ssm.instanceId}` : current.ssh ? "SSH" : current.elevation ? "sudo 승격" : "로컬", current.sessionId,
          !current.closeStatus && terminalStatus(current.status)].filter(Boolean).join(" · ")
          : current?.kind === "tunnel" ? current.label
            : current?.kind === "rdp" || current?.kind === "vnc"
              ? `${current.label} · ${current.source ? (current.kind === "rdp" ? "SSH 터널 · NLA" : "SSH 터널") : (current.kind === "rdp" ? "직접 · NLA" : "직접 · TLS")}`
              : current ? (current.source === "local" ? "내 PC · 로컬 파일" : current.source === "sftp" ? "SFTP" : "S3") : "연결 없음"}</span>
        {closeBlock && <span role="status">{closeBlock.message}</span>}
        {closeError && <span role="alert">{closeError}</span>}
        {windowError && <span role="alert">{windowError}</span>}
        {fileError && <span role="alert">{fileError}</span>}
        {hostRegistration && <span role={hostRegistration.kind}>{hostRegistration.message}</span>}
      </footer>
      {elevationContext && <ElevationDialog context={elevationContext} onClose={() => setElevationContext(undefined)} onSubmit={(request) => {
        setElevationContext(undefined);
        openElevation(request);
      }} />}
      {wizard && <LocalConnectionWizard {...wizard} groups={profileLoad.profiles?.groups ?? []}
        capabilities={connections.capabilities}
        verifySshHost={connections.verifySshHost}
        test={connections.test} cancelTest={connections.cancelTest} save={connections.save} onClose={closeWizard} />}
    </main>
  );
}
