// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { TerminalView } from "./TerminalView";
import type { LocalTerminalIpc } from "./session";

vi.hoisted(() => {
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(null);
});

it("실제 xterm에서 Enter와 Shift+Enter를 구분하고 조합 중이거나 준비되지 않은 Shift+Enter를 보내지 않는다", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("matchMedia", () => ({ matches: false, addListener() {}, removeListener() {} }));
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(800);
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(400);
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const open = vi.fn<LocalTerminalIpc["open"]>().mockResolvedValue("remote");
  const input = vi.fn<LocalTerminalIpc["input"]>().mockResolvedValue(undefined);
  const ipc: LocalTerminalIpc = {
    open, input, start: vi.fn().mockResolvedValue(undefined), ack: vi.fn().mockResolvedValue(undefined),
    resize: vi.fn().mockResolvedValue(undefined), close: async () => null,
  };

  try {
    const render = (active = true, closing = false) => act(async () => {
      root.render(<TerminalView active={active} closing={closing} focusRequested={false} label="원격 셸" ipc={ipc} remote />);
    });
    await render();
    const textarea = host.querySelector("textarea");
    if (!textarea) throw new Error("xterm 입력란 없음");
    const enter = (init: KeyboardEventInit = {}, type = "keydown") => textarea.dispatchEvent(new KeyboardEvent(type, {
      key: "Enter", keyCode: 13, bubbles: true, cancelable: true, ...init,
    }));
    const shift = { shiftKey: true };
    enter(shift);
    const [, receive] = open.mock.calls[0];
    await act(async () => { receive({ kind: "ready" }); });

    enter();
    expect(enter(shift)).toBe(false);
    enter(shift, "keyup");
    enter({ ...shift, altKey: true });
    textarea.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
    enter({ ...shift, isComposing: true });
    enter({ ...shift, keyCode: 229 });
    textarea.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true }));

    await render(false);
    enter(shift);
    await render(true, true);
    enter(shift);
    await render();
    await act(async () => { expect(enter(shift)).toBe(false); });
    await vi.waitFor(() => expect(input.mock.calls.map(([id, bytes]) => [id, new TextDecoder().decode(bytes)])).toEqual([
      ["remote", "\r"], ["remote", "\x1b[13;2u"], ["remote", "\x1b\r"], ["remote", "\x1b[13;2u"],
    ]));
  } finally {
    await act(async () => { root.unmount(); });
    host.remove();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  }
});

it("우클릭은 포커스가 있던 준비된 터미널에서만 붙여넣고, 창을 고르는 우클릭과 Shift 우클릭은 붙여넣지 않는다", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("matchMedia", () => ({ matches: false, addListener() {}, removeListener() {} }));
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(800);
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(400);
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const open = vi.fn<LocalTerminalIpc["open"]>().mockResolvedValue("remote");
  const input = vi.fn<LocalTerminalIpc["input"]>().mockResolvedValue(undefined);
  const ipc: LocalTerminalIpc = {
    open, input, start: vi.fn().mockResolvedValue(undefined), ack: vi.fn().mockResolvedValue(undefined),
    resize: vi.fn().mockResolvedValue(undefined), close: async () => null,
  };
  const clipboard = { readText: vi.fn().mockResolvedValue("whoami"), writeText: vi.fn().mockResolvedValue(undefined) };
  const pasted = () => input.mock.calls.map(([, bytes]) => new TextDecoder().decode(bytes)).filter((text) => text.includes("whoami"));
  try {
    await act(async () => {
      root.render(<TerminalView active focusRequested={false} label="원격 셸" ipc={ipc} remote clipboard={clipboard} />);
    });
    const textarea = host.querySelector("textarea");
    const screen = host.querySelector<HTMLElement>(".xterm");
    if (!textarea || !screen) throw new Error("xterm 요소 없음");
    const press = (init: MouseEventInit = {}) => {
      screen.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 2, ...init }));
      const menu = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2, ...init });
      screen.dispatchEvent(menu);
      return menu.defaultPrevented;
    };
    // 준비 전 원격 탭은 클립보드를 읽지 않는다.
    textarea.focus();
    expect(press()).toBe(true);
    expect(clipboard.readText).not.toHaveBeenCalled();

    const [, receive] = open.mock.calls[0];
    await act(async () => { receive({ kind: "ready" }); });
    // 포커스가 없던 터미널을 고르는 우클릭은 붙여넣지 않는다.
    textarea.blur();
    press();
    await act(async () => { await Promise.resolve(); });
    expect(pasted()).toEqual([]);
    // Shift 우클릭은 기본 메뉴를 남기고 붙여넣지 않는다.
    textarea.focus();
    expect(press({ shiftKey: true })).toBe(false);
    // 포커스가 있던 준비된 터미널은 붙여넣는다.
    expect(press()).toBe(true);
    await vi.waitFor(() => expect(pasted()).toEqual(["whoami"]));
  } finally {
    await act(async () => { root.unmount(); });
    host.remove();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  }
});
