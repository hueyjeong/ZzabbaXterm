// @vitest-environment jsdom
import type { EventCallback } from "@tauri-apps/api/event";
import { act, StrictMode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "../../App";
import { editor } from "../editor/monaco";
import { contextMenu, control, dblclick, fileRow } from "../files/testing";
import type { SavedConnections } from "../connections/connectionIpc";
import type { ConnectionRequest, ConnectionTestDraft, ConnectionTestRun } from "../connections/profiles";
import * as rdp from "../rdp/RdpTab";
import * as vnc from "../vnc/VncView";

const native = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
  fit: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({
  isTauri: () => true,
  invoke: native.invoke,
  convertFileSrc: (path: string, protocol: string) => `${protocol}:${path}`,
  Channel: class {
    constructor(public onmessage: (message: unknown) => void) { }
    receive = (message: unknown) => this.onmessage(message);
  },
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: native.listen }));
vi.mock("@tauri-apps/api/webview", () => ({ getCurrentWebview: () => ({ label: "main" }) }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit = native.fit; } }));
vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    private field!: HTMLTextAreaElement;
    private screen!: HTMLElement;
    loadAddon() { }
    attachCustomKeyEventHandler() { }
    open(screen: HTMLElement) {
      this.screen = screen;
      this.field = screen.appendChild(document.createElement("textarea"));
    }
    focus() { this.field.focus(); }
    blur() { this.field.blur(); }
    onKey() { return { dispose() { } }; }
    onData(input: (data: string) => void) {
      this.field.addEventListener("input", (event) => {
        if (event instanceof InputEvent && event.data) input(event.data);
      });
      for (const type of ["keydown", "paste"]) {
        this.field.addEventListener(type, () => input(type));
      }
      return { dispose() { } };
    }
    write(bytes: Uint8Array, parsed: () => void) {
      this.screen.append(new TextDecoder().decode(bytes));
      parsed();
    }
    dispose() { this.field.remove(); }
  },
}));

let host: HTMLDivElement;
let root: Root;
let viewportWidth: number;
let observeWorkspace: () => void;
let observeTabs: () => void;
const channels = new Map<string, (message: unknown) => void>();
const starts = new Map<string, Promise<void>>();
const emptyStore: SavedConnections = { version: 1, groups: [], targets: [] };
let loadProfiles: () => Promise<SavedConnections>;
let runConnectionCommand: (command: string, args: unknown) => Promise<unknown>;
let closeTerminal: (id: string) => Promise<unknown>;
let runDiscovery: (args: unknown) => Promise<unknown>;
let runStorage: (command: string, args: unknown) => Promise<unknown>;
let runSftp: (command: string, args: unknown) => Promise<unknown>;
let runTunnel: (command: string, args: unknown) => Promise<unknown>;
let receiveCliOpen: EventCallback<{ requestId: string }> | undefined;
let receiveNanoOpen: EventCallback<{ offerId: string; sessionId: string }> | undefined;
let receiveNanoDisconnect: typeof receiveNanoOpen;
let runNanoEditor: (command: string, args: unknown) => Promise<unknown>;
let receiveWindowClose: EventCallback<{ requestId: string }> | undefined;
let respondToWindowClose: () => Promise<boolean>;
let pendingWindowClose: () => Promise<string | null>;
let runCliEditor: (command: string, args: { attached?: boolean }) => Promise<unknown>;
let openFile: () => Promise<unknown>;
let saveFile: () => Promise<unknown>;

beforeEach(async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("CSS", { escape: (value: string) => value.replace(/[^\w-]/g, "\\$&") });
  vi.stubGlobal("matchMedia", vi.fn(() => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() })));
  const canvas = HTMLCanvasElement.prototype;
  vi.spyOn(canvas, "getContext").mockImplementation(() => ({
    clearRect: vi.fn(), fillRect: vi.fn(), beginPath: vi.fn(), moveTo: vi.fn(), lineTo: vi.fn(), stroke: vi.fn(),
  }) as unknown as CanvasRenderingContext2D);
  viewportWidth = 1200;
  const dialog = window.HTMLDialogElement.prototype;
  // jsdom에는 네이티브 모달 동작이 없다.
  for (const [name, method] of Object.entries({
    showModal(this: HTMLDialogElement) { this.setAttribute("open", ""); },
    close(this: HTMLDialogElement) { this.removeAttribute("open"); },
  })) {
    Object.defineProperty(dialog, name, { configurable: true, value: method });
  }
  vi.stubGlobal("ResizeObserver", class {
    constructor(private readonly callback: () => void) { }
    observe(element: HTMLElement) {
      if (element.classList.contains("workspace-body")) observeWorkspace = this.callback;
      if (element.matches(".workspace-tabs")) observeTabs = this.callback;
    }
    disconnect() { }
  });
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function(this: HTMLElement) {
    return this.closest("[hidden]") ? 0 : viewportWidth;
  });
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function(this: HTMLElement) {
    return this.closest("[hidden]") ? 0 : 400;
  });
  channels.clear();
  starts.clear();
  loadProfiles = async () => emptyStore;
  runConnectionCommand = async (command) => { throw new Error(`예상하지 않은 연결 명령: ${command}`); };
  closeTerminal = async () => null;
  runDiscovery = async () => { throw new Error("예상하지 않은 SSM 검색"); };
  runStorage = async command => { throw new Error(`예상하지 않은 스토리지 명령: ${command}`); };
  runSftp = async command => { throw new Error(`예상하지 않은 SFTP 명령: ${command}`); };
  runTunnel = async command => { throw new Error(`예상하지 않은 터널 명령: ${command}`); };
  receiveCliOpen = undefined;
  receiveNanoOpen = undefined;
  receiveNanoDisconnect = undefined;
  runNanoEditor = async command => { throw new Error(`예상하지 않은 nano 편집 명령: ${command}`); };
  receiveWindowClose = undefined;
  respondToWindowClose = async () => false;
  pendingWindowClose = async () => null;
  native.listen.mockImplementation(async (event, receive) => {
    if (event === "editor-local-cli-open") {
      receiveCliOpen = receive;
      return () => { receiveCliOpen = undefined; };
    }
    if (event === "editor-remote-nano-open") {
      receiveNanoOpen = receive;
      return () => { receiveNanoOpen = undefined; };
    }
    if (event === "editor-remote-nano-disconnected") {
      receiveNanoDisconnect = receive;
      return () => { receiveNanoDisconnect = undefined; };
    }
    if (event === "workspace-close-requested") {
      receiveWindowClose = receive;
      return () => { receiveWindowClose = undefined; };
    }
    return () => { };
  });
  runCliEditor = async command => { throw new Error(`예상하지 않은 CLI 편집 명령: ${command}`); };
  openFile = async () => ({ status: "cancelled" });
  saveFile = async () => null;
  native.invoke.mockImplementation(async (command, args) => {
    if (command === "connections_load") return loadProfiles();
    if (command === "ai_models") return ["local:latest"];
    if (command === "ai_targets" || command === "ai_decisions") return [];
    if (command.startsWith("connection_") || ["verify_ssh_host", "commit_ssh_host", "cancel_ssh_host", "register_saved_ssh_host"].includes(command)) return runConnectionCommand(command, args);
    if (command === "ssm_discovery") return runDiscovery(args);
    if (command.startsWith("storage_")) return runStorage(command, args);
    if (command.startsWith("editor_sftp_")) return runSftp(command, args);
    if (command.startsWith("tunnel_")) return runTunnel(command, args);
    if (command === "editor_local_cli_pending") return [];
    if (command.startsWith("editor_local_cli_")) return runCliEditor(command, args);
    if (command === "editor_remote_nano_pending") return [];
    if (command.startsWith("editor_remote_nano_")) return runNanoEditor(command, args);
    if (command === "editor_local_open") return openFile();
    if (command === "editor_local_save") return saveFile();
    if (command === "workspace_window" && args.action === "respondToClose") return respondToWindowClose();
    if (command === "workspace_window" && args.action === "pendingClose") return pendingWindowClose();
    if (command === "terminal_start") return starts.get(args.id);
    if (command === "terminal_close") return closeTerminal(args.id);
    if (command !== "terminal_open") return;
    const number = channels.size + 10;
    const id = args.remoteEditor ? String(number) : `pty-${number}`;
    channels.set(id, args.output.receive);
    return id;
  });
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
  await act(async () => { root.render(<App />); });
});

type RunArgs = { run: ConnectionTestRun };

afterEach(async () => {
  await act(async () => { root.unmount(); });
  host.remove();
  vi.restoreAllMocks();
  vi.resetAllMocks();
  vi.unstubAllGlobals();
});

function find<T extends HTMLElement>(selector: string): T {
  const element = host.querySelector<T>(selector);
  expect(element, selector).not.toBeNull();
  return element!;
}

function button(name: string) {
  return [...host.querySelectorAll("button")].find((item) => (item.getAttribute("aria-label") ?? item.textContent) === name);
}

async function chooseMethod(value: string) {
  const tile = host.querySelector<HTMLButtonElement>(`dialog [role="radiogroup"][aria-label="접속 방식"] [role="radio"][value="${value}"]`);
  expect(tile, value).not.toBeNull();
  await act(async () => { tile!.click(); });
  expect(tile!.getAttribute("aria-checked")).toBe("true");
}

async function press(name: string) {
  const control = button(name);
  expect(control, name).toBeDefined();
  await act(async () => { control!.click(); });
}

/** 열린 메뉴(role="menu") 가운데 마지막 것을 찾는다. 이름을 주면 aria-label이 같은 메뉴만 찾는다. */
function openMenu(label?: string) {
  const menus = [...host.querySelectorAll<HTMLElement>('[role="menu"]')]
    .filter(menu => label === undefined || menu.getAttribute("aria-label") === label);
  expect(menus.length, `메뉴 ${label ?? ""}`).toBeGreaterThan(0);
  return menus[menus.length - 1];
}

function menuItemNames(menu = openMenu()) {
  return [...menu.querySelectorAll('[role="menuitem"]')].map(item => item.textContent);
}

async function chooseMenuItem(name: string, menu = openMenu()) {
  const item = [...menu.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')].find(entry => entry.textContent === name);
  expect(item, name).toBeDefined();
  await act(async () => { item!.click(); });
}

/** 단추(aria-label 또는 요소)로 메뉴를 열고 그 메뉴를 돌려준다. */
async function showMenu(control: string | HTMLElement) {
  const element = typeof control === "string" ? button(control) : control;
  expect(element, String(control)).toBeDefined();
  await act(async () => { element!.click(); });
  return openMenu();
}

async function closeMenu(menu = openMenu()) {
  await key(menu.querySelector<HTMLElement>('[role="menuitem"]') ?? menu, "Escape");
}

/** 제목줄 `새 탭 종류` 메뉴의 항목을 실행한다. */
async function openNewTab(name: "파일 열기…" | "RDP 연결" | "VNC 연결" | "S3 호환 스토리지" | "새 연결 프로필…" | "로컬 터미널") {
  await chooseMenuItem(name, await showMenu("새 탭 종류"));
}

/** 탭 이름으로 탭 문맥 메뉴를 연다. */
async function showTabMenu(label: string) {
  const tab = button(label)?.closest<HTMLElement>(".workspace-tab");
  expect(tab, label).toBeDefined();
  await act(async () => { tab!.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true })); });
  return openMenu(`${label} 탭 메뉴`);
}

async function tabMenuAction(label: string, action: string) {
  await chooseMenuItem(action, await showTabMenu(label));
}

/** 탭이 속한 탭 그룹 번호(왼쪽부터 1)다. */
function tabGroup(label: string) {
  const head = button(label)?.closest(".workspace-group-head");
  return head ? [...host.querySelectorAll(".workspace-group-head")].indexOf(head) + 1 : 0;
}

/** 세션 화면이 놓인 탭 그룹 번호(왼쪽부터 1)다. */
function sessionGroup(element: Element) {
  const session = element.closest<HTMLElement>(".workspace-session");
  return session ? (Number(session.style.gridColumn) + 1) / 2 : 0;
}

/** 탭 종료 상태 문구(종료 중·종료 확인 불가)다. */
function tabCloseStatus(label: string) {
  return button(label)?.closest(".workspace-tab")?.querySelector(".workspace-tab-close-status")?.textContent?.trim();
}

/** 연결 트리에서 이름이 같은 대상 행을 찾는다. */
function treeTarget(label: string) {
  const row = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]:not([aria-expanded])')]
    .find(item => item.querySelector(":scope > .tree-row > span")?.textContent === label);
  expect(row, label).toBeDefined();
  return row!;
}

/** 연결 트리 대상의 문맥 메뉴 항목을 실행한다. */
async function treeMenuAction(label: string, action: string) {
  await key(treeTarget(label), "ContextMenu");
  await chooseMenuItem(action);
}

/** 파일 목록 행의 문맥 메뉴(파일 작업·폴더 작업) 항목을 실행한다. */
async function fileMenuAction(title: string, action: string, scope: ParentNode = host) {
  await contextMenu(fileRow(scope, title));
  await chooseMenuItem(action);
}

const sftpSidebar = () => find('.workspace-sidebar[aria-label="SFTP 파일"]');

/** 키보드 입력이 가는 탭(포커스 그룹의 활성 탭) 이름이다. */
function focusedTabLabel() {
  return host.querySelector('.workspace-tabstrip[data-focused] .workspace-tab-select[aria-pressed="true"]')?.textContent;
}

async function key(element: HTMLElement, value: string) {
  await act(async () => { element.dispatchEvent(new KeyboardEvent("keydown", { key: value, bubbles: true })); });
}

async function type(field: HTMLTextAreaElement, value: string, inserted = value) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(field, value);
    field.dispatchEvent(new InputEvent("input", { bubbles: true, data: inserted }));
  });
}

function calls(operation: string) {
  return native.invoke.mock.calls.flatMap(([command, args]) => command === `terminal_${operation}` ? [args] : []);
}

function connectionCalls(command: string) {
  return native.invoke.mock.calls.filter(([name]) => name === command);
}

