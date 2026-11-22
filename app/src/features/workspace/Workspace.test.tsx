// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "../../App";
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
let runDiscovery: (args: unknown) => Promise<unknown>;

beforeEach(async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
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
  runDiscovery = async () => { throw new Error("예상하지 않은 SSM 검색"); };
  native.invoke.mockImplementation(async (command, args) => {
    if (command === "connections_load") return loadProfiles();
    if (command.startsWith("connection_")) return runConnectionCommand(command, args);
    if (command === "ssm_discovery") return runDiscovery(args);
    if (command === "terminal_start") return starts.get(args.id);
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
    async function select(label: string, value: string) {
      const control = [...find("dialog").querySelectorAll("label")]
        .find((item) => item.firstChild?.textContent?.trim() === label)?.querySelector("select");
      expect(control?.disabled).toBe(false);
      await act(async () => {
        if (!control) throw new Error(label);
        control.value = value;
        control.dispatchEvent(new Event("change", { bubbles: true }));
      });
      return control;
    }

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
    expect(find("dialog").textContent).toContain(`${node.profile} / ${node.region} / ${node.instanceId}`);
    await press("취소");
    expect(host.querySelector("dialog")).toBeNull();
    expect(connectionCalls("connection_save")).toHaveLength(0);
  });

  it("저장된 SSM 행의 문맥 메뉴는 SSM만 제공하고 열기에는 저장 대상 ID만 전달한다", async () => {
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
  });
});
