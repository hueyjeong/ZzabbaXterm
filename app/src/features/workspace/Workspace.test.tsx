// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "../../App";

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

beforeEach(async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  viewportWidth = 1200;
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
  native.invoke.mockImplementation(async (command, args) => {
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