async function changeInput(label: string, value: string) {
  const field = [...host.querySelectorAll("label")].find((item) => item.textContent?.trim() === label)?.querySelector("input");
  expect(field, label).toBeDefined();
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(field, value);
    field!.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function restartWorkspace() {
  await act(async () => { root.unmount(); });
  root = createRoot(host);
  await act(async () => { root.render(<App />); });
}

async function requestWindowClose(requestId: string) {
  expect(receiveWindowClose).toBeDefined();
  await act(async () => { void receiveWindowClose?.({ event: "workspace-close-requested", id: 1, payload: { requestId } }); });
}

const closeReplies = () => connectionCalls("workspace_window").map(([, args]) => args)
  .filter(args => args.action === "respondToClose");

const status = () => find("footer.workspace-status").textContent;
const tree = () => find('[role="tree"]');
async function openSecond() {
  const group = find('[role="treeitem"][aria-expanded]');
  await act(async () => { group.focus(); });
  await key(group, "ArrowRight");
  expect(document.activeElement).toBe(find('[role="treeitem"]:not([aria-expanded])'));
  await key(document.activeElement as HTMLElement, "Enter");
}

describe("작업 공간 창 닫기", () => {
  it("제목줄은 네이티브 닫기만 요청하고 깨끗한 창은 바로 준비 상태를 보낸다", async () => {
    await act(async () => { root.unmount(); });
    native.listen.mockClear();
    root = createRoot(host);
    await act(async () => { root.render(<StrictMode><App /></StrictMode>); });
    expect(connectionCalls("workspace_window").filter(([, args]) => args.action === "pendingClose")).toHaveLength(2);
    native.invoke.mockClear();
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const response = Promise.withResolvers<boolean>();
    respondToWindowClose = () => response.promise;
    await press("닫기");
    expect(connectionCalls("workspace_window")).toEqual([["workspace_window", { action: "close" }]]);
    expect(find("main").hasAttribute("inert")).toBe(false);
    await requestWindowClose("clean");
    await requestWindowClose("clean");
    expect(native.listen.mock.calls.filter(([event]) => event === "workspace-close-requested")).toEqual([
      ["workspace-close-requested", expect.any(Function), { target: { kind: "Webview", label: "main" } }],
    ]);
    expect(confirm).not.toHaveBeenCalled();
    expect(closeReplies()).toEqual([{ action: "respondToClose", requestId: "clean", decision: { kind: "ready" } }]);
    expect(find("main").hasAttribute("inert")).toBe(true);
    await act(async () => response.resolve(true));
    expect(find("main").hasAttribute("inert")).toBe(true);
  });

  it("리스너 등록 전에 유실된 닫기를 조회하고 같은 ID의 이벤트는 중복 처리하지 않는다", async () => {
    await act(async () => { root.unmount(); });
    native.invoke.mockClear();
    pendingWindowClose = async () => {
      expect(receiveWindowClose).toBeDefined();
      return "pending";
    };
    root = createRoot(host);
    await act(async () => { root.render(<App />); });
    await requestWindowClose("pending");
    expect(connectionCalls("workspace_window")).toEqual([
      ["workspace_window", { action: "pendingClose" }],
      ["workspace_window", { action: "respondToClose", requestId: "pending", decision: { kind: "ready" } }],
    ]);
    expect(find("main").hasAttribute("inert")).toBe(false);
  });

  it("대기 중인 닫기 조회가 실패해도 등록된 리스너로 다음 요청을 처리한다", async () => {
    await act(async () => { root.unmount(); });
    pendingWindowClose = async () => { throw "조회 실패"; };
    root = createRoot(host);
    await act(async () => { root.render(<App />); });
    expect(status()).toContain("창 닫기 연결 실패: 조회 실패");
    await requestWindowClose("next");
    expect(closeReplies()).toEqual([{ action: "respondToClose", requestId: "next", decision: { kind: "ready" } }]);
    expect(status()).not.toContain("조회 실패");
  });

  it("포커스 해제로 확정된 편집을 폐기 확인 목록에 포함한다", async () => {
    openFile = async () => ({ status: "opened", handle: "blur-file", name: "입력.txt", text: "원본" });
    await openNewTab("파일 열기…");
    const model = editor.getModels().find(item => item.uri.query === "blur-file")!;
    const tab = button("입력.txt")!;
    await act(async () => { tab.focus(); });
    expect(document.activeElement).toBe(tab);
    tab.addEventListener("blur", () => model.setValue("확정된 편집"), { once: true });
    await requestWindowClose("blur");
    expect(closeReplies()).toEqual([{
      action: "respondToClose", requestId: "blur", decision: { kind: "discard", files: ["입력.txt"] },
    }]);
    expect(model.getValue()).toBe("확정된 편집");
    expect(document.activeElement).toBe(tab);
  });

  it("숨은 미저장 파일과 저장 결과 미확인 파일을 함께 보내고 네이티브 취소 후 편집을 유지한다", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const response = Promise.withResolvers<boolean>();
    respondToWindowClose = () => response.promise;
    openFile = async () => ({ status: "opened", handle: "dirty-file", name: "미저장.txt", text: "원본" });
    await openNewTab("파일 열기…");
    const dirty = editor.getModels().find(model => model.uri.query === "dirty-file")!;
    await act(async () => dirty.setValue("편집본"));
    openFile = async () => ({ status: "opened", handle: "clean-file", name: "저장됨.txt", text: "원본" });
    await openNewTab("파일 열기…");
    openFile = async () => ({ status: "opened", handle: "uncertain-file", name: "저장 미확인.txt", text: "원본" });
    await openNewTab("파일 열기…");
    expect(confirm).not.toHaveBeenCalled();
    const uncertain = editor.getModels().find(model => model.uri.query === "uncertain-file")!;
    const version = uncertain.getAlternativeVersionId();
    await act(async () => uncertain.pushEditOperations(null, [{
      range: { startLineNumber: 1, startColumn: 1, endLineNumber: 1, endColumn: 1 }, text: "수정 ",
    }], () => null));
    const recoveryPath = String.raw`C:\작업 폴더\한글 복구.txt`;
    saveFile = async () => { throw { kind: "publicationUnconfirmed", recoveryPath }; };
    const activeEditor = find('.editor-tab[aria-label="저장 미확인.txt"]:not([hidden])');
    const save = [...activeEditor.querySelectorAll("button")].find(button => (button.getAttribute("aria-label") ?? button.textContent) === "저장");
    expect(save?.disabled).toBe(false);
    await act(async () => { save?.click(); });
    expect(connectionCalls("editor_local_save")).toEqual([["editor_local_save", { handle: "uncertain-file", text: "수정 원본" }]]);
    expect(activeEditor.querySelector("output")?.textContent).toContain(`저장 결과 미확인: 복구 파일: ${recoveryPath}`);
    await act(async () => uncertain.undo());
    expect(uncertain.getAlternativeVersionId()).toBe(version);
    expect(find('.editor-tab[aria-label="미저장.txt"]').hidden).toBe(true);
    const previousFocus = document.activeElement;

    await requestWindowClose("discard");
    await requestWindowClose("discard");
    expect(confirm).not.toHaveBeenCalled();
    expect(closeReplies()).toEqual([{
      action: "respondToClose", requestId: "discard", decision: { kind: "discard", files: ["미저장.txt", "저장 미확인.txt"] },
    }]);
    expect(find("main").hasAttribute("inert")).toBe(true);
    await act(async () => response.resolve(false));
    expect(document.activeElement).toBe(previousFocus);
    expect(status()).not.toContain("닫기 실패");
    expect(button("저장 미확인.txt")?.getAttribute("aria-pressed")).toBe("true");
    expect(dirty.getValue()).toBe("편집본");
    expect(uncertain.getValue()).toBe("원본");
    expect(find("main").hasAttribute("inert")).toBe(false);
    expect(connectionCalls("editor_local_close")).toEqual([]);
    await press("미저장.txt");
    expect(find('.editor-tab[aria-label="미저장.txt"]').hidden).toBe(false);
  });

  it.each(["종료 확인 불가", "창 닫기 요청이 만료되었습니다"])("닫기 요청이 거절되면 편집과 포커스를 복원한다: %s", async (error) => {
    const response = Promise.withResolvers<boolean>();
    respondToWindowClose = () => response.promise;
    openFile = async () => ({ status: "opened", handle: "keep-file", name: "유지.txt", text: "원본" });
    await openNewTab("파일 열기…");
    const model = editor.getModels().find(item => item.uri.query === "keep-file")!;
    await act(async () => model.setValue("편집본"));
    const previousFocus = document.activeElement;
    await requestWindowClose("approve");
    expect(find("main").hasAttribute("inert")).toBe(true);
    await press("새 탭 종류");
    expect(host.querySelector('[role="menu"]')).toBeNull();
    await press("유지.txt 닫기");
    expect(connectionCalls("editor_local_open")).toHaveLength(1);
    expect(button("유지.txt")).toBeDefined();

    await act(async () => response.reject(error));
    expect(status()).toContain(`닫기 실패: ${error}`);
    expect(find("main").hasAttribute("inert")).toBe(false);
    expect(document.activeElement).toBe(previousFocus);
    expect(model.getValue()).toBe("편집본");
    await press("저장");
    expect(connectionCalls("editor_local_save")).toEqual([["editor_local_save", { handle: "keep-file", text: "편집본" }]]);
    respondToWindowClose = async () => true;
    await requestWindowClose("retry");
    expect(closeReplies()).toEqual([
      { action: "respondToClose", requestId: "approve", decision: { kind: "discard", files: ["유지.txt"] } },
      { action: "respondToClose", requestId: "retry", decision: { kind: "ready" } },
    ]);
  });

  it("열린 연결 추가 모달도 승인 중에는 잠기고 닫기 실패 후 다시 조작할 수 있다", async () => {
    const response = Promise.withResolvers<boolean>();
    respondToWindowClose = () => response.promise;
    await press("연결 추가");
    const dialog = find<HTMLDialogElement>('dialog[aria-label="연결 추가"]');
    await requestWindowClose("wizard-open");
    expect(dialog.hasAttribute("inert")).toBe(true);
    await act(async () => { dialog.dispatchEvent(new Event("cancel", { cancelable: true })); });
    expect(dialog.open).toBe(true);
    await act(async () => response.reject("종료 실패"));
    expect(dialog.hasAttribute("inert")).toBe(false);
    await press("취소");
    expect(host.querySelector('dialog[aria-label="연결 추가"]')).toBeNull();
  });

  it("파일 선택 중에는 창 닫기를 보류하고 파일 선택은 계속한다", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const picker = Promise.withResolvers<unknown>();
    native.invoke.mockRejectedValueOnce("최소화 거부");
    await press("최소화");
    openFile = () => picker.promise;
    await openNewTab("파일 열기…");
    await requestWindowClose("picker");
    expect(status()).toContain("파일 선택 중 · 창 닫기 보류");
    expect(confirm).not.toHaveBeenCalled();
    expect(closeReplies()).toEqual([{ action: "respondToClose", requestId: "picker", decision: { kind: "blocked" } }]);
    expect(find("main").hasAttribute("inert")).toBe(false);
    await act(async () => picker.resolve({ status: "opened", handle: "picked", name: "선택.txt", text: "원본" }));
    expect(button("선택.txt")?.getAttribute("aria-pressed")).toBe("true");
    expect(status()).not.toContain("창 닫기 보류");
    expect(status()).toContain("최소화 실패: 최소화 거부");
  });
});

describe("중앙 원격 화면", () => {
  function dispatchTrusted(target: HTMLElement, event: Event) {
    // jsdom의 공개 dispatchEvent는 항상 isTrusted를 지운다. 실제 컴포넌트의 신뢰 검사는 유지한다.
    const impl = Object.getOwnPropertySymbols(event).find(symbol => symbol.description === "impl");
    if (!impl) throw new Error("jsdom 이벤트 구현을 찾을 수 없습니다");
    const state = Reflect.get(event, impl);
    state.isTrusted = true;
    Reflect.get(target, impl)._dispatch(state);
  }

  it.each([
    ["rdp", true], ["rdp", false], ["vnc", true], ["vnc", false],
  ] as const)("%s 엔진의 프레임·입력·탭 종료가 수명을 따른다 (첫 종료 확인: %s)", async (protocol, confirmed) => {
    const label = `${protocol.toUpperCase()} 3`;
    const identity = { sessionId: "b".repeat(32), generation: 1 };
    const owner = { tab: identity.sessionId, document: 1 };
    const proof = "a".repeat(32);
    vi.stubGlobal(`__ZZABBA_${protocol.toUpperCase()}_DOCUMENT__`, { proof });
    const fetchPacket = vi.fn<(url: string) => Promise<Pick<Response, "ok" | "headers" | "arrayBuffer" | "json">>>();
    vi.stubGlobal("fetch", fetchPacket);
    vi.stubGlobal("ImageData", class {
      constructor(readonly data: Uint8ClampedArray, readonly width: number, readonly height: number) { }
    });
    const draw = vi.fn();
    vi.mocked(HTMLCanvasElement.prototype.getContext).mockReturnValue({ putImageData: draw } as unknown as CanvasRenderingContext2D);
    const pendingRdp = Promise.withResolvers<void>();
    const invoke = native.invoke.getMockImplementation()!;
    let output: { onmessage(ticket: string): void } | undefined;
    let closeAttempts = 0;
    native.invoke.mockImplementation((command, args) => {
      if (command === "rdp_prepare") return Promise.resolve(identity);
      if (command === "vnc_create") return Promise.resolve(owner);
      if (command === `${protocol}_connect`) {
        output = args.output;
        return protocol === "rdp" ? pendingRdp.promise : Promise.resolve();
      }
      if (command === `${protocol}_close`) {
        const closed = ++closeAttempts > 1 || confirmed;
        if (protocol === "vnc") return Promise.resolve(closed);
        if (!closed) return Promise.reject(new Error("RDP 종료 확인 불가"));
        pendingRdp.resolve();
      }
      return invoke(command, args);
    });
    let ticketSequence = 0;
    async function deliver(packet: object | ArrayBuffer) {
      const binary = packet instanceof ArrayBuffer;
      const ticket = (++ticketSequence).toString(16).padStart(32, "0");
      fetchPacket.mockResolvedValueOnce({
        ok: true, headers: new Headers({ "content-type": binary ? "application/octet-stream" : "application/json" }),
        arrayBuffer: async () => {
          if (!binary) throw new Error("바이너리 패킷이 아닙니다");
          return packet;
        },
        json: async () => JSON.parse(JSON.stringify(packet)),
      });
      await act(async () => { output!.onmessage(ticket); });
      expect(fetchPacket).toHaveBeenLastCalledWith(`${protocol}-packet:${proof}-${ticket}`, expect.anything());
    }

    await openSecond();
    const terminal = find<HTMLTextAreaElement>('section[aria-label="로컬 2"] textarea');
    await openNewTab(`${protocol.toUpperCase() as "RDP" | "VNC"} 연결`);
    expect(button(label)?.getAttribute("aria-pressed")).toBe("true");
    expect(status()).toBe(`${label} · 직접 · ${protocol === "rdp" ? "NLA" : "TLS"}`);
    const pane = find(".workspace-session.workspace-desktop");
    const form = pane.querySelector("form")!;
    const server = form.querySelector("input")!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(server, "desktop.example.invalid");
      server.dispatchEvent(new Event("input", { bubbles: true }));
      form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    });
    expect(output).toBeUndefined();
    await act(async () => form.requestSubmit());
    expect(connectionCalls(`${protocol}_connect`)).toEqual([[
      `${protocol}_connect`, protocol === "rdp"
        ? { id: identity, draft: { host: "desktop.example.invalid", port: 3389, tlsOnly: false }, output }
        : { owner, draft: { host: "desktop.example.invalid", port: 5900, password: true }, output },
      { headers: { [`x-zzabba-${protocol}-document`]: proof } },
    ]]);
    if (protocol === "vnc") await deliver({ identity, kind: "started", serial: 0 });
    const canvas = find<HTMLCanvasElement>('canvas[aria-label="원격 화면"]');
    expect(canvas.tabIndex).toBe(-1);
    expect(pane.textContent).toContain("첫 화면 대기 중");
    expect(pane.textContent).not.toContain("연결됨");

    if (protocol === "rdp") {
      await deliver({ sequence: 1, kind: "frame", displayGeneration: 1, full: true, frameSequence: 1, width: 1, height: 1, x: 0, y: 0, w: 1, h: 1 });
      const chunk = new ArrayBuffer(16);
      new DataView(chunk).setBigUint64(0, 1n, true);
      new Uint8Array(chunk, 12).set([1, 2, 3, 255]);
      await deliver(chunk);
      await deliver({ sequence: 2, kind: "display", displayGeneration: 1 });
    } else {
      await deliver({ identity, kind: "waiting", value: 1, serial: 1 });
      const chunk = new ArrayBuffer(37);
      const header = new DataView(chunk);
      header.setUint8(0, 1);
      header.setBigUint64(1, 1n, true);
      header.setBigUint64(9, 1n, true);
      for (const offset of [17, 19, 25, 27]) header.setUint16(offset, 1, true);
      new Uint8Array(chunk, 33).set([1, 2, 3, 255]);
      await deliver(chunk);
    }
    expect(draw).toHaveBeenCalledOnce();
    expect(draw).toHaveBeenCalledWith(expect.objectContaining({ data: Uint8ClampedArray.of(1, 2, 3, 255) }), 0, 0);
    expect(pane.textContent).toContain("연결됨 · 1 × 1");

    const input = () => new KeyboardEvent("keydown", { code: "KeyA", key: "a", bubbles: true, cancelable: true });
    const keyCalls = () => connectionCalls(`${protocol}_input`).filter(([, args]) => args.input.action.kind === "key");
    await act(async () => dispatchTrusted(canvas, input()));
    expect(keyCalls()).toHaveLength(0);
    await act(async () => {
      canvas.focus();
      canvas.dispatchEvent(input());
    });
    expect(keyCalls()).toHaveLength(0);
    await act(async () => dispatchTrusted(canvas, input()));
    expect(keyCalls()).toEqual([[`${protocol}_input`, expect.objectContaining({
      input: expect.objectContaining({ action: expect.objectContaining({ kind: "key", down: true }) }),
    }), expect.anything()]]);

    await press("로컬 2");
    expect(document.activeElement).toBe(terminal);
    expect(canvas.closest("[hidden][inert]")).not.toBeNull();
    await act(async () => dispatchTrusted(canvas, input()));
    expect(keyCalls()).toHaveLength(1);
    expect(menuItemNames(await showTabMenu("로컬 1"))).toEqual(["오른쪽으로 분리", "같은 연결 새 탭", "같은 연결 오른쪽에 열기", "닫기"]);
    await chooseMenuItem("오른쪽으로 분리");
    expect([tabGroup("로컬 2"), tabGroup(label), tabGroup("로컬 1")]).toEqual([1, 1, 2]);
    expect(sessionGroup(terminal)).toBe(1);
    expect(terminal.closest("[hidden], [inert]")).toBeNull();
    const boundary = find(".workspace-group-splitter");
    await press(label);
    expect(find(".workspace-group-splitter")).toBe(boundary);
    expect(sessionGroup(pane)).toBe(1);
    expect(find('section[aria-label="로컬 1"]').closest("[hidden], [inert]")).toBeNull();
    expect(terminal.closest("[hidden][inert]")).not.toBeNull();
    expect(pane.querySelector("canvas")).toBe(canvas);
    expect(pane.textContent).toContain("연결됨 · 1 × 1");
    expect(connectionCalls(`${protocol}_connect`)).toHaveLength(1);
    expect(connectionCalls(`${protocol}_close`)).toEqual([]);
    await act(async () => {
      canvas.focus();
      dispatchTrusted(canvas, input());
    });
    expect(keyCalls()).toHaveLength(2);

    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const closeArgs = protocol === "rdp" ? { id: identity } : { owner, generation: null };
    const closeOptions = { headers: { [`x-zzabba-${protocol}-document`]: proof } };
    if (!confirmed) await press("로컬 2");
    await press(`${label} 닫기`);
    expect(connectionCalls(`${protocol}_close`)).toEqual([[`${protocol}_close`, closeArgs, closeOptions]]);
    if (!confirmed) {
      expect(find(".workspace-session.workspace-desktop")).toBe(pane);
      expect(pane.hidden).toBe(false);
      const tab = button(label)!;
      expect(tab.getAttribute("aria-pressed")).toBe("true");
      expect(tab.parentElement?.querySelector('[role="alert"]')?.textContent).toBe("종료 확인 불가");
      await press(`${label} 종료 다시 확인`);
      expect(connectionCalls(`${protocol}_close`)).toEqual([
        [`${protocol}_close`, closeArgs, closeOptions],
        [`${protocol}_close`, protocol === "rdp" ? closeArgs : { ...closeArgs, retry: true }, closeOptions],
      ]);
    }
    expect(confirm).not.toHaveBeenCalled();
    expect(pane.isConnected).toBe(false);
    expect(document.activeElement).toBe(terminal);
    expect(sessionGroup(terminal)).toBe(1);
    expect(tabGroup("로컬 2")).toBe(1);
    expect(find(".workspace-group-splitter")).toBe(boundary);
    expect(host.querySelectorAll(".workspace-group-head")).toHaveLength(2);
    expect(calls("open")).toHaveLength(2);
    expect(calls("close")).toEqual([]);
  });
});

