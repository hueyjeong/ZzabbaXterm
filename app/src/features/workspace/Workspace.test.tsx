// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "../../App";
import { editor } from "../editor/monaco";
import type { SavedConnections } from "../connections/connectionIpc";
import type { ConnectionRequest, ConnectionTestDraft, ConnectionTestRun } from "../connections/profiles";

const native = vi.hoisted(() => ({
  invoke: vi.fn(),
  fit: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({
  isTauri: () => true,
  invoke: native.invoke,
  Channel: class { constructor(readonly receive: (message: unknown) => void) { } },
}));
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
  openFile = async () => ({ status: "cancelled" });
  saveFile = async () => null;
  native.invoke.mockImplementation(async (command, args) => {
    if (command === "connections_load") return loadProfiles();
    if (command === "ai_models" || command === "ai_targets" || command === "ai_decisions") return [];
    if (command.startsWith("connection_") || ["verify_ssh_host", "commit_ssh_host", "cancel_ssh_host", "register_saved_ssh_host"].includes(command)) return runConnectionCommand(command, args);
    if (command === "ssm_discovery") return runDiscovery(args);
    if (command.startsWith("storage_")) return runStorage(command, args);
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
    for (const [label, arrow] of [["AI 열 너비", "ArrowLeft"], ["AI 채팅 높이", "ArrowDown"]]) {
      await key(find(`[role="separator"][aria-label="${label}"]`), arrow);
    }
    const values = () => ["트리 너비", "AI 열 너비", "AI 채팅 높이"].map((label) =>
      find(`[role="separator"][aria-label="${label}"]`).getAttribute("aria-valuenow"));
    expect(values()).toEqual(["340", "380", "55"]);
    await act(async () => { viewportWidth = 660; observeWorkspace(); });
    expect(values()).toEqual(["211", "211", "55"]);
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
    expect(values()).toEqual(["211", "211", "55"]);
    await act(async () => { viewportWidth = 1200; observeWorkspace(); });
    expect(values()).toEqual(["340", "380", "55"]);
    expect(tree().closest("[hidden]")).toBeNull();
    expect(panel.querySelector("textarea")).toBe(input);
    expect(input.value).toBe("echo B_READY");
    expect(draft.value).toBe("CHAT_ONLY");
    expect(calls("open")).toHaveLength(2);
    expect(calls("close")).toHaveLength(0);
    expect(calls("input")).toEqual([{ id: "pty-11", bytes: Array.from(new TextEncoder().encode("echo B_READY")) }]);
    expect(native.invoke).not.toHaveBeenCalledWith("workspace_window", expect.anything());
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
    expect(status()).toBe("연결 없음AI 전용 셸 · 대기");
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
    for (const selector of ['.workspace-tab button', '.workspace-mode', '.window-control.close']) {
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
    expect(find(".workspace-tree").textContent).toContain("저장된 연결 없음");
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
    expect(find(".workspace-tree").textContent).not.toContain("저장된 연결 없음");
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
    await press("스토리지 탭 추가");
    const select = find<HTMLSelectElement>('section[aria-label="AWS S3"] select');
    await act(async () => {
      select.value = "legacy";
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await press("스토리지 연결");
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
    await press("스토리지 탭 추가");
    const select = find<HTMLSelectElement>('section[aria-label="AWS S3"] select');
    await act(async () => {
      select.value = "saved-ssm";
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await press("스토리지 연결");
    const id = storageOpenId();
    await press("S3 2 닫기");
    expect(button("S3 2")).toBeUndefined();
    expect(connectionCalls("storage_close")).toEqual([["storage_close", { request: { sessionId: id } }]]);
    await act(async () => opening.resolve({ ok: true, value: { sessionId: id, profile: node.profile, region: node.region, endpoint: "https://s3.example.invalid" } }));
    expect(button("S3 2")).toBeUndefined();
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
    await press("스토리지 탭 추가");
    const select = find<HTMLSelectElement>('section[aria-label="AWS S3"] select');
    await act(async () => {
      select.value = "saved-ssm";
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await press("스토리지 연결");
    expect(connectionCalls("storage_relogin")).toHaveLength(0);
    await press("SSO 재로그인");
    expect(connectionCalls("storage_relogin")).toEqual([["storage_relogin", { request: { id: storageOpenId() } }]]);
    expect(select.value).toBe("saved-ssm");
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
    await press("스토리지 탭 추가");
    const select = find<HTMLSelectElement>('section[aria-label="AWS S3"] select');
    await act(async () => {
      select.value = "saved-ssm";
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await press("스토리지 연결");
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
    await act(async () => picker.resolve({ kind: "approved", approvalId: "approved-id" }));
    expect(connectionCalls("storage_download")).toEqual([["storage_download", {
      request: {
        sessionId: session.sessionId, bucket: "test-bucket", region: node.region, key, approvalId: "approved-id",
      }
    }]]);
    expect(connectionCalls("storage_close")).toHaveLength(0);
    await press("로컬 1");
    expect(find('section[aria-label="로컬 1"] textarea')).toBe(document.activeElement);
    await act(async () => downloaded.resolve({ ok: true, value: { bytes: 7, integrity: "sdkChecksumVerified" } }));
    await press("S3 2");
    expect(find('section[aria-label="AWS S3"]').textContent).toContain("다운로드 완료: 7 바이트 · SDK 체크섬 검증됨");
    await press("S3 2 닫기");
    expect(connectionCalls("storage_close")).toEqual([["storage_close", { request: { sessionId: session.sessionId } }]]);
    expect(button("S3 2")).toBeUndefined();
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
    expect(find('[role="menu"]').textContent).toBe("AWS Systems Manager");
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
    expect([...find('[role="menu"]').querySelectorAll('[role="menuitem"]')].map(item => item.textContent)).toEqual(["SSH", "호스트 키 등록"]);
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
