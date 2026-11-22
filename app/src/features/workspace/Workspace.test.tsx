// @vitest-environment jsdom
import type { EventCallback } from "@tauri-apps/api/event";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "../../App";
import { editor } from "../editor/monaco";
import type { SavedConnections } from "../connections/connectionIpc";
import type { ConnectionRequest, ConnectionTestDraft, ConnectionTestRun } from "../connections/profiles";

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
  native.listen.mockImplementation(async (event, receive) => {
    if (event === "editor-local-cli-open") receiveCliOpen = receive;
    return () => { receiveCliOpen = undefined; };
  });
  runCliEditor = async command => { throw new Error(`예상하지 않은 CLI 편집 명령: ${command}`); };
  openFile = async () => ({ status: "cancelled" });
  saveFile = async () => null;
  native.invoke.mockImplementation(async (command, args) => {
    if (command === "connections_load") return loadProfiles();
    if (command === "ai_models" || command === "ai_targets" || command === "ai_decisions") return [];
    if (command.startsWith("connection_") || ["verify_ssh_host", "commit_ssh_host", "cancel_ssh_host", "register_saved_ssh_host"].includes(command)) return runConnectionCommand(command, args);
    if (command === "ssm_discovery") return runDiscovery(args);
    if (command.startsWith("storage_")) return runStorage(command, args);
    if (command.startsWith("editor_sftp_")) return runSftp(command, args);
    if (command.startsWith("tunnel_")) return runTunnel(command, args);
    if (command === "editor_local_cli_pending") return [];
    if (command.startsWith("editor_local_cli_")) return runCliEditor(command, args);
    if (command === "editor_local_open") return openFile();
    if (command === "editor_local_save") return saveFile();
    if (command === "terminal_start") return starts.get(args.id);
    if (command === "terminal_close") return closeTerminal(args.id);
    if (command !== "terminal_open") return;
    const id = `pty-${channels.size + 10}`;
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

async function press(name: string) {
  const control = button(name);
  expect(control, name).toBeDefined();
  await act(async () => { control!.click(); });
}

async function chooseFileConnection(targetId: string) {
  const select = find<HTMLSelectElement>('select[aria-label="파일 연결"]');
  await act(async () => {
    select.value = targetId;
    select.dispatchEvent(new Event("change", { bubbles: true }));
  });
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

const status = () => find("footer").textContent;
const tree = () => find('[role="tree"]');
async function openSecond() {
  const group = find('[role="treeitem"][aria-expanded]');
  await act(async () => { group.focus(); });
  await key(group, "ArrowRight");
  expect(document.activeElement).toBe(find('[role="treeitem"]:not([aria-expanded])'));
  await key(document.activeElement as HTMLElement, "Enter");
}

describe("중앙 원격 화면", () => {
  function dispatchTrusted(target: HTMLElement, event: Event) {
    // jsdom의 공개 dispatchEvent는 항상 isTrusted를 지운다. 실제 컴포넌트의 신뢰 검사는 유지한다.
    const impl = Object.getOwnPropertySymbols(event).find(symbol => symbol.description === "impl");
    if (!impl) throw new Error("jsdom 이벤트 구현을 찾을 수 없습니다");
    const state = Reflect.get(event, impl);
    state.isTrusted = true;
    Reflect.get(target, impl)._dispatch(state);
  }

  it.each(["rdp", "vnc"] as const)("%s 엔진의 프레임·입력·종료가 탭 수명을 따른다", async (protocol) => {
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
    native.invoke.mockImplementation((command, args) => {
      if (command === "rdp_prepare") return Promise.resolve(identity);
      if (command === "vnc_create") return Promise.resolve(owner);
      if (command === `${protocol}_connect`) {
        output = args.output;
        return protocol === "rdp" ? pendingRdp.promise : Promise.resolve();
      }
      if (command === `${protocol}_close`) pendingRdp.resolve();
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
    await press(`${protocol.toUpperCase()} 탭 열기`);
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
        ? { id: identity, draft: { host: "desktop.example.invalid", port: 3389 }, output }
        : { owner, draft: { host: "desktop.example.invalid", port: 5900, password: true }, output },
      { headers: { [`x-zzabba-${protocol}-document`]: proof } },
    ]]);
    if (protocol === "vnc") await deliver({ identity, kind: "started", serial: 0 });
    const canvas = find<HTMLCanvasElement>('canvas[aria-label="원격 화면"]');
    expect(canvas.tabIndex).toBe(-1);
    expect(pane.textContent).toContain("첫 화면 대기 중");
    expect(pane.textContent).not.toContain("연결됨");

    if (protocol === "rdp") {
      await deliver({ sequence: 1, kind: "frame", full: true, frameSequence: 1, width: 1, height: 1, x: 0, y: 0, w: 1, h: 1 });
      const chunk = new ArrayBuffer(16);
      new DataView(chunk).setBigUint64(0, 1n, true);
      new Uint8Array(chunk, 12).set([1, 2, 3, 255]);
      await deliver(chunk);
      await deliver({ sequence: 2, kind: "display" });
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
    const choice = find<HTMLSelectElement>(".workspace-center-actions select");
    expect([...choice.options].map(option => option.text)).toEqual(["선택", "로컬 1"]);
    await act(async () => {
      choice.selectedIndex = 1;
      choice.dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(terminal.closest('[data-pane="left"]')).not.toBeNull();
    const boundary = find(".splitter-center");
    await press(label);
    expect(find(".splitter-center")).toBe(boundary);
    expect(host.querySelector(".workspace-center-split")).toBeNull();
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

    await press(`${label} 닫기`);
    expect(pane.isConnected).toBe(false);
    expect(connectionCalls(`${protocol}_close`)).toEqual([[
      `${protocol}_close`, protocol === "rdp" ? { id: identity } : { owner, generation: null },
      { headers: { [`x-zzabba-${protocol}-document`]: proof } },
    ]]);
    expect(document.activeElement).toBe(terminal);
    expect(terminal.closest('[data-pane="left"]')).not.toBeNull();
    expect(find(".splitter-center")).toBe(boundary);
    expect(boundary.parentElement?.classList.contains("workspace-center-split")).toBe(true);
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
    const selected = find('button[aria-pressed="true"]').parentElement!;
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
    const treeSplitter = find('[role="separator"][aria-label="트리 너비"]');
    treeSplitter.setPointerCapture = vi.fn();
    await act(async () => {
      treeSplitter.dispatchEvent(new PointerEvent("pointerdown", { pointerId: 1, clientX: 280, bubbles: true }));
      treeSplitter.dispatchEvent(new PointerEvent("pointermove", { pointerId: 1, clientX: 340, bubbles: true }));
      treeSplitter.dispatchEvent(new PointerEvent("lostpointercapture", { pointerId: 1, bubbles: true }));
    });
    await key(find('[role="separator"][aria-label="AI 열 너비"]'), "ArrowLeft");
    expect(host.querySelector('[role="separator"][aria-label="AI 채팅 높이"]')).toBeNull();
    const values = () => ["트리 너비", "AI 열 너비"].map((label) =>
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
    expect(native.invoke).not.toHaveBeenCalledWith("workspace_window", expect.anything());
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
    const aiTarget = find<HTMLSelectElement>(".a01-proposals select");
    await act(async () => {
      aiTarget.selectedIndex = 1;
      aiTarget.dispatchEvent(new Event("change", { bubbles: true }));
    });
    const aiValue = aiTarget.value;
    expect(aiValue).not.toBe("");

    await openSecond();
    await press("로컬 1");
    const left = find<HTMLTextAreaElement>('section[aria-label="로컬 1"] textarea');
    const right = find<HTMLTextAreaElement>('section[aria-label="로컬 2"] textarea');
    await type(left, "echo ");
    const choice = find<HTMLSelectElement>(".workspace-center-actions select");
    expect([...choice.options].map((option) => option.text)).toEqual(["선택", "로컬 2"]);
    await act(async () => {
      choice.selectedIndex = 1;
      choice.dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(left.closest('[data-pane="left"]')).not.toBeNull();
    expect(right.closest('[data-pane="right"]')).not.toBeNull();
    expect(right.closest("[hidden], [inert]")).toBeNull();
    expect(document.activeElement).toBe(left);
    await type(right, "UNFOCUSED");
    expect(calls("input")).toEqual([{ id: "pty-10", bytes: Array.from(new TextEncoder().encode("echo ")) }]);
    const boundary = find('[role="separator"][aria-label="중앙 터미널 분할"]');
    expect(boundary.getAttribute("aria-orientation")).toBe("vertical");
    await act(async () => boundary.focus());
    await key(boundary, "ArrowRight");
    expect(boundary.getAttribute("aria-valuenow")).toBe("55");
    expect(document.activeElement).toBe(boundary);
    vi.spyOn(boundary.parentElement!, "clientWidth", "get").mockReturnValue(508);
    vi.spyOn(boundary, "offsetWidth", "get").mockReturnValue(8);
    boundary.setPointerCapture = vi.fn();
    await act(async () => {
      boundary.dispatchEvent(new PointerEvent("pointerdown", { pointerId: 1, clientX: 250, bubbles: true }));
      boundary.dispatchEvent(new PointerEvent("pointermove", { pointerId: 1, clientX: 300, bubbles: true }));
      boundary.dispatchEvent(new PointerEvent("lostpointercapture", { pointerId: 1, bubbles: true }));
    });
    expect(boundary.getAttribute("aria-valuenow")).toBe("65");

    await act(async () => right.focus());
    await type(right, "RIGHT");
    expect(document.activeElement).toBe(right);
    expect(button("로컬 2")?.getAttribute("aria-pressed")).toBe("true");
    expect(status()).toContain("pty-11");
    await press("집중 보기");
    await press("기본 보기");
    expect(find(".splitter-center")).toBe(boundary);
    expect(boundary.getAttribute("aria-valuenow")).toBe("65");
    await press("로컬 1");
    expect(document.activeElement).toBe(left);
    await press("분할 닫기");
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
    expect(aiTarget.value).toBe(aiValue);
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
    for (const selector of ['.workspace-tab button', '.workspace-desktop-actions button', '.workspace-mode', '.window-control.close']) {
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
    expect(groups.map((group) => group.firstElementChild?.textContent)).toEqual(["▾ 회사 개발alpha", "▾ 회사 개발beta"]);
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
    expect([...tree().querySelectorAll('[role="treeitem"] > .tree-row > span:not([aria-hidden])')].map(item => item.textContent)).toEqual(["로컬", "내 PC"]);
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
    await select("접속 방식", "ssm");
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
    await select("접속 방식", "ssm");
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
    await select("접속 방식", "ssm");
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
    await press("파일");
    await chooseFileConnection("legacy");
    expect(find('section[aria-label="AWS S3"] [role="alert"]').textContent).toContain("다시 시험하고 저장");
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
    await press("파일");
    await chooseFileConnection("saved-ssm");
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
    await press("파일");
    await chooseFileConnection("saved-ssm");
    expect(connectionCalls("storage_relogin")).toHaveLength(0);
    await press("SSO 재로그인");
    expect(connectionCalls("storage_relogin")).toEqual([["storage_relogin", { request: { id: storageOpenId() } }]]);
    expect(find<HTMLSelectElement>('select[aria-label="파일 연결"]').value).toBe("saved-ssm");
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
    await press("파일");
    await chooseFileConnection("saved-ssm");
    expect(connectionCalls("storage_open")).toEqual([["storage_open", { request: { targetId: "saved-ssm", id: session.sessionId } }]]);
    await changeInput("버킷", "test-bucket");
    await press("버킷 열기");
    await press(key);
    await press("다운로드");
    openFile = async () => ({ status: "opened", handle: "local-fixture", name: "격리.txt", text: "원본" });
    await press("파일 열기");
    const model = editor.getModels().find(item => item.uri.query === "local-fixture")!;
    await act(async () => model.setValue("미저장 편집"));
    expect(button("격리.txt")?.getAttribute("aria-pressed")).toBe("true");
    await press("AI 채팅");
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
    await press("파일");
    expect(find('section[aria-label="AWS S3"]').textContent).toContain("다운로드 완료: 7 바이트 · SDK 체크섬 검증됨");
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
      .toEqual(["AWS Systems Manager", "터널 열기"]);
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
    const aiSession = find<HTMLSelectElement>(`.workspace-chat select:has(option[value="${sessionId}"])`);
    expect([...aiSession.options].map(option => option.value)).toEqual(["", sessionId]);
    await press("RDP 탭 열기");
    expect([...aiSession.options].map(option => option.value)).toEqual(["", sessionId]);
    await press("RDP 3 닫기");
    expect(document.activeElement).toBe(field);
    await press("파일");
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
    await press("파일 열기");
    expect(button("관리 노드")?.getAttribute("aria-pressed")).toBe("true");
    openFile = async () => ({ status: "opened", handle: "selected-file", name: "시험.txt", text: "원본" });
    await press("파일 열기");
    const model = editor.getModels().find((item) => item.uri.query === "selected-file")!;
    await act(async () => model.setValue("첫 편집"));
    await press("로컬 1");
    expect(document.activeElement).toBe(localInput);
    await press("시험.txt");
    expect(find('.editor-tab[aria-label="시험.txt"]').contains(document.activeElement)).toBe(true);
    expect(model.getValue()).toBe("첫 편집");
    await press("저장");
    expect(connectionCalls("editor_local_save")).toEqual([["editor_local_save", { handle: "selected-file", text: "첫 편집" }]]);
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
    runConnectionCommand = async () => { throw "cancelled"; };
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
    await select("접속 방식", "ssh");
    expect(find<HTMLOptionElement>('option[value="ssm"][disabled]').textContent).toContain("SSH-over-SSM");
    await changeInput("서버 호스트", "dev.example.invalid");
    await changeInput("SSH 포트", "2222");
    await changeInput("SSH 계정", "deploy");
    await changeInput("호스트 키 SHA256 지문", fingerprint);
    await press("다음");
    const auth = await select("SSH 인증 방식", "manualPassword");
    expect(auth.querySelector<HTMLOptionElement>('option[value="storedPassword"]')?.disabled).toBe(true);
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
    await select("접속 방식", "ssh");
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
      targets: [{ revision: 1, profile: {
        id: "server", groupId: "work", displayName: "개발 서버", defaultMethod: "ssh",
        ssh: {
          host: "dev.example.invalid", port: 2222, account: "deploy", hostKeyFingerprint: fingerprint,
          transport: { kind: "direct" }, auth: { kind: "manualPassword" },
        },
      } }],
    });
    expect(tree().textContent).not.toContain("개발 서버");
    await press("새로고침");
    const target = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]')].find(item => item.textContent === "개발 서버")!;
    expect(target).toBeDefined();

    await key(target, "ContextMenu");
    await key(find('[role="menuitem"]'), "Escape");
    expect(host.querySelector('[role="menu"]')).toBeNull();
    expect(connectionCalls("register_saved_ssh_host")).toEqual([]);

    await key(target, "ContextMenu");
    expect([...find('[role="menu"]').querySelectorAll('[role="menuitem"]')].map(item => item.textContent))
      .toEqual(["SSH", "호스트 키 등록", "터널 열기"]);
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
    const row = [...tree().querySelectorAll<HTMLElement>('[role="treeitem"]:not([aria-expanded])')]
      .find(item => item.querySelector(":scope > .tree-row > span")?.textContent === label);
    expect(row, label).toBeDefined();
    await key(row!, "Enter");
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
    await press("파일");
    const fileConnection = find('select[aria-label="파일 연결"]');
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
    expect(find('select[aria-label="파일 연결"]')).toBe(fileConnection);
    expect(fileConnection.closest("[hidden], [inert]")).toBeNull();
    await press("로컬 2");
    expect([...find<HTMLSelectElement>(".workspace-center-actions select").options].map(option => option.text))
      .toEqual(["선택", "로컬 1"]);
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
    expect(find('select[aria-label="파일 연결"]')).toBe(fileConnection);
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
    await press("파일");
    const sftp = find<HTMLButtonElement>('[role="tab"][aria-selected="true"]');
    expect(sftp.textContent).toBe("파일");
    expect(document.activeElement).toBe(sftp);
    expect(find('section[aria-label="서버 A"]').closest("[hidden]")).toBeNull();
    expect(draft.closest("[hidden][inert]")).not.toBeNull();
    const firstFile = button(`${first.id}.txt`)!;
    await press(`${first.id}.txt`);
    expect(firstFile.getAttribute("aria-pressed")).toBe("true");
    await press("AI 채팅");
    expect(firstFile.closest("[hidden][inert]")).not.toBeNull();
    expect(draft.value).toBe("유지할 채팅 초안");
    expect(connectionCalls("editor_sftp_detach")).toEqual([]);
    await press("파일");
    expect(button(`${first.id}.txt`)).toBe(firstFile);
    expect(firstFile.getAttribute("aria-pressed")).toBe("true");

    const second = await openSsh("서버 B");
    expect(firstFile.closest("[hidden][inert]")).not.toBeNull();
    const secondFile = button(`${second.id}.txt`)!;
    expect(secondFile.closest("[hidden]")).toBeNull();
    expect(firstFile.closest("[hidden][inert]")).not.toBeNull();
    await press("서버 A");
    expect(button(`${first.id}.txt`)).toBe(firstFile);
    expect(firstFile.closest("[hidden]")).toBeNull();
    expect(secondFile.closest("[hidden][inert]")).not.toBeNull();
    expect(connectionCalls("editor_sftp_attach")).toEqual([
      ["editor_sftp_attach", { sessionId: first.id }], ["editor_sftp_attach", { sessionId: second.id }],
    ]);

    await act(async () => second.receive({ kind: "exited", value: 0 }));
    expect(button(`${second.id}.txt`)).toBeUndefined();
    expect(firstFile.closest("[hidden]")).toBeNull();
    expect(connectionCalls("editor_sftp_detach")).toEqual([["editor_sftp_detach", { sessionId: second.id }]]);
    expect(calls("close").some(args => args.id === second.id)).toBe(false);
    await press("서버 B");
    expect(find(".workspace-file-status").textContent).toBe("활성 SSH 연결 없음");
    expect(find('textarea[aria-label="메시지 입력"]')).toBe(draft);
    expect(draft.value).toBe("유지할 채팅 초안");
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
    const choice = find<HTMLSelectElement>(".workspace-center-actions select");
    await act(async () => {
      choice.value = [...choice.options].find(option => option.text === "서버 B")!.value;
      choice.dispatchEvent(new Event("change", { bubbles: true }));
    });
    const right = find<HTMLTextAreaElement>('section[aria-label="서버 B"] textarea');
    const split = find(".splitter-center");
    const openedTerminals = calls("open").length;
    const visible = (element: HTMLElement) => element.closest("[hidden], [inert]") === null;
    await press("파일");
    await press(`폴더 ${first.id}`);
    const firstPath = find(".sftp-location span");
    expect(visible(firstPath)).toBe(true);

    await openTarget("개발 버킷");
    const pinned = find(".workspace-storage");
    const object = button("saved-0/notes.txt")!;
    await press("saved-0/notes.txt");
    expect(visible(pinned)).toBe(true);
    expect(visible(firstPath)).toBe(false);
    expect(button("서버 A")?.getAttribute("aria-pressed")).toBe("true");
    await press("VNC 탭 열기");
    expect(visible(pinned)).toBe(true);
    expect(visible(firstPath)).toBe(false);
    expect(right.closest("[hidden][inert]")).not.toBeNull();
    await press("서버 A");
    expect(find(".workspace-storage")).toBe(pinned);
    await act(async () => right.focus());
    expect(visible(pinned)).toBe(true);
    expect(status()).toContain(second.id);
    expect(connectionCalls("editor_sftp_attach")).toEqual([["editor_sftp_attach", { sessionId: first.id }]]);
    await press("AI 채팅");
    expect(visible(pinned)).toBe(false);
    expect(pinned.isConnected).toBe(true);
    await press("파일");
    expect(visible(pinned)).toBe(true);

    await chooseFileConnection("");
    await press(`폴더 ${second.id}`);
    const secondPath = find('.workspace-sftp:not([hidden]) .sftp-location span');
    expect(visible(secondPath)).toBe(true);
    expect(visible(firstPath)).toBe(false);
    expect(visible(pinned)).toBe(false);
    await chooseFileConnection("s3-0");
    await openTarget("개발 버킷");
    expect(find(".workspace-storage")).toBe(pinned);
    expect(button("saved-0/notes.txt")).toBe(object);
    expect(object.getAttribute("aria-pressed")).toBe("true");
    expect(find(".storage-location").textContent).toContain("saved-0/");
    expect(visible(firstPath)).toBe(false);
    expect(visible(secondPath)).toBe(false);
    expect(connectionCalls("storage_open")).toHaveLength(1);
    expect(connectionCalls("storage_list_objects")).toHaveLength(1);

    await openTarget("격리 버킷");
    const other = find(".workspace-storage:not([hidden])");
    expect(other).not.toBe(pinned);
    expect(visible(pinned)).toBe(false);
    expect(connectionCalls("storage_open")).toHaveLength(2);
    await press("격리 버킷 닫기");
    expect(find(".workspace-storage:not([hidden])")).toBe(other);
    expect(find(".workspace-storage [role=alert]").textContent?.trim()).toBe("종료 확인 불가");
    failClose = false;
    await press("격리 버킷 종료 다시 확인");
    const secondOpen = connectionCalls("storage_open")[1]?.[1]?.request as { id: string };
    const closeRequest = ["storage_close", { request: { sessionId: secondOpen.id } }];
    expect(connectionCalls("storage_close")).toEqual([closeRequest, closeRequest]);
    expect(other.isConnected).toBe(false);
    await chooseFileConnection("s3-0");
    expect(find(".workspace-storage:not([hidden])")).toBe(pinned);
    expect(connectionCalls("editor_sftp_attach")).toHaveLength(2);
    expect(connectionCalls("editor_sftp_detach")).toHaveLength(0);
    expect(calls("open")).toHaveLength(openedTerminals);
    expect(find(".splitter-center")).toBe(split);
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
    await press("서버 A");
    await press("파일");
    const choice = find<HTMLSelectElement>(".workspace-center-actions select");
    await act(async () => {
      choice.value = [...choice.options].find((option) => option.text === "서버 B")!.value;
      choice.dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(connectionCalls("editor_sftp_attach")).toEqual([["editor_sftp_attach", { sessionId: first.id }]]);
    const closeBaseline = calls("close").length;
    closeTerminal = async (id) => {
      if (id === first.id) throw new Error("종료 실패");
      return null;
    };
    await press("서버 A 닫기");
    expect(button("서버 A 종료 다시 확인")).toBeDefined();
    expect(calls("close").slice(closeBaseline)).toEqual([{ id: first.id }]);
    const right = find<HTMLTextAreaElement>('section[aria-label="서버 B"] textarea');
    await act(async () => right.focus());
    expect(connectionCalls("editor_sftp_attach")).toEqual([
      ["editor_sftp_attach", { sessionId: first.id }], ["editor_sftp_attach", { sessionId: second.id }],
    ]);
    await type(right, "READY");
    failDetach = false;
    closeTerminal = async () => null;
    await press("서버 A 종료 다시 확인");
    expect(button("서버 A")).toBeUndefined();
    expect(host.querySelector(".splitter-center")).toBeNull();
    expect(find('section[aria-label="서버 B"] textarea')).toBe(right);
    expect(right.closest("[hidden], [inert]")).toBeNull();
    expect(status()).toContain(second.id);
    expect(calls("open").slice(openBaseline)).toHaveLength(2);
    expect(calls("close").slice(closeBaseline)).toEqual([{ id: first.id }, { id: first.id }]);
    expect(calls("input")).toEqual([{ id: second.id, bytes: Array.from(new TextEncoder().encode("READY")) }]);
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
    await press("파일");
    await key(button("원격.txt")!, "ContextMenu");
    await press("편집");
    expect(connectionCalls("editor_sftp_open")).toEqual([["editor_sftp_open", { sessionId: ssh.id, entryId: "file" }]]);
    expect(status()).toBe("SFTP");
    const model = editor.getModels().find(item => item.uri.query === "remote-file")!;
    await act(async () => model.setValue("원격 저장"));
    await press("저장");
    expect(connectionCalls("editor_sftp_save")).toEqual([["editor_sftp_save", { sessionId: ssh.id, handle: "remote-file", text: "원격 저장" }]]);
    expect(connectionCalls("editor_local_save")).toEqual([]);
    await act(async () => model.setValue("미저장 편집"));
    const other = await openSsh("서버 B");
    const otherFile = button("다른 서버.txt");
    expect(otherFile).toBeDefined();
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
    expect(button("다른 서버.txt")).toBe(otherFile);
    expect(otherFile?.closest("[hidden]")).toBeNull();
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