describe("CLI 로컬 파일 열기", () => {
  const requestId = "1".repeat(32);
  const handle = "a".repeat(32);
  const file = { status: "opened", handle, name: "CLI 문서.txt", text: "CLI 원본" };
  const preview = '.editor-tab[aria-label="CLI 문서.txt"]';

  async function openFromCli(respond: () => Promise<void>) {
    runCliEditor = async (command, args) => {
      if (command === "editor_local_cli_claim") return file;
      if (args.attached) await respond();
    };
    await act(async () => receiveCliOpen!({ event: "editor-local-cli-open", id: 1, payload: { requestId } }));
  }

  it("승인된 창 닫기 중에 도착한 CLI 파일은 부착하지 않는다", async () => {
    const response = Promise.withResolvers<boolean>();
    respondToWindowClose = () => response.promise;
    await requestWindowClose("before-cli");
    await openFromCli(() => { throw new Error("부착이 차단되지 않았습니다"); });
    expect(connectionCalls("editor_local_cli_respond")).toEqual([
      ["editor_local_cli_respond", { requestId, handle, attached: false }],
    ]);
    expect(button(file.name)).toBeUndefined();
    await act(async () => response.resolve(true));
  });

  it("마운트한 편집 탭의 정확한 핸들을 부착하고 해당 탭을 닫을 때만 파일을 닫는다", async () => {
    const attached = Promise.withResolvers<void>();
    let textAtAttach: string | undefined;
    await openSecond();
    await press("로컬 1");
    await openFromCli(() => {
      textAtAttach = editor.getModels().find(item => item.uri.query === handle)?.getValue();
      return attached.promise;
    });

    expect(textAtAttach).toBe(file.text);
    expect(connectionCalls("editor_local_cli_respond")).toEqual([
      ["editor_local_cli_respond", { requestId, handle, attached: true }],
    ]);
    await requestWindowClose("cli-mount");
    expect(status()).toContain("CLI 편집 탭 연결 중 · 창 닫기 보류");
    expect(closeReplies()).toEqual([{ action: "respondToClose", requestId: "cli-mount", decision: { kind: "blocked" } }]);
    expect(find(preview).hidden).toBe(true);
    expect(button("로컬 1")?.getAttribute("aria-pressed")).toBe("true");
    await press("로컬 2");
    const input = find<HTMLTextAreaElement>('section[aria-label="로컬 2"] textarea');
    expect(document.activeElement).toBe(input);
    await type(input, "READY");
    expect(calls("input")).toEqual([{ id: "pty-11", bytes: Array.from(new TextEncoder().encode("READY")) }]);

    await act(async () => attached.resolve());
    expect(button(file.name)?.getAttribute("aria-pressed")).toBe("true");
    expect(find(preview).hidden).toBe(false);
    expect(status()).toBe("내 PC · 로컬 파일");
    await press("로컬 2");
    expect(find('section[aria-label="로컬 2"] textarea')).toBe(input);
    expect(input.value).toBe("READY");
    await press("로컬 1 닫기");
    expect(connectionCalls("editor_local_close")).toEqual([]);
    await press(`${file.name} 닫기`);
    expect(connectionCalls("editor_local_close")).toEqual([["editor_local_close", { handle }]]);
    expect(button(file.name)).toBeUndefined();
    expect(calls("close")).toEqual([{ id: "pty-10" }]);
    expect(button("로컬 2")?.getAttribute("aria-pressed")).toBe("true");
  });

  it.each(["수신 종료", "부착 응답 실패"])("%s 시 임시 편집 탭만 사라진다", async (failure) => {
    const attached = Promise.withResolvers<void>();
    await openSecond();
    const input = find<HTMLTextAreaElement>('section[aria-label="로컬 2"] textarea');
    await openFromCli(() => attached.promise);
    expect(button(file.name)?.disabled).toBe(true);

    if (failure === "수신 종료") {
      await act(async () => { window.dispatchEvent(new Event("pagehide")); });
      expect(button(file.name)).toBeUndefined();
      await act(async () => attached.resolve());
    } else {
      await act(async () => attached.reject(new Error("응답 연결 끊김")));
    }

    expect(button(file.name)).toBeUndefined();
    expect(editor.getModels().some(item => item.uri.query === handle)).toBe(false);
    expect(button("로컬 1")).toBeDefined();
    expect(button("로컬 2")?.getAttribute("aria-pressed")).toBe("true");
    expect(document.activeElement).toBe(input);
    await type(input, "STILL_READY");
    expect(calls("input")).toEqual([{ id: "pty-11", bytes: Array.from(new TextEncoder().encode("STILL_READY")) }]);
    expect(calls("close")).toEqual([]);
    expect(connectionCalls("editor_local_close")).toEqual([]);
  });
});

describe("L02", () => {
  it("셸이 준비되기 전에는 입력을 받지 않고 준비되면 선택한 터미널로 포커스가 이동한다", async () => {
    const start = Promise.withResolvers<void>();
    starts.set("pty-11", start.promise);
    await openSecond();
    const input = find<HTMLTextAreaElement>('section[aria-label="로컬 2"] textarea');
    const screen = input.parentElement!;
    expect(status()).toContain("pty-11 · 여는 중");
    expect(screen.hasAttribute("inert")).toBe(true);
    expect(document.activeElement).not.toBe(input);
    // jsdom에서 합성 입력으로 inert를 우회해도 준비 전 데이터는 PTY로 가지 않는다.
    await type(input, "EARLY");
    expect(calls("input")).toEqual([]);

    await act(async () => { start.resolve(); });
    expect(status()).toContain("pty-11 · 연결됨");
    expect(screen.hasAttribute("inert")).toBe(false);
    expect(document.activeElement).toBe(input);
    await type(input, "READY");
    expect(calls("input")).toEqual([{ id: "pty-11", bytes: Array.from(new TextEncoder().encode("READY")) }]);
  });

  it.each(['.workspace-chat textarea', '.window-control.minimize', '.splitter-left'])
    ("셸을 여는 동안 %s에 옮긴 포커스는 준비 후에도 그대로다", async (selector) => {
      const start = Promise.withResolvers<void>();
      starts.set("pty-11", start.promise);
      await openSecond();
      const control = find(selector);
      await act(async () => { control.focus(); });
      await act(async () => { start.resolve(); });
      expect(status()).toContain("pty-11 · 연결됨");
      expect(document.activeElement).toBe(control);
      await press("로컬 2");
      expect(document.activeElement).toBe(find('section[aria-label="로컬 2"] textarea'));
    });

  it("창 축소와 집중 보기 복귀에서 선택 탭을 노출한다", async () => {
    await openSecond();
    const list = find(".workspace-tabs");
    const selected = find('.workspace-tab-select[aria-pressed="true"]').parentElement!;
    let listWidth = 200;
    vi.spyOn(list, "getBoundingClientRect").mockImplementation(() => new DOMRect(0, 0, listWidth, 38));
    vi.spyOn(selected, "getBoundingClientRect").mockImplementation(() => new DOMRect(90 - list.scrollLeft, 0, 90, 38));
    listWidth = 130;
    observeTabs();
    expect(list.scrollLeft).toBe(50);
    listWidth = 200;
    await press("집중 보기");
    list.scrollLeft = 0;
    listWidth = 110;
    await press("기본 보기");
    expect(list.scrollLeft).toBe(70);
    expect(status()).toContain("pty-11");
  });

  it("사용자가 넓힌 폭은 조절 시점의 실제 상한까지만 저장한다", async () => {
    await act(async () => { viewportWidth = 660; observeWorkspace(); });
    const left = find('.splitter-left');
    const right = find('.splitter-right');
    await key(left, "Home");
    await key(left, "End");
    right.setPointerCapture = vi.fn();
    await key(right, "Home");
    await act(async () => {
      right.dispatchEvent(new PointerEvent("pointerdown", { pointerId: 1, clientX: 460, bubbles: true }));
      right.dispatchEvent(new PointerEvent("pointermove", { pointerId: 1, clientX: 300, bubbles: true }));
      right.dispatchEvent(new PointerEvent("lostpointercapture", { pointerId: 1, bubbles: true }));
    });
    await act(async () => { viewportWidth = 1200; observeWorkspace(); });
    expect([left, right].map((part) => part.getAttribute("aria-valuenow"))).toEqual(["211", "211"]);
  });

  it("좁은 창에서 같은 드래그의 시작점으로 돌아오면 기존 트리 선호 폭을 되찾는다", async () => {
    const left = find(".splitter-left");
    left.setPointerCapture = vi.fn();
    async function pointer(type: string, clientX: number) {
      await act(async () => {
        left.dispatchEvent(new PointerEvent(type, { pointerId: 1, clientX, bubbles: true }));
      });
    }
    await pointer("pointerdown", 280);
    await pointer("pointermove", 340);
    await pointer("lostpointercapture", 340);
    expect(left.getAttribute("aria-valuenow")).toBe("340");

    await act(async () => { viewportWidth = 660; observeWorkspace(); });
    expect(left.getAttribute("aria-valuenow")).toBe("211");
    await pointer("pointerdown", 211);
    await pointer("pointermove", 200);
    expect(left.getAttribute("aria-valuenow")).toBe("200");
    await pointer("pointermove", 211);
    expect(left.getAttribute("aria-valuenow")).toBe("211");
    await pointer("lostpointercapture", 211);

    await act(async () => { viewportWidth = 1200; observeWorkspace(); });
    expect(left.getAttribute("aria-valuenow")).toBe("340");
  });

  it("집중 보기와 탭 왕복은 두 PTY와 패널 선호값, 미제출 입력, 채팅 초안을 유지한다", async () => {
    expect(status()).toContain("pty-10");
    await openSecond();
    expect(status()).toContain("pty-11");
    const panel = find<HTMLElement>('[aria-label="로컬 2"]:is(section)');
    const input = panel.querySelector("textarea")!;
    await type(input, "echo B_READY");
    const draft = find<HTMLTextAreaElement>('textarea[aria-label="메시지 입력"]');
    await type(draft, "CHAT_ONLY");
    const treeSplitter = find('[role="separator"][aria-label="사이드바 너비"]');
    treeSplitter.setPointerCapture = vi.fn();
    await act(async () => {
      treeSplitter.dispatchEvent(new PointerEvent("pointerdown", { pointerId: 1, clientX: 280, bubbles: true }));
      treeSplitter.dispatchEvent(new PointerEvent("pointermove", { pointerId: 1, clientX: 340, bubbles: true }));
      treeSplitter.dispatchEvent(new PointerEvent("lostpointercapture", { pointerId: 1, bubbles: true }));
    });
    await key(find('[role="separator"][aria-label="AI 열 너비"]'), "ArrowLeft");
    expect(host.querySelector('[role="separator"][aria-label="AI 채팅 높이"]')).toBeNull();
    const values = () => ["사이드바 너비", "AI 열 너비"].map((label) =>
      find(`[role="separator"][aria-label="${label}"]`).getAttribute("aria-valuenow"));
    expect(values()).toEqual(["340", "380"]);
    await act(async () => { viewportWidth = 660; observeWorkspace(); });
    expect(values()).toEqual(["211", "211"]);
    const footer = find("footer");
    await press("집중 보기");
    expect(tree().closest("[hidden]")).not.toBeNull();
    expect(find("footer")).toBe(footer);
    expect(status()).toContain("pty-11");
    await press("로컬 1");
    expect(status()).toContain("pty-10");
    expect(panel.closest("[inert][hidden]")).not.toBeNull();
    await press("로컬 2");
    await press("기본 보기");
    expect(values()).toEqual(["211", "211"]);
    await act(async () => { viewportWidth = 1200; observeWorkspace(); });
    expect(values()).toEqual(["340", "380"]);
    expect(tree().closest("[hidden]")).toBeNull();
    expect(panel.querySelector("textarea")).toBe(input);
    expect(input.value).toBe("echo B_READY");
    expect(draft.value).toBe("CHAT_ONLY");
    expect(calls("open")).toHaveLength(2);
    expect(calls("close")).toHaveLength(0);
    expect(calls("input")).toEqual([{ id: "pty-11", bytes: Array.from(new TextEncoder().encode("echo B_READY")) }]);
    expect(connectionCalls("workspace_window")).toEqual([["workspace_window", { action: "pendingClose" }]]);
  });

  it("연 터미널만 좌우로 표시하고, 사람 포커스와 AI 조회 대상은 따로 유지한다", async () => {
    const target = { targetId: "local-a", generation: "7" };
    const invoke = native.invoke.getMockImplementation()!;
    native.invoke.mockImplementation((command: string, args: unknown) => {
      if (command === "ai_targets") return Promise.resolve([{ ...target, label: "조회 A" }]);
      if (command === "ai_select") return Promise.resolve(true);
      return invoke(command, args);
    });
    await press("모델 및 대상 새로고침");
    const aiTarget = find<HTMLButtonElement>('button[aria-label^="조회 대상"]');
    expect(aiTarget.getAttribute("aria-label")).toBe("조회 대상: 선택 안 함");
    expect(menuItemNames(await showMenu(aiTarget))).toEqual(["조회 A (local-a)"]);
    await chooseMenuItem("조회 A (local-a)");
    const aiValue = aiTarget.getAttribute("aria-label");
    expect(aiValue).toBe("조회 대상: 조회 A (local-a)");

    await openSecond();
    const left = find<HTMLTextAreaElement>('section[aria-label="로컬 1"] textarea');
    const right = find<HTMLTextAreaElement>('section[aria-label="로컬 2"] textarea');
    // 탭이 둘인 그룹의 분할 단추는 활성 탭(로컬 2)을 오른쪽 새 그룹으로 옮긴다.
    await press("오른쪽으로 분할");
    await press("로컬 1");
    await type(left, "echo ");
    expect([tabGroup("로컬 1"), tabGroup("로컬 2")]).toEqual([1, 2]);
    expect([sessionGroup(left), sessionGroup(right)]).toEqual([1, 2]);
    expect(right.closest("[hidden], [inert]")).toBeNull();
    expect(document.activeElement).toBe(left);
    expect(focusedTabLabel()).toBe("로컬 1");
    await type(right, "UNFOCUSED");
    expect(calls("input")).toEqual([{ id: "pty-10", bytes: Array.from(new TextEncoder().encode("echo ")) }]);
    const boundary = find('[role="separator"][aria-label="탭 그룹 1·2 경계"]');
    expect(boundary.getAttribute("aria-orientation")).toBe("vertical");
    await act(async () => boundary.focus());
    await key(boundary, "ArrowRight");
    expect(boundary.getAttribute("aria-valuenow")).toBe("55");
    expect(document.activeElement).toBe(boundary);
    // 두 그룹 머리의 합친 폭은 50~450px이다.
    const [leftHead, rightHead] = host.querySelectorAll(".workspace-group-head");
    vi.spyOn(leftHead, "getBoundingClientRect").mockReturnValue(new DOMRect(50, 0, 198, 38));
    vi.spyOn(rightHead, "getBoundingClientRect").mockReturnValue(new DOMRect(253, 0, 197, 38));
    boundary.setPointerCapture = vi.fn();
    await act(async () => {
      boundary.dispatchEvent(new PointerEvent("pointerdown", { pointerId: 1, clientX: 250, bubbles: true }));
      boundary.dispatchEvent(new PointerEvent("pointermove", { pointerId: 1, clientX: 310, bubbles: true }));
      boundary.dispatchEvent(new PointerEvent("lostpointercapture", { pointerId: 1, bubbles: true }));
    });
    expect(boundary.getAttribute("aria-valuenow")).toBe("65");

    await act(async () => right.focus());
    await type(right, "RIGHT");
    expect(document.activeElement).toBe(right);
    expect(button("로컬 2")?.getAttribute("aria-pressed")).toBe("true");
    expect(focusedTabLabel()).toBe("로컬 2");
    expect(status()).toContain("pty-11");
    await press("집중 보기");
    await press("기본 보기");
    expect(find(".workspace-group-splitter")).toBe(boundary);
    expect(boundary.getAttribute("aria-valuenow")).toBe("65");
    await press("로컬 1");
    expect(document.activeElement).toBe(left);
    await tabMenuAction("로컬 1", "모든 그룹 합치기");
    expect(host.querySelectorAll(".workspace-group-head")).toHaveLength(1);
    expect(boundary.isConnected).toBe(false);
    expect(right.closest("[hidden][inert]")).not.toBeNull();
    await press("로컬 2");
    expect(document.activeElement).toBe(right);
    expect(left.value).toBe("echo ");
    expect(calls("open")).toHaveLength(2);
    expect(calls("close")).toEqual([]);
    expect(calls("input")).toEqual([
      { id: "pty-10", bytes: Array.from(new TextEncoder().encode("echo ")) },
      { id: "pty-11", bytes: Array.from(new TextEncoder().encode("RIGHT")) },
    ]);
    expect(find('button[aria-label^="조회 대상"]').getAttribute("aria-label")).toBe(aiValue);
    expect(connectionCalls("ai_select")).toEqual([["ai_select", { target }]]);
    expect(connectionCalls("ai_set_policy")).toEqual([]);
  });

  it("숨은 탭의 출력과 종료는 활성 탭의 입력, 포커스, 상태를 바꾸지 않는다", async () => {
    await openSecond();
    const active = find<HTMLTextAreaElement>('section[aria-label="로컬 2"] textarea');
    expect(document.activeElement).toBe(active);
    await type(active, "echo ");
    native.fit.mockClear();
    await act(async () => {
      const bytes = new Uint8Array(new ArrayBuffer(6));
      bytes.set(new TextEncoder().encode("A_DONE"));
      channels.get("pty-10")!(bytes.buffer);
      channels.get("pty-10")!({ kind: "exited", value: 7 });
    });
    expect(document.activeElement).toBe(active);
    expect(native.fit).not.toHaveBeenCalled();
    expect(active.value).toBe("echo ");
    await type(active, `${active.value}B_READY`, "B_READY");
    expect(calls("input")).toEqual(["echo ", "B_READY"].map((text) => ({
      id: "pty-11", bytes: Array.from(new TextEncoder().encode(text)),
    })));
    expect(status()).toContain("pty-11");
    await press("로컬 1");
    expect(find('section[aria-label="로컬 1"]').textContent).toContain("A_DONE");
    expect(status()).toContain("pty-10 · 종료 코드 7");
    await press("로컬 1 닫기");
    expect(status()).toContain("pty-11");
    await press("로컬 2 닫기");
    expect(status()).toBe("연결 없음");
    expect(calls("close")).toEqual([{ id: "pty-11" }]);
  });

  it("창 버튼은 키보드로 도달할 수 있고 닫기 오류가 나면 탭을 그대로 둔다", async () => {
    native.invoke.mockImplementationOnce(async () => { throw "거부됨"; });
    await press("닫기");
    expect(status()).toContain("닫기 실패: 거부됨");
    native.invoke.mockClear();
    const controls = [...find("header").querySelectorAll("button")].slice(-4);
    expect(controls.map((item) => item.getAttribute("aria-label"))).toEqual(["집중 보기", "최소화", "최대화/복원", "닫기"]);
    expect(controls.map((item) => item.tabIndex)).toEqual([0, 0, 0, 0]);
    for (const name of ["최소화", "최대화/복원", "닫기"]) await press(name);
    expect(native.invoke.mock.calls).toEqual(["minimize", "toggleMaximize", "close"].map((action) => ["workspace_window", { action }]));
    expect(status()).toContain("pty-10");
    expect(calls("close")).toHaveLength(0);
  });

  it("창 이동은 핸들과 여백에서만 시작하며 오류를 표시한다", async () => {
    native.invoke.mockClear();
    for (const selector of ['.workspace-tab button', '.workspace-new-tab button', '.workspace-view-actions button', '.workspace-mode', '.window-control.close']) {
      await act(async () => {
        find(selector).dispatchEvent(new MouseEvent("mousedown", { bubbles: true, detail: 1 }));
      });
    }
    expect(native.invoke).not.toHaveBeenCalled();
    native.invoke.mockImplementationOnce(async () => { throw "거부됨"; });
    await act(async () => {
      find('.workspace-handle').dispatchEvent(new MouseEvent("mousedown", { bubbles: true, detail: 1 }));
    });
    expect(status()).toContain("창 이동 실패: 거부됨");
    await act(async () => {
      find('.workspace-drag-gap').dispatchEvent(new MouseEvent("mousedown", { bubbles: true, detail: 2 }));
      find('.workspace-drag-gap').dispatchEvent(new MouseEvent("dblclick", { bubbles: true, detail: 2 }));
    });
    expect(native.invoke.mock.calls).toEqual([
      ["workspace_window", { action: "drag" }],
      ["workspace_window", { action: "toggleMaximize" }],
    ]);
  });
});

describe("C01 저장된 로컬 연결", () => {
  it("동명 그룹의 동명 대상을 검색해 Enter·더블클릭·문맥 메뉴로 정확한 저장 ID를 연다", async () => {
    loadProfiles = async () => ({
      version: 1,
      groups: ["alpha", "beta"].map((id) => ({ revision: 1, profile: { id, displayName: "회사 개발" } })),
      targets: ["alpha", "beta"].map((groupId) => ({
        revision: 1, profile: {
          id: `work-${groupId}`, groupId, displayName: "작업 셸", defaultMethod: "local",
          local: { shell: "powerShell", startDirectory: `C:\\test\\${groupId}` },
        }
      })),
    });
    await restartWorkspace();
    const search = find<HTMLInputElement>('[aria-label="연결 검색"]');
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(search, "작업");
      search.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const groups = [...tree().querySelectorAll<HTMLElement>('[aria-expanded]')];
    expect(groups.map((group) => group.firstElementChild?.textContent)).toEqual(["회사 개발alpha", "회사 개발beta"]);
    await act(async () => { groups[1].focus(); });
    await key(groups[1], "ArrowRight");
    expect(document.activeElement?.textContent).toBe("작업 셸");
    await key(document.activeElement as HTMLElement, "Enter");
    expect(calls("open").slice(-1)[0]).toMatchObject({ targetId: "work-beta", rows: 24, cols: 80 });
    const alpha = groups[0].querySelector<HTMLElement>('[role="treeitem"]')!;
    await act(async () => { alpha.dispatchEvent(new MouseEvent("dblclick", { bubbles: true })); });
    expect(calls("open").slice(-1)[0]).toMatchObject({ targetId: "work-alpha" });
    await key(alpha, "ContextMenu");
    expect([...find('[role="menu"]').querySelectorAll('[role="menuitem"]')].map((item) => item.textContent)).toEqual(["로컬"]);
    await press("로컬");
    expect(calls("open").filter((args) => args.targetId).map((args) => args.targetId))
      .toEqual(["work-beta", "work-alpha", "work-alpha"]);
    expect(calls("start").slice(-3).map((args) => args.id)).toEqual(["pty-12", "pty-13", "pty-14"]);
  });

  it("빈 저장소에서 + 마법사로 시험한 뒤 Rust 저장 응답 이후에만 다시 읽고 재시작 시 표시한다", async () => {
    expect([...tree().querySelectorAll('[role="treeitem"] > .tree-row > span:not([aria-hidden], .tree-open, .tree-via)')].map(item => item.textContent)).toEqual(["로컬", "내 PC"]);
    expect(tree().querySelector(".tree-open")?.getAttribute("aria-label")).toBe("열린 탭 1개");
    const save = Promise.withResolvers<void>();
    const reload = Promise.withResolvers<SavedConnections>();
    let store = emptyStore;
    let tested: ConnectionTestDraft | undefined;
    loadProfiles = () => reload.promise;
    runConnectionCommand = async (command, args) => {
      if (command === "connection_test_start") {
        tested = args as ConnectionTestDraft;
        return { draftId: tested.draftId, draftVersion: tested.draftVersion, runId: "run-approved" };
      }
      if (command === "connection_test_wait") {
        const { run } = args as RunArgs;
        return { ...run, sessionId: "probe-pty" };
      }
      if (command === "connection_save") {
        const { request } = args as { request: ConnectionRequest };
        expect(request).toEqual({
          draft: tested?.draft,
          run: { draftId: tested?.draftId, draftVersion: tested?.draftVersion, runId: "run-approved" },
        });
        await save.promise;
        const { group, target } = request.draft;
        store = { version: 1, groups: [{ revision: 1, profile: group }], targets: [{ revision: 1, profile: target }] };
        return null;
      }
      throw command;
    };
    await press("연결 추가");
    expect(find("dialog[open]").textContent).toContain("대상");
    await changeInput("그룹", "회사 개발");
    await changeInput("표시명", "작업 셸");
    await press("다음");
    await changeInput("시작 디렉터리", "C:\\c01-test");
    await press("다음");
    expect(find("dialog").textContent).toContain("현재 사용자");
    await press("다음");
    expect(button("저장")?.disabled).toBe(true);
    await press("연결 테스트");
    expect(find("dialog").textContent).toContain("테스트 성공");
    await press("저장");
    expect(connectionCalls("connections_load")).toHaveLength(1);
    expect(tree().textContent).not.toContain("작업 셸");
    await act(async () => { save.resolve(); });
    expect(host.querySelector("dialog")).toBeNull();
    expect(connectionCalls("connections_load")).toHaveLength(2);
    expect(tree().textContent).not.toContain("작업 셸");
    await act(async () => { reload.resolve(store); });
    expect(tree().textContent).toContain("회사 개발작업 셸");
    loadProfiles = async () => store;
    await restartWorkspace();
    expect(tree().textContent).toContain("회사 개발작업 셸");
    expect(calls("open").every((args) => !("targetId" in args))).toBe(true);
  });

  it("로드 오류를 빈 목록과 구분하고 성공한 초안의 경로를 바꿔 실패 후 취소하면 트리를 보존한다", async () => {
    loadProfiles = async () => { throw "설정 읽기 실패"; };
    await restartWorkspace();
    expect(find(".workspace-tree [role=alert]").textContent).toContain("설정 읽기 실패");
    loadProfiles = async () => emptyStore;
    await press("다시 불러오기");
    const before = tree().textContent;
    const loads = connectionCalls("connections_load").length;
    let failed = false;
    runConnectionCommand = async (command, args) => {
      if (command === "connection_test_start") {
        const { draft, draftId, draftVersion } = args as ConnectionTestDraft;
        failed = draft.target.defaultMethod === "local" && draft.target.local.startDirectory.endsWith("missing");
        return { draftId, draftVersion, runId: failed ? "missing-run" : "good-run" };
      }
      if (command === "connection_test_wait") {
        if (failed) throw "시작 디렉터리를 찾을 수 없습니다";
        const { run } = args as RunArgs;
        return { ...run, sessionId: "test-pty" };
      }
      if (command === "connection_test_cancel") return null;
      throw command;
    };
    await press("연결 추가");
    await changeInput("그룹", "회사 개발");
    await changeInput("표시명", "작업 셸");
    await press("다음");
    await changeInput("시작 디렉터리", "C:\\c01-test");
    await press("다음");
    await press("다음");
    await press("연결 테스트");
    expect(button("저장")?.disabled).toBe(false);
    await press("이전");
    await press("이전");
    await changeInput("시작 디렉터리", "C:\\c01-test\\missing");
    await press("다음");
    await press("다음");
    expect(button("저장")?.disabled).toBe(true);
    await press("연결 테스트");
    expect(find("dialog [role=alert]").textContent).toBe("시작 디렉터리를 찾을 수 없습니다");
    expect(find("dialog").textContent).toContain("C:\\c01-test\\missing");
    await press("취소");
    expect(host.querySelector("dialog")).toBeNull();
    expect(tree().textContent).toBe(before);
    expect(connectionCalls("connections_load")).toHaveLength(loads);
    expect(connectionCalls("connection_save")).toHaveLength(0);
    expect(connectionCalls("connection_test_cancel").map(([, args]) => args.run.runId)).toEqual(["good-run", "missing-run"]);
  });
});

describe("C02 SSM 연결", () => {
  async function select(label: string, value: string) {
    const control = [...find("dialog").querySelectorAll("label")]
      .find((item) => item.firstChild?.textContent?.trim() === label)?.querySelector("select");
    if (!control) throw new Error(label);
    expect(control.disabled).toBe(false);
    await act(async () => {
      control.value = value;
      control.dispatchEvent(new Event("change", { bubbles: true }));
    });
    return control;
  }

  const node = { profile: "company", region: "ap-northeast-2", instanceId: "i-00000001" };

  it("저장된 SSM 터널 탭에는 공식 포트 전달 차단 상태만 표시한다", async () => {
    loadProfiles = async () => ({
      ...emptyStore,
      groups: [{ revision: 1, profile: { id: "work", displayName: "작업" } }],
      targets: [{ revision: 1, profile: { id: "node", groupId: "work", displayName: "관리 노드", defaultMethod: "ssm", ssm: node } }],
    });
    runTunnel = async command => {
      if (command === "tunnel_tab_open") return "ssm-tunnel";
      if (command === "tunnel_status") return [];
      throw new Error(command);
    };
    await restartWorkspace();
    const row = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]')]
      .find(item => item.querySelector(":scope > .tree-row > span")?.textContent === "관리 노드");
    expect(row).toBeDefined();
    await act(async () => { row?.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true })); });
    await press("터널 열기");
    expect(connectionCalls("tunnel_tab_open")).toEqual([["tunnel_tab_open", { target: "node", label: "관리 노드 · 터널 2" }]]);
    const panel = find('.workspace-center section[aria-label="터널"]');
    expect(panel.textContent).toContain("공식 SSM 포트 전달의 바이너리 보존을 확인할 수 없습니다");
    expect(panel.querySelector("form")).toBeNull();
    expect(connectionCalls("tunnel_start")).toEqual([]);
  });

  it("선택한 노드 참조만 시험하고 실패 후에도 선택을 보존하며 저장은 비활성화한다", async () => {
    vi.stubGlobal("__ZZABBA_SSM_DOCUMENT__", "test-document");
    let requestId = 0;
    runDiscovery = async (args) => {
      const { request } = args as { request: { action: string; operation?: { kind: string } } };
      if (request.action === "reserve") return `request-${++requestId}`;
      if (request.operation?.kind === "listProfiles") return [node.profile];
      if (request.operation?.kind === "configuredRegion") return null;
      if (request.operation?.kind === "listOnlineNodes") return [{ instanceId: node.instanceId }];
      throw new Error("예상하지 않은 요청");
    };
    runConnectionCommand = async (command) => {
      if (command === "connection_test_start") throw "SSM 연결 시험을 사용할 수 없습니다";
      throw new Error(`예상하지 않은 연결 명령: ${command}`);
    };

    await press("연결 추가");
    await changeInput("그룹", "개발");
    await changeInput("표시명", "관리 노드");
    await press("다음");
    expect(connectionCalls("ssm_discovery")).toHaveLength(0);
    await chooseMethod("ssm");
    await press("다음");
    expect(button("다음")?.disabled).toBe(true);
    await select("프로필", node.profile);
    await changeInput("리전", node.region);
    await press("리전 적용");
    const selected = await select("온라인 노드", node.instanceId);
    expect(selected?.value).toBe(node.instanceId);
    await changeInput("리전", "us-west-2");
    expect(find("dialog").textContent).toContain("리전 변경 미적용");
    expect(selected.disabled).toBe(true);
    expect(button("다음")?.disabled).toBe(true);
    await press("다음");
    expect(button("연결 테스트")).toBeUndefined();
    await changeInput("리전", node.region);
    await press("다음");
    expect(button("저장")?.disabled).toBe(true);
    await press("연결 테스트");
    const args = connectionCalls("connection_test_start")[0][1] as ConnectionTestDraft;
    expect(args.draft.target).toEqual({
      id: expect.any(String), groupId: args.draft.group.id,
      displayName: "관리 노드", defaultMethod: "ssm", ssm: node
    });
    expect(find("dialog [role=alert]").textContent).toBe("SSM 연결 시험을 사용할 수 없습니다");
    expect(button("저장")?.disabled).toBe(true);
    await press("이전");
    expect([...find("dialog").querySelectorAll("select")].map((control) => control.value)).toEqual([node.profile, node.instanceId]);
    expect(find<HTMLInputElement>("dialog input").value).toBe(node.region);
    await press("다음");
    expect([...find('[aria-label="SSM 연결 대상"]').querySelectorAll("dt, dd")].map((part) => part.textContent)).toEqual([
      "SSO 프로필", node.profile, "리전", node.region, "인스턴스 ID", node.instanceId,
    ]);
    await press("취소");
    expect(host.querySelector("dialog")).toBeNull();
    expect(connectionCalls("connection_save")).toHaveLength(0);
  });

  it.each(["응답 대기", "시험 성공"])("A가 %s 상태에서 B를 선택하면 A 시험을 취소하고 A로 돌아가도 저장을 막는다", async (when) => {
    vi.stubGlobal("__ZZABBA_SSM_DOCUMENT__", "test-document");
    const otherNode = "i-00000002";
    const reply = Promise.withResolvers<ConnectionTestRun & { sessionId: string }>();
    let requestId = 0;
    runDiscovery = async (args) => {
      const { request } = args as { request: { action: string; operation?: { kind: string } } };
      if (request.action === "reserve") return `request-${++requestId}`;
      if (request.operation?.kind === "listProfiles") return [node.profile];
      if (request.operation?.kind === "configuredRegion") return node.region;
      if (request.operation?.kind === "listOnlineNodes") return [node.instanceId, otherNode].map(instanceId => ({ instanceId }));
      throw new Error("예상하지 않은 요청");
    };
    runConnectionCommand = async (command, args) => {
      if (command === "connection_test_start") {
        const { draftId, draftVersion } = args as ConnectionTestDraft;
        const run = { draftId, draftVersion, runId: "trial-a" };
        if (when === "시험 성공") reply.resolve({ ...run, sessionId: "session-a" });
        return run;
      }
      if (command === "connection_test_wait") return reply.promise;
      if (command === "connection_test_cancel") return null;
      throw new Error(`예상하지 않은 연결 명령: ${command}`);
    };
    await press("연결 추가");
    await press("다음");
    await chooseMethod("ssm");
    await press("다음");
    await select("프로필", node.profile);
    await select("온라인 노드", node.instanceId);
    await press("다음");
    await press("연결 테스트");
    expect(button("저장")?.disabled).toBe(when === "응답 대기");
    const { run } = connectionCalls("connection_test_wait")[0][1] as RunArgs;
    await press("이전");
    await select("온라인 노드", otherNode);
    expect(connectionCalls("connection_test_cancel")).toEqual([["connection_test_cancel", { run }]]);
    await act(async () => reply.resolve({ ...run, sessionId: "session-a" }));
    expect(find("dialog").textContent).not.toContain("테스트 성공");
    await press("다음");
    expect(find('[aria-label="SSM 연결 대상"]').textContent).toContain(otherNode);
    expect(button("저장")?.disabled).toBe(true);
    await press("이전");
    await select("온라인 노드", node.instanceId);
    await press("다음");
    expect(button("저장")?.disabled).toBe(true);
    await press("저장");
    expect(connectionCalls("connection_save")).toHaveLength(0);
  });

  it("늦은 SSM 시험 ID를 기다리고 종료 미확인에서 동일 실행을 다시 확인한다", async () => {
    vi.stubGlobal("__ZZABBA_SSM_DOCUMENT__", "test-document");
    const start = Promise.withResolvers<ConnectionTestRun>();
    const cancel = Promise.withResolvers<null>();
    let attempt: ConnectionTestDraft | undefined;
    let requestId = 0;
    runDiscovery = async (args) => {
      const { request } = args as { request: { action: string; operation?: { kind: string } } };
      if (request.action === "reserve") return String(++requestId);
      if (request.operation?.kind === "listProfiles") return [node.profile];
      if (request.operation?.kind === "configuredRegion") return node.region;
      if (request.operation?.kind === "listOnlineNodes") return [{ instanceId: node.instanceId }];
      throw new Error("예상하지 않은 요청");
    };
    runConnectionCommand = async (command, args) => {
      if (command === "connection_test_start") {
        attempt = args as ConnectionTestDraft;
        return start.promise;
      }
      if (command === "connection_test_cancel") return connectionCalls(command).length === 1 ? cancel.promise : null;
      throw new Error(`예상하지 않은 연결 명령: ${command}`);
    };
    await press("연결 추가");
    await press("다음");
    await chooseMethod("ssm");
    await press("다음");
    await select("프로필", node.profile);
    await select("온라인 노드", node.instanceId);
    await press("다음");
    await press("연결 테스트");
    await press("취소");
    const reference = `${node.profile} / ${node.region} / ${node.instanceId}`;
    expect(find("dialog [role=status]").textContent).toBe(`시험 취소 중: ${reference}`);
    expect(connectionCalls("connection_test_cancel")).toHaveLength(0);
    if (!attempt) throw new Error("초안이 없습니다");
    const run = { draftId: attempt.draftId, draftVersion: attempt.draftVersion, runId: "late-trial" };
    await act(async () => start.resolve(run));
    expect(connectionCalls("connection_test_wait")).toHaveLength(0);
    expect(connectionCalls("connection_test_cancel")).toEqual([["connection_test_cancel", { run }]]);
    await act(async () => cancel.reject("미확인"));
    expect(find("dialog [role=alert]").textContent).toBe(`종료 확인 불가: ${reference}`);
    await press("종료 다시 확인");
    expect(connectionCalls("connection_test_cancel")).toEqual(Array(2).fill(["connection_test_cancel", { run }]));
    expect(host.querySelector("dialog")).toBeNull();
  });

  it("구 SSO 연결의 스토리지 개설 거부에서 저장된 대상을 재시험할 수 있다", async () => {
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "dev", displayName: "개발" } }],
      targets: [{ revision: 2, profile: { id: "legacy", groupId: "dev", displayName: "관리 노드", defaultMethod: "ssm", ssm: node } }],
    });
    runStorage = async () => ({ ok: false, error: { stage: "profile", failure: { kind: "retestRequired" }, bytes: 0 } });
    await restartWorkspace();
    await treeMenuAction("관리 노드", "S3 버킷 열기");
    expect(button("관리 노드")?.getAttribute("aria-pressed")).toBe("true");
    expect(find('.workspace-storage section[aria-label="AWS S3"] [role="alert"]').textContent).toContain("다시 시험하고 저장");
    await press("연결 재시험");
    expect(find<HTMLInputElement>('dialog input').value).toBe("관리 노드");
    expect(connectionCalls("connection_save")).toHaveLength(0);
  });

  it("개설 응답을 기다리지 않고 정확한 스토리지 탭을 취소한다", async () => {
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "dev", displayName: "개발" } }],
      targets: [{ revision: 1, profile: { id: "saved-ssm", groupId: "dev", displayName: "관리 노드", defaultMethod: "ssm", ssm: node } }],
    });
    const opening = Promise.withResolvers<unknown>();
    runStorage = async command => command === "storage_open" ? opening.promise : true;
    await restartWorkspace();
    await treeMenuAction("관리 노드", "S3 버킷 열기");
    const id = storageOpenId();
    await press("관리 노드 닫기");
    expect(host.querySelector(".workspace-storage")).toBeNull();
    expect(connectionCalls("storage_close")).toEqual([["storage_close", { request: { sessionId: id } }]]);
    await act(async () => opening.resolve({ ok: true, value: { sessionId: id, profile: node.profile, region: node.region, endpoint: "https://s3.example.invalid" } }));
    expect(host.querySelector(".workspace-storage")).toBeNull();
    expect(connectionCalls("storage_list_objects")).toHaveLength(0);
  });

  it("SSO 만료 응답의 ID만 재로그인에 보내고 개설이나 전송을 재시도하지 않는다", async () => {
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "dev", displayName: "개발" } }],
      targets: [{ revision: 1, profile: { id: "saved-ssm", groupId: "dev", displayName: "관리 노드", defaultMethod: "ssm", ssm: node } }],
    });
    runStorage = async command => {
      if (command === "storage_open") return { ok: false, error: { stage: "identity", failure: { kind: "ssoLoginRequired" }, bytes: 0 } };
      if (command === "storage_relogin") return { ok: true, value: null };
      throw new Error(command);
    };
    await restartWorkspace();
    await treeMenuAction("관리 노드", "S3 버킷 열기");
    expect(connectionCalls("storage_open")[0][1]).toMatchObject({ request: { targetId: "saved-ssm" } });
    expect(connectionCalls("storage_relogin")).toHaveLength(0);
    await press("SSO 재로그인");
    expect(connectionCalls("storage_relogin")).toEqual([["storage_relogin", { request: { id: storageOpenId() } }]]);
    expect(host.querySelectorAll(".workspace-storage")).toHaveLength(1);
    expect(button("관리 노드")?.getAttribute("aria-pressed")).toBe("true");
    expect(connectionCalls("storage_open")).toHaveLength(1);
    expect(connectionCalls("storage_upload")).toHaveLength(0);
    expect(connectionCalls("storage_download")).toHaveLength(0);
  });

  function storageOpenId() {
    const calls = connectionCalls("storage_open");
    const request: unknown = calls[calls.length - 1]?.[1]?.request;
    if (!request || typeof request !== "object" || !("id" in request) || typeof request.id !== "string") throw new Error("개설 ID 없음");
    return request.id;
  }

  it("S3 탭은 로컬 파일 편집과 터미널 전환 중에도 다운로드를 계속한다", async () => {
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "dev", displayName: "개발" } }],
      targets: [{ revision: 1, profile: { id: "saved-ssm", groupId: "dev", displayName: "관리 노드", defaultMethod: "ssm", ssm: node } }],
    });
    await restartWorkspace();
    const session = { sessionId: "storage-id", profile: node.profile, region: node.region, endpoint: "https://s3.example.invalid" };
    const key = "실험//한글 +%2F.txt";
    const picker = Promise.withResolvers<{ kind: "approved"; approvalId: string }>();
    const downloaded = Promise.withResolvers<unknown>();
    runStorage = async (command, args) => {
      switch (command) {
        case "storage_open": {
          const request = args && typeof args === "object" && "request" in args ? args.request : undefined;
          if (!request || typeof request !== "object" || !("id" in request) || typeof request.id !== "string") throw new Error("개설 ID 없음");
          session.sessionId = request.id;
          return { ok: true, value: session };
        }
        case "storage_list_objects": return { ok: true, value: { rows: [{ kind: "object", key, bytes: 9 }], nextToken: null } };
        case "storage_select_local_file": return picker.promise;
        case "storage_download": return downloaded.promise;
        case "storage_close": return true;
        default: throw new Error(command);
      }
    };
    await treeMenuAction("관리 노드", "S3 버킷 열기");
    expect(connectionCalls("storage_open")).toEqual([["storage_open", { request: { targetId: "saved-ssm", id: session.sessionId } }]]);
    const storage = find(".workspace-storage");
    await changeInput("버킷", "test-bucket");
    await press("버킷 열기");
    await act(async () => fileRow(storage, key).click());
    await press("다운로드");
    openFile = async () => ({ status: "opened", handle: "local-fixture", name: "격리.txt", text: "원본" });
    await openNewTab("파일 열기…");
    const model = editor.getModels().find(item => item.uri.query === "local-fixture")!;
    await act(async () => model.setValue("미저장 편집"));
    expect(button("격리.txt")?.getAttribute("aria-pressed")).toBe("true");
    expect(storage.closest("[hidden][inert]")).not.toBeNull();
    await act(async () => picker.resolve({ kind: "approved", approvalId: "approved-id" }));
    expect(connectionCalls("storage_download")).toEqual([["storage_download", {
      request: {
        sessionId: session.sessionId, bucket: "test-bucket", region: node.region, key, approvalId: "approved-id",
      }
    }]]);
    expect(connectionCalls("storage_close")).toHaveLength(0);
    await press("로컬 1");
    const terminal = find('section[aria-label="로컬 1"] textarea');
    expect(terminal).toBe(document.activeElement);
    await act(async () => downloaded.resolve({ ok: true, value: { bytes: 7, integrity: "sdkChecksumVerified" } }));
    await press("관리 노드");
    expect(find(".workspace-storage")).toBe(storage);
    expect(storage.closest("[hidden]")).toBeNull();
    const transfers = [...storage.querySelectorAll(".transfer-table tbody tr")].map(row => row.textContent);
    expect(transfers).toHaveLength(1);
    expect(transfers[0]).toContain("완료");
    expect(transfers[0]).toContain("7 바이트 · SDK 체크섬 검증됨");
    await press("격리.txt");
    await press("관리 노드 닫기");
    expect(connectionCalls("storage_close")).toEqual([["storage_close", { request: { sessionId: session.sessionId } }]]);
    expect(host.querySelector(".workspace-storage")).toBeNull();
    expect(button("격리.txt")?.getAttribute("aria-pressed")).toBe("true");
    expect(model.getValue()).toBe("미저장 편집");
    expect(connectionCalls("editor_local_close")).toHaveLength(0);
  });

  it("SSM 탭은 종료 확인 전 남고 같은 앱 세션 ID로만 다시 확인한다", async () => {
    const close = Promise.withResolvers<null>();
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "dev", displayName: "개발" } }],
      targets: [{ revision: 1, profile: { id: "saved-ssm", groupId: "dev", displayName: "관리 노드", defaultMethod: "ssm", ssm: node } }],
    });
    await restartWorkspace();
    const row = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]')]
      .find((item) => item.textContent === "관리 노드");
    expect(row).toBeDefined();
    await key(row!, "ContextMenu");
    expect([...find('[role="menu"]').querySelectorAll('[role="menuitem"]')].map(item => item.textContent))
      .toEqual(["AWS Systems Manager", "S3 버킷 열기", "터널 열기"]);
    await press("AWS Systems Manager");
    const [{ output, ...args }] = calls("open").slice(-1);
    expect(output).toBeDefined();
    expect(args).toEqual({ targetId: "saved-ssm", cols: 80, rows: 24 });
    expect(status()).toContain("관리 노드 · SSM");
    const field = find<HTMLTextAreaElement>('section[aria-label="관리 노드"] textarea');
    const screen = field.parentElement!;
    const sessionId = [...channels.keys()].slice(-1)[0];
    const receive = channels.get(sessionId)!;
    await act(async () => receive({ kind: "connecting" }));
    expect(status()).toContain("연결 중");
    expect(screen.hasAttribute("inert")).toBe(true);
    expect(document.activeElement).not.toBe(field);
    await key(field, "a");
    await act(async () => field.dispatchEvent(new Event("paste", { bubbles: true, cancelable: true })));
    expect(calls("input")).toHaveLength(0);
    await act(async () => receive({ kind: "ready" }));
    expect(status()).toContain("연결됨");
    expect(screen.hasAttribute("inert")).toBe(false);
    expect(document.activeElement).toBe(field);
    const aiSession = find<HTMLButtonElement>('.workspace-chat button[aria-label^="AI 작업 세션"]');
    expect(menuItemNames(await showMenu(aiSession))).toEqual([`관리 노드 (세션 ${sessionId})`]);
    await closeMenu();
    await openNewTab("RDP 연결");
    expect(menuItemNames(await showMenu(aiSession))).toEqual([`관리 노드 (세션 ${sessionId})`]);
    await closeMenu();
    await press("RDP 3 닫기");
    expect(document.activeElement).toBe(field);
    await press("SFTP 파일");
    expect(find(".workspace-file-status").textContent).toBe("활성 SSH 연결 없음");
    expect(connectionCalls("editor_sftp_attach")).toHaveLength(0);
    await act(async () => field.focus());
    await key(field, "a");
    await act(async () => field.dispatchEvent(new Event("paste", { bubbles: true, cancelable: true })));
    expect(calls("input")).toEqual(["keydown", "paste"].map(text => ({ id: sessionId, bytes: [...new TextEncoder().encode(text)] })));
    closeTerminal = vi.fn().mockReturnValueOnce(close.promise).mockResolvedValue(null);
    native.invoke.mockClear();
    await press("관리 노드 닫기");
    expect(button("관리 노드")).toBeDefined();
    expect(button("관리 노드 닫기")?.disabled).toBe(true);
    expect(find(".workspace-tab [role=status]").textContent?.trim()).toBe("종료 중");
    const [request] = calls("close");
    expect(status()).toContain(`${node.profile} / ${node.region} / ${node.instanceId} · ${request.id}`);
    await act(async () => {
      receive({ kind: "exited", value: 0 });
      receive({ kind: "ready" });
    });
    expect(button("관리 노드")).toBeDefined();
    expect(find(".workspace-tab [role=status]").textContent?.trim()).toBe("종료 중");
    await act(async () => close.reject("미확인"));
    expect(find(".workspace-tab [role=alert]").textContent?.trim()).toBe("종료 확인 불가");
    expect(button("관리 노드")).toBeDefined();
    await press("로컬 1");
    await press("관리 노드 종료 다시 확인");
    expect(calls("close")).toEqual([request, request]);
    expect(button("관리 노드")).toBeUndefined();
    await act(async () => receive({ kind: "ready" }));
    expect(button("관리 노드")).toBeUndefined();
    expect(button("로컬 1")?.getAttribute("aria-pressed")).toBe("true");
  });

  it("원격 종료 대기 중 편집 탭의 저장과 폐기 취소는 파일 핸들에만 적용된다", async () => {
    const ending = Promise.withResolvers<null>();
    const saving = Promise.withResolvers<null>();
    saveFile = () => saving.promise;
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "dev", displayName: "개발" } }],
      targets: [{ revision: 1, profile: { id: "saved-ssm", groupId: "dev", displayName: "관리 노드", defaultMethod: "ssm", ssm: node } }],
    });
    await restartWorkspace();
    const row = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]')].find((item) => item.textContent === "관리 노드");
    expect(row).toBeDefined();
    await key(row!, "Enter");
    const [remoteId] = [...channels.keys()].slice(-1);
    const localInput = find('section[aria-label="로컬 1"] textarea');
    closeTerminal = () => ending.promise;
    native.invoke.mockClear();
    await press("관리 노드 닫기");
    expect(calls("close").slice(-1)).toEqual([{ id: remoteId }]);
    await openNewTab("파일 열기…");
    expect(button("관리 노드")?.getAttribute("aria-pressed")).toBe("true");
    openFile = async () => ({ status: "opened", handle: "selected-file", name: "시험.txt", text: "원본" });
    await openNewTab("파일 열기…");
    const model = editor.getModels().find((item) => item.uri.query === "selected-file")!;
    await act(async () => model.setValue("첫 편집"));
    await press("로컬 1");
    expect(document.activeElement).toBe(localInput);
    await press("시험.txt");
    expect(find('.editor-tab[aria-label="시험.txt"]').contains(document.activeElement)).toBe(true);
    expect(model.getValue()).toBe("첫 편집");
    await press("저장");
    expect(connectionCalls("editor_local_save")).toEqual([["editor_local_save", { handle: "selected-file", text: "첫 편집" }]]);
    await press("로컬 1");
    await requestWindowClose("save-pending");
    expect(status()).toContain("시험.txt: 편집 작업 중 · 창 닫기 보류");
    expect(closeReplies()).toEqual([{ action: "respondToClose", requestId: "save-pending", decision: { kind: "blocked" } }]);
    await press("시험.txt");
    const confirm = vi.spyOn(window, "confirm").mockReturnValueOnce(false).mockReturnValueOnce(true);
    await press("시험.txt 닫기");
    expect(confirm).not.toHaveBeenCalled();
    expect(connectionCalls("editor_local_close")).toHaveLength(0);
    await act(async () => model.setValue("미저장"));
    await act(async () => ending.resolve(null));
    expect(button("관리 노드")).toBeUndefined();
    expect(button("시험.txt")?.getAttribute("aria-pressed")).toBe("true");
    expect(document.activeElement).not.toBe(localInput);
    expect(button("저장")?.disabled).toBe(true);
    await act(async () => saving.resolve(null));
    expect(model.getValue()).toBe("미저장");
    expect(button("저장")?.disabled).toBe(false);
    await vi.waitFor(() => expect(status()).not.toContain("창 닫기 보류"));
    await press("시험.txt 닫기");
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(connectionCalls("editor_local_close")).toHaveLength(0);
    await press("시험.txt 닫기");
    expect(connectionCalls("editor_local_close")).toEqual([["editor_local_close", { handle: "selected-file" }]]);
    expect(button("로컬 1")?.getAttribute("aria-pressed")).toBe("true");
    expect(find('section[aria-label="로컬 1"] textarea')).toBe(localInput);
    expect(document.activeElement).toBe(localInput);
    expect(calls("close")).toEqual([{ id: remoteId }]);
  });
});

describe("F01 직접 SSH", () => {
  const fingerprint = `SHA256:${"A".repeat(43)}`;

  it("네이티브 PIN 등록을 취소하면 연결 시험과 저장을 차단한다", async () => {
    runConnectionCommand = async (command) => {
      if (command === "connection_capabilities") return { storedAuth: true };
      throw "cancelled";
    };
    const select = async (label: string, value: string) => {
      const control = [...find("dialog").querySelectorAll("label")]
        .find(item => item.firstChild?.textContent?.trim() === label)?.querySelector("select");
      if (!control) throw new Error(label);
      await act(async () => {
        control.value = value;
        control.dispatchEvent(new Event("change", { bubbles: true }));
      });
      return control;
    };
    await press("연결 추가");
    await changeInput("그룹", "작업");
    await changeInput("표시명", "개발 서버");
    await press("다음");
    await chooseMethod("ssh");
    expect(find<HTMLOptionElement>('option[value="ssm"][disabled]').textContent).toContain("SSH-over-SSM");
    await changeInput("서버 호스트", "dev.example.invalid");
    await changeInput("SSH 포트", "2222");
    await changeInput("SSH 계정", "deploy");
    await changeInput("호스트 키 SHA256 지문", fingerprint);
    await press("다음");
    const auth = await select("SSH 인증 방식", "manualPassword");
    expect(auth.querySelector<HTMLOptionElement>('option[value="storedPassword"]')?.disabled).toBe(false);
    await press("다음");
    expect(find('[aria-label="SSH 연결 대상"]').textContent).toContain("deploy");
    await press("호스트 키 선택 및 등록");
    expect(connectionCalls("verify_ssh_host")).toEqual([["verify_ssh_host", {
      request: { targetId: expect.any(String), host: "dev.example.invalid", port: 2222, expectedFingerprint: fingerprint },
      attempt: { draftId: expect.any(String), draftVersion: expect.any(Number), attemptId: "1" },
      draft: expect.objectContaining({
        target: expect.objectContaining({
          ssh: {
            host: "dev.example.invalid", port: 2222, account: "deploy", hostKeyFingerprint: fingerprint,
            transport: { kind: "direct" }, auth: { kind: "manualPassword" },
          }
        })
      }),
    }]]);
    expect(find("dialog [role=alert]").textContent).toBe("SSH 호스트 키 등록이 취소되었습니다");
    expect(button("연결 테스트")?.disabled).toBe(true);
    expect(button("저장")?.disabled).toBe(true);
    expect(connectionCalls("connection_test_start")).toEqual([]);
    expect(connectionCalls("ssm_discovery")).toEqual([]);
  });

  it("시험 PTY만 입력을 받고 닫힌 시험의 늦은 성공은 저장할 수 없다", async () => {
    const waits = new Map<string, PromiseWithResolvers<unknown>>();
    let serial = 0;
    let pin: { targetId: string; host: string; port: number; fingerprint: string } | undefined;
    runConnectionCommand = async (command, args) => {
      if (command === "verify_ssh_host") {
        const { request } = args as { request: { targetId: string; host: string; port: number } };
        const { targetId, host, port } = request;
        pin = { targetId, host, port, fingerprint };
        return { ...pin, ticket: "a".repeat(32) };
      }
      if (command === "commit_ssh_host") return pin;
      if (command === "connection_test_start") {
        const { draftId, draftVersion } = args as ConnectionTestDraft;
        const runId = `attempt-${++serial}`;
        waits.set(runId, Promise.withResolvers());
        return { draftId, draftVersion, runId };
      }
      const { run } = args as RunArgs;
      if (command === "connection_ssh_test_attach") return `trial-${run.runId}`;
      if (command === "connection_test_wait") return waits.get(run.runId)?.promise;
      if (command === "connection_test_cancel") return null;
      throw new Error(command);
    };
    const select = async (label: string, value: string) => {
      const control = [...find("dialog").querySelectorAll("label")]
        .find(item => item.firstChild?.textContent?.trim() === label)?.querySelector("select");
      if (!control) throw new Error(label);
      await act(async () => {
        control.value = value;
        control.dispatchEvent(new Event("change", { bubbles: true }));
      });
    };
    await press("연결 추가");
    await changeInput("그룹", "작업");
    await changeInput("표시명", "개발 서버");
    await press("다음");
    await chooseMethod("ssh");
    await changeInput("서버 호스트", "dev.example.invalid");
    await changeInput("SSH 계정", "deploy");
    await changeInput("호스트 키 SHA256 지문", fingerprint);
    await press("다음");
    await select("SSH 인증 방식", "manualPassword");
    await press("다음");
    await press("호스트 키 선택 및 등록");
    await press("연결 테스트");
    const [[, { run: first }]] = connectionCalls("connection_ssh_test_attach");
    expect(first.runId).toBe("attempt-1");
    const field = find<HTMLTextAreaElement>('[aria-label="SSH 시험 터미널"] textarea');
    expect(document.activeElement).toBe(field);
    await type(field, "fixture");
    expect(calls("input")).toEqual([{ id: "trial-attempt-1", bytes: Array.from(new TextEncoder().encode("fixture")) }]);
    expect(button("저장")?.disabled).toBe(true);
    await press("SSH 시험 닫기");
    expect(connectionCalls("connection_test_cancel")).toEqual([["connection_test_cancel", { run: first }]]);
    await press("연결 테스트");
    await act(async () => waits.get(first.runId)?.resolve({ ...first, sessionId: "trial-attempt-1" }));
    expect(button("저장")?.disabled).toBe(true);
    await press("이전");
    expect(connectionCalls("connection_test_cancel").map(([, args]) => args.run.runId)).toEqual(["attempt-1", "attempt-2"]);
  });

  it("같은 창에서 새로고침한 SSH 행의 키를 등록하고 더블클릭으로만 연결한다", async () => {
    const registration = Promise.withResolvers<null>();
    runConnectionCommand = vi.fn().mockRejectedValueOnce("SSH 호스트 키 등록이 취소되었습니다").mockReturnValueOnce(registration.promise);
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "work", displayName: "작업" } }],
      targets: [{
        revision: 1, profile: {
          id: "server", groupId: "work", displayName: "개발 서버", defaultMethod: "ssh",
          ssh: {
            host: "dev.example.invalid", port: 2222, account: "deploy", hostKeyFingerprint: fingerprint,
            transport: { kind: "direct" }, auth: { kind: "manualPassword" },
          },
        }
      }],
    });
    expect(tree().textContent).not.toContain("개발 서버");
    await press("연결 새로고침");
    const target = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]')].find(item => item.textContent === "개발 서버")!;
    expect(target).toBeDefined();

    await key(target, "ContextMenu");
    await key(find('[role="menuitem"]'), "Escape");
    expect(host.querySelector('[role="menu"]')).toBeNull();
    expect(connectionCalls("register_saved_ssh_host")).toEqual([]);

    await key(target, "ContextMenu");
    expect([...find('[role="menu"]').querySelectorAll('[role="menuitem"]')].map(item => item.textContent))
      .toEqual(["SSH", "터널 열기", "호스트 키 등록"]);
    await press("호스트 키 등록");
    expect(host.querySelector('[role="menu"]')).toBeNull();
    expect(find('footer [role="alert"]').textContent).toContain("SSH 호스트 키 등록이 취소되었습니다");

    await key(target, "ContextMenu");
    await press("호스트 키 등록");
    expect(connectionCalls("register_saved_ssh_host")).toEqual([
      ["register_saved_ssh_host", { targetId: "server" }],
      ["register_saved_ssh_host", { targetId: "server" }],
    ]);
    await act(async () => { registration.resolve(null); });
    expect(status()).toContain("SSH 호스트 키 등록 완료");
    expect(calls("open").filter(args => args.targetId)).toEqual([]);
    await act(async () => { target.dispatchEvent(new MouseEvent("dblclick", { bubbles: true })); });
    expect(calls("open").slice(-1)[0]).toMatchObject({ targetId: "server" });
  });

  it("SSH 종료가 미확인이면 탭을 남기고 같은 PTY ID로만 다시 확인한다", async () => {
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "work", displayName: "작업" } }],
      targets: [{
        revision: 1, profile: {
          id: "server", groupId: "work", displayName: "개발 서버", defaultMethod: "ssh",
          ssh: {
            host: "dev.example.invalid", port: 2222, account: "deploy", hostKeyFingerprint: fingerprint,
            transport: { kind: "direct" }, auth: { kind: "manualPassword" }
          }
        }
      }],
    });
    await restartWorkspace();
    const target = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]')].find(item => item.textContent === "개발 서버")!;
    await key(target, "Enter");
    await key(target, "Enter");
    expect(calls("open").slice(-2).map(args => args.targetId)).toEqual(["server", "server"]);
    const [first, second] = calls("start").slice(-2).map(args => args.id);
    expect(first).not.toBe(second);
    closeTerminal = vi.fn().mockRejectedValueOnce("SSH 자식 종료 확인 불가: wait").mockResolvedValue(null);
    native.invoke.mockClear();
    await press("개발 서버 닫기");
    expect(calls("close")).toEqual([{ id: first }]);
    expect(find(".workspace-tab [role=alert]").textContent?.trim()).toBe("종료 확인 불가");
    expect(status()).toContain(`SSH · ${second}`);
    await press("개발 서버 종료 다시 확인");
    expect(calls("close")).toEqual([{ id: first }, { id: first }]);
    expect(document.querySelector(".workspace-tab [role=alert]")).toBeNull();
    expect(status()).toContain(`SSH · ${second}`);
  });

  it.each(["unconfirmed", "close", "exited", "disconnected", "channel", "saved", "unknown"] as const)("F02 nano 편집 오퍼에 고정된 세션의 상태만 반영한다 (%s)", async (scenario) => {
    const offerId = "1".repeat(32);
    const handle = "f".repeat(32);
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "work", displayName: "작업" } }],
      targets: [{
        revision: 1, profile: {
          id: "nano", groupId: "work", displayName: "nano 서버", defaultMethod: "ssh",
          ssh: {
            host: "nano.example.test", port: 2222, account: "test", hostKeyFingerprint: fingerprint,
            transport: { kind: "direct" }, auth: { kind: "keyFile", privateKeyPath: "/fixture/id_ed25519" }
          },
        }
      }],
    });
    const otherOfferId = "3".repeat(32);
    runNanoEditor = async (command, args) => {
      if (command === "editor_remote_nano_claim") {
        if (args && typeof args === "object" && "offerId" in args && args.offerId === otherOfferId) {
          return { handle: "e".repeat(32), name: "다른 편집.txt", text: "다른 원본" };
        }
        return { handle, name: "원격 편집.txt", text: "원격 원본" };
      }
      if (command === "editor_remote_nano_mounted") {
        if (scenario === "unconfirmed") throw new Error("응답 유실");
        return;
      }
      if (command === "editor_remote_nano_failed") throw new Error("부착 확인 유실");
      throw new Error(command);
    };
    const save = Promise.withResolvers<unknown>();
    runSftp = async command => {
      if (command === "editor_sftp_save") return save.promise;
      if (command !== "editor_sftp_close") throw new Error(command);
    };
    await restartWorkspace();
    const row = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]')].find(item => item.textContent === "nano 서버")!;
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    await key(row, "ContextMenu");
    await press("nano 연동으로 열기");
    expect(calls("open").slice(-1)[0]).toMatchObject({ targetId: "nano", remoteEditor: true });
    const { id: sessionId } = calls("start").slice(-1)[0];
    expect(receiveNanoOpen).toBeDefined();
    const event = "editor-remote-nano-open";
    await act(async () => { void receiveNanoOpen?.({ event, id: 1, payload: { offerId: "2".repeat(32), sessionId: "999" } }); });
    expect(button("원격 편집.txt")).toBeUndefined();
    expect(connectionCalls("editor_remote_nano_mounted")).toHaveLength(0);
    await act(async () => { void receiveNanoOpen?.({ event, id: 1, payload: { offerId, sessionId } }); });
    expect(connectionCalls("editor_remote_nano_mounted")).toEqual([["editor_remote_nano_mounted", { offerId, handle }]]);
    expect(find('.editor-tab[aria-label="원격 편집.txt"]').hidden).toBe(false);
    const editorPane = find('.editor-tab[aria-label="원격 편집.txt"]');
    if (scenario === "unconfirmed") {
      expect(editorPane.querySelector<HTMLButtonElement>(".editor-toolbar button")?.disabled).toBe(true);
      expect(connectionCalls("editor_remote_nano_failed")).toContainEqual(["editor_remote_nano_failed", { offerId, handle }]);
      expect(status()).toContain("nano 편집 연결 확인 불가: 응답 유실");
      await press("원격 편집.txt 닫기");
      expect(connectionCalls("editor_sftp_close")).toEqual([["editor_sftp_close", { sessionId, handle }]]);
    } else {
      const output = editorPane.querySelector("output");
      const model = editor.getModels().find(item => item.uri.query === handle)!;
      await act(async () => model.setValue("미저장 편집"));
      await key(row, "ContextMenu");
      await press("nano 연동으로 열기");
      const otherSessionId = calls("start").slice(-1)[0].id;
      expect(otherSessionId).not.toBe(sessionId);
      await act(async () => { channels.get(otherSessionId)?.({ kind: "exited", value: 0 }); });
      await press("원격 편집.txt");
      const channelEvent = "editor-remote-nano-disconnected";
      const emitDisconnect = async (payload: { sessionId: string; offerId: string }) => {
        await act(async () => { void receiveNanoDisconnect?.({ event: channelEvent, id: 2, payload }); });
      };
      await emitDisconnect({ sessionId: otherSessionId, offerId });
      await emitDisconnect({ sessionId, offerId: otherOfferId });
      expect(output?.textContent).toBe("미저장");

      if (scenario === "saved" || scenario === "unknown") await press("저장");
      if (scenario === "close") {
        const closed = Promise.withResolvers<null>();
        closeTerminal = () => closed.promise;
        await press("nano 서버 닫기");
        expect(output?.textContent).toContain("연결 끊김");
        expect(button("nano 서버 닫기")?.disabled).toBe(true);
        await act(async () => closed.resolve(null));
        expect(calls("close")).toContainEqual({ id: sessionId });
      } else if (scenario === "channel") {
        await emitDisconnect({ sessionId, offerId });
        await act(async () => { void receiveNanoOpen?.({ event, id: 3, payload: { sessionId, offerId: otherOfferId } }); });
        expect(find('.editor-tab[aria-label="다른 편집.txt"]').hidden).toBe(false);
        await emitDisconnect({ sessionId, offerId });
        expect(find('.editor-tab[aria-label="다른 편집.txt"] output').textContent).toBe("");
        await press("원격 편집.txt");
      } else {
        const end = scenario === "disconnected" ? { kind: "disconnected", value: "연결 단절" } : { kind: "exited", value: 0 };
        await act(async () => { channels.get(sessionId)?.(end); });
      }

      if (scenario === "saved" || scenario === "unknown") {
        await act(async () => save.resolve(scenario === "saved" ? { status: "saved" } : {
          status: "unknown", error: { kind: "disconnected" }, recoveryPath: "/tmp/recovery.txt",
        }));
        await emitDisconnect({ sessionId, offerId });
      }
      expect(output?.textContent).toContain(scenario === "unknown" ? "복구 파일: /tmp/recovery.txt" : "연결 끊김");
      expect(editor.getModels().find(item => item.uri.query === handle)).toBe(model);
      expect(model.getValue()).toBe("미저장 편집");
      expect(button("저장")?.disabled).toBe(scenario === "saved" || scenario === "unknown");
      expect(connectionCalls("editor_sftp_save")).toHaveLength(scenario === "saved" || scenario === "unknown" ? 1 : 0);
    }
    expect(confirm).not.toHaveBeenCalled();
    expect(connectionCalls("editor_sftp_attach")).toHaveLength(0);
    expect(connectionCalls("editor_sftp_detach")).toHaveLength(0);
  });
});

describe("현재 SSH 세션의 SFTP", () => {
  beforeEach(async () => {
    loadProfiles = async () => ({
      version: 1,
      groups: [{ revision: 1, profile: { id: "work", displayName: "작업" } }],
      targets: [
        ...["서버 A", "서버 B"].map<SavedConnections["targets"][number]>((displayName, index) => ({
          revision: 1,
          profile: {
            id: `ssh-${index}`, groupId: "work", displayName, defaultMethod: "ssh",
            ssh: {
              host: "dev.example.invalid", port: 2222, account: "deploy", hostKeyFingerprint: `SHA256:${"A".repeat(43)}`,
              transport: { kind: "direct" }, auth: { kind: "manualPassword" },
            },
          },
        })),
        ...["개발 버킷", "격리 버킷"].map<SavedConnections["targets"][number]>((displayName, index) => ({
          revision: 1,
          profile: {
            id: `s3-${index}`, groupId: "work", displayName, defaultMethod: "s3",
            s3: { profile: "dev", region: "ap-northeast-2", bucket: `bucket-${index}`, prefix: `saved-${index}/` },
          },
        })),
      ],
    });
    await restartWorkspace();
  });

  async function openTarget(label: string) {
    await key(treeTarget(label), "Enter");
  }

  async function openSsh(label: string) {
    await openTarget(label);
    const started = calls("start");
    const { id } = started[started.length - 1];
    return { id, receive: channels.get(id)! };
  }

  it("저장된 SSH 터널을 중앙에 열고 공유 실패와 종료 미확인에 각 네이티브 핸들을 사용한다", async () => {
    const close = vi.fn().mockResolvedValueOnce(null).mockRejectedValueOnce("터널 자식 종료 확인 불가").mockResolvedValue(null);
    const pendingShare = vi.fn().mockRejectedValueOnce("공유 실패").mockResolvedValue(null);
    let opens = 0;
    runTunnel = async (command, args) => {
      if (command === "tunnel_tab_open") return `tunnel-${++opens}`;
      if (command === "tunnel_tab_close") return close(args);
      if (command === "tunnel_share") return pendingShare(args);
      if (command === "tunnel_status") return [{
        id: "forward", endpoint: "127.0.0.1:8090", route: "-L", owners: ["서버 A"], state: "ready", error: null,
      }];
      throw new Error(command);
    };
    await openSecond();
    await press("SFTP 파일");
    const fileView = find(".workspace-sidebar > .workspace-side-body:not([hidden])");
    expect(fileView.closest("[hidden], [inert]")).toBeNull();
    expect(sftpSidebar().contains(fileView)).toBe(true);
    const row = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]')]
      .find(item => item.querySelector(":scope > .tree-row > span")?.textContent === "서버 A");
    expect(row).toBeDefined();
    await act(async () => { row?.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true })); });
    expect(button("호스트 키 등록")).toBeDefined();
    const openedTerminals = calls("open").length;
    await press("터널 열기");
    const title = "서버 A · 터널 3";
    expect(connectionCalls("tunnel_tab_open")).toEqual([["tunnel_tab_open", { target: "ssh-0", label: title }]]);
    expect(calls("open")).toHaveLength(openedTerminals);
    const panel = find('.workspace-center section[aria-label="터널"]');
    expect(panel.closest("[hidden]")).toBeNull();
    expect(status()).toContain(title);
    expect(find(".workspace-sidebar > .workspace-side-body:not([hidden])")).toBe(fileView);
    expect(fileView.closest("[hidden], [inert]")).toBeNull();
    await press("로컬 2");
    // 터널 탭은 같은 연결의 새 터미널로 복제하지 않는다.
    expect(menuItemNames(await showTabMenu(title))).toEqual(["오른쪽으로 분리", "닫기"]);
    await closeMenu();
    await press(title);

    await press("새 탭에 공유");
    expect(pendingShare).toHaveBeenCalledWith({ tab: "tunnel-1", to: "tunnel-2", id: "forward" });
    expect(close).toHaveBeenNthCalledWith(1, { tab: "tunnel-2" });
    expect(panel.querySelector('[role="alert"]')?.textContent).toBe("공유 실패");
    expect(button("서버 A · 터널 4")).toBeUndefined();

    await press("새 탭에 공유");
    expect(pendingShare).toHaveBeenLastCalledWith({ tab: "tunnel-1", to: "tunnel-3", id: "forward" });
    expect(button("서버 A · 터널 5")?.getAttribute("aria-pressed")).toBe("true");
    await press(title);
    await press(`${title} 닫기`);
    expect(close).toHaveBeenNthCalledWith(2, { tab: "tunnel-1" });
    expect(panel.querySelector('[role="alert"]')?.textContent).toBe("터널 자식 종료 확인 불가");
    await press(`${title} 종료 다시 확인`);
    expect(close).toHaveBeenNthCalledWith(3, { tab: "tunnel-1" });
    expect(button(title)).toBeUndefined();
    expect(button("서버 A · 터널 5")?.getAttribute("aria-pressed")).toBe("true");
    expect(find(".workspace-sidebar > .workspace-side-body:not([hidden])")).toBe(fileView);
  });

  it.each(["rdp", "vnc"] as const)("Ready 터널의 %s 선택자를 중앙 탭에 전달한다", async protocol => {
    const rdpTab = vi.spyOn(rdp, "RdpTab");
    const vncView = vi.spyOn(vnc, "VncView");
    const destination = { host: "private.example.invalid", port: 14444 };
    runTunnel = async command => {
      if (command === "tunnel_tab_open") return "rust-owned-handle";
      if (command === "tunnel_status") return [{
        id: "rust-forward-id", endpoint: "127.0.0.1:8090", route: "-L", owners: ["서버 A"], state: "ready", error: null,
        desktop: { protocol, targetId: "rust-desktop-target", destination },
      }];
      throw new Error(command);
    };
    const row = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]')]
      .find(item => item.querySelector(":scope > .tree-row > span")?.textContent === "서버 A");
    expect(row).toBeDefined();
    await act(async () => { row?.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true })); });
    await press("터널 열기");
    const title = protocol.toUpperCase();
    await press(`${title}로 열기`);
    expect(button(`${title} 3`)?.getAttribute("aria-pressed")).toBe("true");
    const props = protocol === "rdp" ? rdpTab.mock.lastCall?.[0] : vncView.mock.lastCall?.[0];
    expect(props).toEqual(expect.objectContaining({ active: true, source: {
      tabHandle: "rust-owned-handle", tunnelId: "rust-forward-id", targetId: "rust-desktop-target", destination,
    } }));
    expect(status()).toBe(`${title} 3 · SSH 터널${protocol === "rdp" ? " · NLA" : ""}`);
    expect(find(".workspace-center .workspace-session.workspace-desktop").closest("[hidden]")).toBeNull();
  });

  it("채팅과 중앙 탭을 전환해도 SSH별 목록과 초안을 유지하고 종료된 세션만 분리한다", async () => {
    runSftp = async (command, args) => {
      const { sessionId } = args as { sessionId: string };
      if (command === "editor_sftp_attach") return {
        entries: [{ entryId: "file", name: `${sessionId}.txt`, isDirectory: false }], parentEntryId: null,
      };
      if (command === "editor_sftp_detach") return;
      throw new Error(command);
    };
    const draft = find<HTMLTextAreaElement>('textarea[aria-label="메시지 입력"]');
    await type(draft, "유지할 채팅 초안");
    expect(connectionCalls("editor_sftp_attach")).toEqual([]);
    const starting = Promise.withResolvers<void>();
    starts.set(`pty-${channels.size + 10}`, starting.promise);
    const first = await openSsh("서버 A");
    expect(status()).toContain("여는 중");
    expect(connectionCalls("editor_sftp_attach")).toEqual([]);
    await act(async () => starting.resolve());
    expect(connectionCalls("editor_sftp_attach")).toEqual([]);
    await press("SFTP 파일");
    expect(button("SFTP 파일")?.getAttribute("aria-pressed")).toBe("true");
    expect(find('section[aria-label="서버 A"]').closest("[hidden]")).toBeNull();
    expect(draft.closest("[hidden], [inert]")).toBeNull();
    const firstFile = fileRow(host, `${first.id}.txt`);
    await act(async () => firstFile.click());
    expect(firstFile.getAttribute("aria-selected")).toBe("true");
    // SFTP 보기를 닫아도 패널과 선택은 유지한다.
    await press("SFTP 파일");
    expect(firstFile.closest("[hidden][inert]")).not.toBeNull();
    expect(draft.value).toBe("유지할 채팅 초안");
    expect(connectionCalls("editor_sftp_detach")).toEqual([]);
    await press("SFTP 파일");
    expect(fileRow(host, `${first.id}.txt`)).toBe(firstFile);
    expect(firstFile.getAttribute("aria-selected")).toBe("true");

    const second = await openSsh("서버 B");
    expect(firstFile.closest("[hidden][inert]")).not.toBeNull();
    const secondFile = fileRow(host, `${second.id}.txt`);
    expect(secondFile.closest("[hidden]")).toBeNull();
    expect(firstFile.closest("[hidden][inert]")).not.toBeNull();
    await press("서버 A");
    expect(fileRow(host, `${first.id}.txt`)).toBe(firstFile);
    expect(firstFile.closest("[hidden]")).toBeNull();
    expect(secondFile.closest("[hidden][inert]")).not.toBeNull();
    expect(connectionCalls("editor_sftp_attach")).toEqual([
      ["editor_sftp_attach", { sessionId: first.id }], ["editor_sftp_attach", { sessionId: second.id }],
    ]);

    await act(async () => second.receive({ kind: "exited", value: 0 }));
    expect(() => fileRow(host, `${second.id}.txt`)).toThrow();
    expect(firstFile.closest("[hidden]")).toBeNull();
    expect(connectionCalls("editor_sftp_detach")).toEqual([["editor_sftp_detach", { sessionId: second.id }]]);
    expect(calls("close").some(args => args.id === second.id)).toBe(false);
    await press("서버 B");
    expect(find(".workspace-file-status").textContent).toBe("활성 SSH 연결 없음");
    expect(find('textarea[aria-label="메시지 입력"]')).toBe(draft);
    expect(draft.value).toBe("유지할 채팅 초안");
  });

  it("활성 SSH와 호환 스토리지를 오가도 초안과 세션을 유지하고 같은 세션의 종료를 다시 확인한다", async () => {
    runSftp = async command => {
      if (command === "editor_sftp_attach") return { entries: [], parentEntryId: null };
      throw new Error(command);
    };
    const target = { serviceId: "fixture", alias: "호환 설정", origin: "https://storage.example.test", region: "fixture-1", addressing: "path" };
    let failClose = true;
    runStorage = async (command, args) => {
      const { request: { sessionId } } = args as { request: { sessionId: string } };
      if (command === "storage_compatible_open") return {
        ok: true, value: {
          sessionId, target,
          initial: { kind: "buckets", page: { rows: [{ name: "fixture-bucket", region: target.region }], nextToken: null } }
        }
      };
      if (command === "storage_close") {
        if (failClose) throw new Error("종료 응답 유실");
        return true;
      }
      throw new Error(command);
    };
    const ssh = await openSsh("서버 A");
    await press("SFTP 파일");
    const follow = find(".workspace-sftp:not([hidden])");
    await openNewTab("S3 호환 스토리지");
    const panel = find(".workspace-storage");
    expect(panel.closest("[hidden]")).toBeNull();
    expect(follow.matches("[hidden][inert]")).toBe(true);
    await changeInput("서비스 ID", target.serviceId);
    const draft = panel.querySelector("input");
    await press("서버 A");
    expect(find(".workspace-sftp:not([hidden])")).toBe(follow);
    expect(panel.closest("[hidden][inert]")).not.toBeNull();
    // 같은 대상을 다시 열면 기존 탭으로 이동한다.
    await openNewTab("S3 호환 스토리지");
    expect(host.querySelectorAll(".workspace-storage")).toHaveLength(1);
    expect(find(".workspace-storage")).toBe(panel);
    expect(panel.closest("[hidden]")).toBeNull();
    expect(panel.querySelector("input")).toBe(draft);
    expect(draft?.value).toBe(target.serviceId);
    expect(connectionCalls("storage_compatible_open")).toHaveLength(0);
    await changeInput("별칭", target.alias);
    await changeInput("HTTPS 출처", target.origin);
    await changeInput("서명 리전", target.region);
    await changeInput("키링 항목 ID", "fixture-key");
    const scopeSelect = [...panel.querySelectorAll("select")].find(select => select.labels?.[0]?.firstChild?.textContent === "연결 범위");
    expect(scopeSelect).toBeDefined();
    await act(async () => {
      scopeSelect!.value = "service_browse";
      scopeSelect!.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await press("설정 저장 및 연결");
    const opens = connectionCalls("storage_compatible_open");
    expect(opens).toHaveLength(1);
    expect(opens[0][1].request).toMatchObject({ scope: { kind: "service_browse" } });
    const { sessionId } = opens[0][1].request as { sessionId: string };
    expect(sessionId).toBeTruthy();
    const bucket = control(panel, "fixture-bucket");
    await press("서버 A");
    expect(panel.closest("[hidden][inert]")).not.toBeNull();
    await openNewTab("S3 호환 스토리지");
    expect(control(panel, "fixture-bucket")).toBe(bucket);
    expect(connectionCalls("storage_compatible_open")).toHaveLength(1);
    expect(connectionCalls("storage_list_buckets")).toHaveLength(0);
    expect(connectionCalls("storage_close")).toHaveLength(0);
    await press("S3 호환 HTTPS 닫기");
    expect(tabCloseStatus("S3 호환 HTTPS")).toBe("종료 확인 불가");
    expect(panel.isConnected).toBe(true);
    failClose = false;
    await press("S3 호환 HTTPS 종료 다시 확인");
    const close = ["storage_close", { request: { sessionId } }];
    expect(connectionCalls("storage_close")).toEqual([close, close]);
    expect(panel.isConnected).toBe(false);
    expect(button("S3 호환 HTTPS")).toBeUndefined();
    expect(button("서버 A")?.getAttribute("aria-pressed")).toBe("true");
    expect(find(".workspace-sftp:not([hidden])")).toBe(follow);
    expect(connectionCalls("editor_sftp_attach")).toEqual([["editor_sftp_attach", { sessionId: ssh.id }]]);
    expect(connectionCalls("editor_sftp_detach")).toHaveLength(0);
  });

  it("A에서 S3를 고정하고 B를 따라간 뒤 같은 S3의 경로와 선택으로 돌아온다", async () => {
    runSftp = async (command, args) => {
      const { sessionId } = args as { sessionId: string };
      if (command === "editor_sftp_attach") return {
        entries: [{ entryId: "dir", name: `폴더 ${sessionId}`, isDirectory: true }], parentEntryId: null,
      };
      if (command === "editor_sftp_list") return {
        entries: [{ entryId: "file", name: `${sessionId}.txt`, isDirectory: false }], parentEntryId: null,
      };
      if (command === "editor_sftp_detach") return;
      throw new Error(command);
    };
    let failClose = true;
    runStorage = async (command, args) => {
      const { request } = args as { request: { id: string; targetId: string; prefix: string } };
      if (command === "storage_open") {
        const index = request.targetId === "s3-0" ? 0 : 1;
        const location = { bucket: `bucket-${index}`, prefix: `saved-${index}/`, region: "ap-northeast-2" };
        return { ok: true, value: { sessionId: request.id, profile: "dev", endpoint: "https://s3.example.invalid", region: location.region, location } };
      }
      if (command === "storage_list_objects") return {
        ok: true, value: { rows: [{ kind: "object", key: `${request.prefix}notes.txt`, bytes: 7 }], nextToken: null },
      };
      if (command === "storage_count_previous_files") return { ok: true, value: 0 };
      if (command === "storage_close") {
        if (failClose) throw new Error("종료 실패");
        return true;
      }
      throw new Error(command);
    };
    const second = await openSsh("서버 B");
    const first = await openSsh("서버 A");
    await tabMenuAction("서버 B", "오른쪽으로 분리");
    await press("서버 A");
    expect([tabGroup("서버 A"), tabGroup("서버 B")]).toEqual([1, 2]);
    const right = find<HTMLTextAreaElement>('section[aria-label="서버 B"] textarea');
    const split = find(".workspace-group-splitter");
    const openedTerminals = calls("open").length;
    const visible = (element: HTMLElement) => element.closest("[hidden], [inert]") === null;
    const sftpLocation = () => find('.workspace-sftp:not([hidden]) [aria-label="SFTP 경로"] [aria-current="location"]');
    await press("SFTP 파일");
    await dblclick(fileRow(host, `폴더 ${first.id}`));
    const firstPath = sftpLocation();
    expect(firstPath.textContent).toBe(`폴더 ${first.id}`);
    expect(visible(firstPath)).toBe(true);

    await openTarget("개발 버킷");
    const pinned = find(".workspace-storage");
    expect(tabGroup("개발 버킷")).toBe(1);
    const object = fileRow(pinned, "saved-0/notes.txt");
    await act(async () => object.click());
    expect(object.getAttribute("aria-selected")).toBe("true");
    expect(visible(pinned)).toBe(true);
    expect(visible(firstPath)).toBe(false);
    expect(visible(right)).toBe(true);
    await openNewTab("VNC 연결");
    expect(visible(pinned)).toBe(false);
    expect(pinned.isConnected).toBe(true);
    expect(visible(firstPath)).toBe(false);
    expect(visible(right)).toBe(true);
    await press("서버 A");
    expect(sftpLocation()).toBe(firstPath);
    expect(visible(firstPath)).toBe(true);
    expect(connectionCalls("editor_sftp_attach")).toEqual([["editor_sftp_attach", { sessionId: first.id }]]);

    // SFTP 보기는 포커스된 직접 SSH 탭을 따라간다.
    await act(async () => right.focus());
    expect(status()).toContain(second.id);
    await dblclick(fileRow(host, `폴더 ${second.id}`));
    const secondPath = sftpLocation();
    expect(secondPath.textContent).toBe(`폴더 ${second.id}`);
    expect(visible(secondPath)).toBe(true);
    expect(visible(firstPath)).toBe(false);
    expect(visible(pinned)).toBe(false);
    await openTarget("개발 버킷");
    expect(find(".workspace-storage")).toBe(pinned);
    expect(visible(pinned)).toBe(true);
    expect(fileRow(pinned, "saved-0/notes.txt")).toBe(object);
    expect(object.getAttribute("aria-selected")).toBe("true");
    expect(find(".workspace-storage .s3-crumbs").textContent).toContain("saved-0");
    expect(visible(firstPath)).toBe(false);
    expect(visible(secondPath)).toBe(false);
    expect(connectionCalls("storage_open")).toHaveLength(1);
    expect(connectionCalls("storage_list_objects")).toHaveLength(1);

    await openTarget("격리 버킷");
    const other = [...host.querySelectorAll<HTMLElement>(".workspace-storage")].find(item => item !== pinned)!;
    expect(other).toBeDefined();
    expect(visible(other)).toBe(true);
    expect(visible(pinned)).toBe(false);
    expect(connectionCalls("storage_open")).toHaveLength(2);
    await press("격리 버킷 닫기");
    expect(visible(other)).toBe(true);
    expect(tabCloseStatus("격리 버킷")).toBe("종료 확인 불가");
    failClose = false;
    await press("격리 버킷 종료 다시 확인");
    const secondOpen = connectionCalls("storage_open")[1]?.[1]?.request as { id: string };
    const closeRequest = ["storage_close", { request: { sessionId: secondOpen.id } }];
    expect(connectionCalls("storage_close")).toEqual([closeRequest, closeRequest]);
    expect(other.isConnected).toBe(false);
    await openTarget("개발 버킷");
    expect(visible(pinned)).toBe(true);
    expect(connectionCalls("storage_open")).toHaveLength(2);
    expect(connectionCalls("editor_sftp_attach")).toEqual([
      ["editor_sftp_attach", { sessionId: first.id }], ["editor_sftp_attach", { sessionId: second.id }],
    ]);
    expect(connectionCalls("editor_sftp_detach")).toHaveLength(0);
    expect(calls("open")).toHaveLength(openedTerminals);
    expect(find(".workspace-group-splitter")).toBe(split);
    expect(button("서버 B")?.getAttribute("aria-pressed")).toBe("true");
    await press("개발 버킷 닫기");
  });

  it("분할된 SSH의 SFTP 분리와 SSH 종료 확인이 실패해도 상대 터미널을 계속 쓴다", async () => {
    let failDetach = true;
    runSftp = async (command) => {
      if (command === "editor_sftp_attach") return { entries: [], parentEntryId: null };
      if (command === "editor_sftp_detach") {
        if (failDetach) throw new Error("분리 실패");
        return;
      }
      throw new Error(command);
    };
    const openBaseline = calls("open").length;
    const first = await openSsh("서버 A");
    const second = await openSsh("서버 B");
    // 서버 A만 오른쪽 그룹에 두고 서버 B는 왼쪽 그룹의 활성 탭으로 남긴다.
    await tabMenuAction("서버 A", "오른쪽으로 분리");
    expect([tabGroup("서버 B"), tabGroup("서버 A")]).toEqual([1, 2]);
    expect(focusedTabLabel()).toBe("서버 A");
    const split = find(".workspace-group-splitter");
    await press("SFTP 파일");
    expect(connectionCalls("editor_sftp_attach")).toEqual([["editor_sftp_attach", { sessionId: first.id }]]);
    const closeBaseline = calls("close").length;
    closeTerminal = async (id) => {
      if (id === first.id) throw new Error("종료 실패");
      return null;
    };
    await press("서버 A 닫기");
    expect(button("서버 A 종료 다시 확인")).toBeDefined();
    expect(calls("close").slice(closeBaseline)).toEqual([{ id: first.id }]);
    const remaining = find<HTMLTextAreaElement>('section[aria-label="서버 B"] textarea');
    await act(async () => remaining.focus());
    expect(connectionCalls("editor_sftp_attach")).toEqual([
      ["editor_sftp_attach", { sessionId: first.id }], ["editor_sftp_attach", { sessionId: second.id }],
    ]);
    await type(remaining, "READY");
    failDetach = false;
    closeTerminal = async () => null;
    await press("서버 A 종료 다시 확인");
    expect(button("서버 A")).toBeUndefined();
    expect(split.isConnected).toBe(false);
    expect(host.querySelector(".workspace-group-splitter")).toBeNull();
    expect(find('section[aria-label="서버 B"] textarea')).toBe(remaining);
    expect(remaining.closest("[hidden], [inert]")).toBeNull();
    expect(status()).toContain(second.id);
    expect(calls("open").slice(openBaseline)).toHaveLength(2);
    expect(calls("close").slice(closeBaseline)).toEqual([{ id: first.id }, { id: first.id }]);
    expect(calls("input")).toEqual([{ id: second.id, bytes: Array.from(new TextEncoder().encode("READY")) }]);
  });

  it("창 닫기를 승인한 뒤 도착한 SFTP와 S3 편집 핸들을 각각 반납한다", async () => {
    const sftpOpen = Promise.withResolvers<unknown>();
    const storageOpen = Promise.withResolvers<unknown>();
    const response = Promise.withResolvers<boolean>();
    respondToWindowClose = () => response.promise;
    runSftp = async command => {
      if (command === "editor_sftp_attach") return { entries: [{ entryId: "remote", name: "원격.txt", isDirectory: false }], parentEntryId: null };
      if (command === "editor_sftp_open") return sftpOpen.promise;
      if (command === "editor_sftp_close" || command === "editor_sftp_detach") return;
      throw new Error(command);
    };
    const location = { bucket: "bucket-0", prefix: "saved-0/", region: "ap-northeast-2" };
    let storageId = "";
    runStorage = async (command, args) => {
      if (command === "storage_open") {
        const request = args && typeof args === "object" && "request" in args ? args.request : undefined;
        if (!request || typeof request !== "object" || !("id" in request) || typeof request.id !== "string") throw new Error("개설 ID 없음");
        storageId = request.id;
        return { ok: true, value: { sessionId: storageId, profile: "dev", endpoint: "https://s3.example.invalid", region: location.region, location } };
      }
      if (command === "storage_list_objects") return {
        ok: true, value: {
          rows: [{ kind: "object", key: "saved-0/객체.txt", rowId: "object", bytes: 3 }], nextToken: null,
        }
      };
      if (command === "storage_count_previous_files") return { ok: true, value: 0 };
      if (command === "storage_edit_open") return storageOpen.promise;
      if (command === "storage_edit_close" || command === "storage_close") return true;
      throw new Error(command);
    };
    const ssh = await openSsh("서버 A");
    await press("SFTP 파일");
    await fileMenuAction("원격.txt", "편집");
    await openTarget("개발 버킷");
    await fileMenuAction("saved-0/객체.txt", "편집", find(".workspace-storage"));
    expect(connectionCalls("editor_sftp_open")).toHaveLength(1);
    expect(connectionCalls("storage_edit_open")).toHaveLength(1);

    await requestWindowClose("late-files");
    expect(closeReplies()).toEqual([{ action: "respondToClose", requestId: "late-files", decision: { kind: "ready" } }]);
    await act(async () => {
      sftpOpen.resolve({ handle: "late-sftp", name: "늦은 원격.txt", text: "원본" });
      storageOpen.resolve({ handle: "late-s3", label: "늦은 객체.txt", text: "원본" });
    });
    expect(button("늦은 원격.txt")).toBeUndefined();
    expect(button("늦은 객체.txt")).toBeUndefined();
    expect(connectionCalls("editor_sftp_close")).toEqual([["editor_sftp_close", { sessionId: ssh.id, handle: "late-sftp" }]]);
    expect(connectionCalls("storage_edit_close")).toEqual([["storage_edit_close", { request: { sessionId: storageId, handle: "late-s3" } }]]);
    await act(async () => response.resolve(true));
  });

  it("SFTP 종료 확인 뒤 해당 SSH만 닫고 원격 편집 버퍼와 파일별 닫기 흐름을 유지한다", async () => {
    const detached = Promise.withResolvers<void>();
    runSftp = async (command, args) => {
      switch (command) {
        case "editor_sftp_attach": {
          const { sessionId } = args as { sessionId: string };
          const name = sessionId === ssh.id ? "원격.txt" : "다른 서버.txt";
          return { entries: [{ entryId: "file", name, isDirectory: false }], parentEntryId: null };
        }
        case "editor_sftp_open": return { handle: "remote-file", name: "원격.txt", text: "원본" };
        case "editor_sftp_save": return { status: "saved" };
        case "editor_sftp_close": return;
        case "editor_sftp_detach": return detached.promise;
        default: throw new Error(command);
      }
    };
    const ssh = await openSsh("서버 A");
    await press("SFTP 파일");
    await fileMenuAction("원격.txt", "편집");
    expect(connectionCalls("editor_sftp_open")).toEqual([["editor_sftp_open", { sessionId: ssh.id, entryId: "file" }]]);
    expect(status()).toBe("SFTP");
    const model = editor.getModels().find(item => item.uri.query === "remote-file")!;
    await act(async () => model.setValue("원격 저장"));
    await press("저장");
    expect(connectionCalls("editor_sftp_save")).toEqual([["editor_sftp_save", { sessionId: ssh.id, handle: "remote-file", text: "원격 저장" }]]);
    expect(connectionCalls("editor_local_save")).toEqual([]);
    await act(async () => model.setValue("미저장 편집"));
    const other = await openSsh("서버 B");
    const otherFile = fileRow(host, "다른 서버.txt");
    await press("원격.txt");
    await press("서버 A 닫기");
    expect(button("서버 A 닫기")?.disabled).toBe(true);
    expect(connectionCalls("editor_sftp_detach")).toEqual([["editor_sftp_detach", { sessionId: ssh.id }]]);
    expect(calls("close").filter(args => args.id === ssh.id || args.id === other.id)).toEqual([]);
    await act(async () => detached.resolve());
    expect(button("서버 A")).toBeUndefined();
    expect(button("서버 B")).toBeDefined();
    expect(calls("close").filter(args => args.id === ssh.id || args.id === other.id)).toEqual([{ id: ssh.id }]);
    expect(connectionCalls("editor_sftp_detach")).toHaveLength(1);
    expect(button("원격.txt")?.getAttribute("aria-pressed")).toBe("true");
    expect(model.getValue()).toBe("미저장 편집");
    await press("서버 B");
    expect(fileRow(host, "다른 서버.txt")).toBe(otherFile);
    expect(otherFile.closest("[hidden]")).toBeNull();
    await press("원격.txt");

    const confirm = vi.spyOn(window, "confirm").mockReturnValueOnce(false).mockReturnValueOnce(true);
    await press("원격.txt 닫기");
    expect(button("원격.txt")).toBeDefined();
    expect(connectionCalls("editor_sftp_close")).toEqual([]);
    await press("원격.txt 닫기");
    expect(confirm).toHaveBeenCalledWith("원격.txt: 미저장 편집 내용을 버리고 닫을까요?");
    expect(connectionCalls("editor_sftp_close")).toEqual([["editor_sftp_close", { sessionId: ssh.id, handle: "remote-file" }]]);
    expect(connectionCalls("editor_local_close")).toEqual([]);
    expect(button("원격.txt")).toBeUndefined();
  });
});
