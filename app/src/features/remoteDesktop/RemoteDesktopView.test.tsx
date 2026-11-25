// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { RemoteDesktopView } from "./RemoteDesktopView";
import { createRemoteDesktopViewSession, type RemoteDesktopEvent, type RemoteDesktopSession } from "./session";
import { connect, type Connection } from "../vnc/session";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn<(name: string, args: Record<string, unknown>) => Promise<void>>() }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke, Channel: class {
    constructor(readonly onmessage: (message: unknown) => void) { }
  }
}));
vi.mock("../vnc/document", () => ({
  documentProof: () => "a".repeat(32),
  invokeVnc: (name: string, args: Record<string, unknown>) => invoke(name, args),
  readPacket: async (_proof: string, packet: unknown) => packet,
}));

function fakeSession(generation = 1) {
  let receive: (event: RemoteDesktopEvent) => boolean = () => false;
  const session = {
    sessionId: "desktop",
    generation,
    subscribe(listener: typeof receive) {
      receive = listener;
      return stop;
    },
    send: vi.fn<RemoteDesktopSession["send"]>(),
    setActive: vi.fn<RemoteDesktopSession["setActive"]>(),
    requestClipboard: vi.fn<NonNullable<RemoteDesktopSession["requestClipboard"]>>(),
  };
  const stop = vi.fn();
  return { session, emit: (event: RemoteDesktopEvent) => receive(event), stop };
}

function full(sequence: number, width = 2, height = 1, generation = 1) {
  return {
    kind: "full" as const, sequence, sessionId: "desktop", generation,
    size: { width, height }, rect: { x: 0, y: 0, width, height },
    rgba: new Uint8Array(width * height * 4).fill(255),
  };
}

/** jsdom의 공개 dispatchEvent는 isTrusted를 지우므로 구현을 직접 호출한다. 기본 동작을 막았는지 돌려준다. */
function dispatchTrusted(target: EventTarget, event: Event) {
  const impl = Object.getOwnPropertySymbols(event).find(symbol => symbol.description === "impl")!;
  const state = Reflect.get(event, impl);
  state.isTrusted = true;
  Reflect.get(target, impl)._dispatch(state);
  return event.defaultPrevented;
}

let host: HTMLDivElement;
let root: Root;
const putImageData = vi.fn();

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("ImageData", class {
    constructor(readonly data: Uint8ClampedArray, readonly width: number, readonly height: number) { }
  });
  putImageData.mockReset();
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({ putImageData } as unknown as CanvasRenderingContext2D);
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("유효한 전체 프레임 이후에 연결 상태와 서버 해상도를 표시한다", () => {
  const { session, emit } = fakeSession();
  act(() => root.render(<RemoteDesktopView session={session} active label="원격 데스크톱" />));
  const canvas = host.querySelector("canvas")!;
  const send = (event: RemoteDesktopEvent) => act(() => emit(event));
  const frame = full(1);
  send({ ...frame, kind: "patch" });
  send({ ...frame, rgba: new Uint8Array(7) });
  send({ ...frame, rect: { ...frame.rect, x: -1 } });
  send({ ...frame, rect: { ...frame.rect, width: 1 }, rgba: new Uint8Array(4) });
  send({ ...frame, size: { width: 8193, height: 1 } });
  send({ ...frame, size: { width: 8192, height: 8192 } });
  expect(host.querySelector('[role="status"]')?.textContent).toBe("첫 화면 대기 중");
  expect(putImageData).not.toHaveBeenCalled();
  send(frame);
  canvas.style.width = "100px";
  expect(host.querySelector('[role="status"]')?.textContent).toBe("연결됨 · 2 × 1");
  expect([canvas.width, canvas.height]).toEqual([2, 1]);
  const patch = { ...frame, kind: "patch" as const, rect: { x: 1, y: 0, width: 1, height: 1 }, rgba: Uint8Array.of(7, 0, 0, 255) };
  send({ ...patch, sequence: 3 });
  send({ ...patch, sequence: 2, rect: { ...patch.rect, x: 2 } });
  send({ ...patch, sequence: 2, size: { width: 3, height: 1 } });
  send({ ...patch, sequence: 2, sessionId: "other" });
  send({ ...patch, sequence: 2, generation: 0 });
  expect(putImageData).toHaveBeenCalledTimes(1);
  send({ ...patch, sequence: 2 });
  send({ ...patch, sequence: 2 });
  expect(putImageData).toHaveBeenCalledTimes(2);
  expect(putImageData).toHaveBeenLastCalledWith(expect.objectContaining({ data: Uint8ClampedArray.of(7, 0, 0, 255) }), 1, 0);
  send(full(4, 4, 3));
  expect(host.querySelector('[role="status"]')?.textContent).toBe("연결됨 · 4 × 3");
  expect([canvas.width, canvas.height]).toEqual([4, 3]);
});

it("세대 교체와 단절 후에는 늦게 도착한 프레임을 버린다", () => {
  const old = fakeSession();
  const next = fakeSession(2);
  act(() => root.render(<RemoteDesktopView session={old.session} active label="원격 데스크톱" />));
  act(() => old.emit(full(1)));
  act(() => root.render(<RemoteDesktopView session={next.session} active label="원격 데스크톱" />));
  act(() => old.emit(full(2)));
  act(() => next.emit(full(1)));
  expect(old.stop).toHaveBeenCalledOnce();
  expect(host.querySelector("canvas")?.width).toBe(0);
  expect(host.querySelector('[role="status"]')?.textContent).toBe("첫 화면 대기 중");
  act(() => next.emit(full(1, 3, 1, 2)));
  act(() => next.emit({ ...next.session, kind: "disconnected", sequence: 2, message: "전송 종료" }));
  act(() => next.emit(full(3, 2, 1, 2)));
  expect(putImageData).toHaveBeenCalledTimes(2);
  expect(host.querySelector("canvas")?.width).toBe(0);
  expect(host.querySelector('[role="status"]')?.textContent).toBe("연결 끊김: 전송 종료");
});

it("그리기 실패를 연결 완료 대신 오류로 표시한다", async () => {
  invoke.mockReset().mockResolvedValue(undefined);
  const owner = { tab: "viewer", document: 1 };
  const identity = { sessionId: "viewer", generation: 1 };
  let session!: Connection;
  connect(owner, { host: "localhost", port: 5900, password: false }, (value) => { session = value; }, vi.fn(), vi.fn(),
    { readText: vi.fn(), writeText: vi.fn() });
  const { output } = invoke.mock.calls[0][1] as { output: { onmessage(message: unknown): void } };
  output.onmessage({ identity, kind: "started", serial: 0 });
  await Promise.resolve();
  act(() => root.render(<RemoteDesktopView session={session} active label="원격 데스크톱" />));
  putImageData.mockImplementationOnce(() => { throw new Error("draw error"); });
  const frame = new ArrayBuffer(33);
  const header = new DataView(frame);
  header.setUint8(0, 1);
  header.setBigUint64(1, 1n, true);
  header.setBigUint64(9, 1n, true);
  for (const offset of [17, 19, 25, 27]) header.setUint16(offset, 1, true);
  await act(async () => {
    output.onmessage({ identity, kind: "waiting", value: 1, serial: 1 });
    output.onmessage(frame);
    output.onmessage(frame);
  });
  session.resize(2, 2);
  expect(host.querySelector('[role="status"]')?.textContent).toBe("연결 실패: 원격 화면을 그릴 수 없습니다");
  expect(host.querySelector("canvas")?.width).toBe(0);
  expect(host.querySelector("button")?.disabled).toBe(true);
  expect(putImageData).toHaveBeenCalledOnce();
  expect(invoke.mock.calls.slice(1)).toEqual([
    ["vnc_ack", { owner, generation: 1, sequence: 0, offset: 0 }],
    ["vnc_ack", { owner, generation: 1, sequence: 0, offset: 1 }],
    ["vnc_close", { owner, generation: 1 }],
  ]);
});

it("첫 화면 전과 세션 폐기 후 입력 및 클립보드를 차단하고 현재 입력에는 순번을 붙인다", () => {
  const { session, emit } = fakeSession();
  const view = createRemoteDesktopViewSession(session, document.createElement("canvas"), () => { });
  const key = { kind: "key", down: true, code: "KeyA", key: "a", location: 0, repeat: false } as const;
  view.send(key);
  view.clipboard("copy");
  emit(full(1));
  view.send(key);
  view.send({ ...key, down: false });
  view.clipboard("paste");
  view.dispose();
  emit(full(2));
  view.send(key);
  view.clipboard("copy");
  expect(session.send.mock.calls).toEqual([
    [{ ...key, generation: 1, sessionId: "desktop", sequence: 1 }],
    [{ ...key, down: false, generation: 1, sessionId: "desktop", sequence: 2 }],
  ]);
  expect(session.requestClipboard.mock.calls).toEqual([[{ sessionId: "desktop", generation: 1, action: "paste" }]]);
});

it("비활성 탭은 포커스와 클립보드 버튼을 잠그고 활성 탭에서도 합성 입력을 버린다", () => {
  const { session, emit } = fakeSession();
  const render = (active: boolean) => act(() => root.render(<RemoteDesktopView session={session} active={active} label="원격 데스크톱" />));
  render(true);
  act(() => emit(full(1)));
  const keyboard = host.querySelector("textarea")!;
  const copy = host.querySelector("button")!;
  keyboard.focus();
  keyboard.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, code: "KeyA", key: "a" }));
  keyboard.dispatchEvent(new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data: "a" }));
  keyboard.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: "한" }));
  copy.click();
  expect(session.send).not.toHaveBeenCalled();
  expect(session.requestClipboard).not.toHaveBeenCalled();
  render(false);
  expect(host.querySelector("section")?.hidden).toBe(true);
  expect(document.activeElement).not.toBe(keyboard);
  expect(keyboard.tabIndex).toBe(-1);
  expect(copy.disabled).toBe(true);
  expect(session.setActive).toHaveBeenLastCalledWith(false);
  // 비활성 탭에서는 입력칸에 포커스가 있어도 신뢰 이벤트를 보내지 않는다.
  keyboard.focus();
  dispatchTrusted(keyboard, new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data: "a" }));
  act(() => { dispatchTrusted(keyboard, new CompositionEvent("compositionstart", { bubbles: true, data: "" })); });
  act(() => { dispatchTrusted(keyboard, new CompositionEvent("compositionend", { bubbles: true, data: "한" })); });
  expect(session.send).not.toHaveBeenCalled();
});

it("Shift keydown 없이 온 shiftKey=true를 실제 키보다 먼저 보낸다", () => {
  const { session, emit } = fakeSession();
  act(() => root.render(<RemoteDesktopView session={session} active label="원격 데스크톱" />));
  act(() => emit(full(1)));
  const keyboard = host.querySelector("textarea")!;
  keyboard.focus();
  dispatchTrusted(keyboard, new KeyboardEvent("keydown", { bubbles: true, code: "KeyX", key: "X", shiftKey: true }));
  expect(session.send.mock.calls.map(([input]) => input.kind === "key" ? [input.code, input.down] : [input.kind, false])).toEqual([
    ["ShiftLeft", true],
    ["KeyX", true],
  ]);
});

function connectedKeyboard() {
  const { session, emit } = fakeSession();
  act(() => root.render(<RemoteDesktopView session={session} active label="원격 데스크톱" />));
  act(() => emit(full(1)));
  const keyboard = host.querySelector("textarea")!;
  act(() => keyboard.focus());
  const sent = () => session.send.mock.calls.map(([input]) =>
    input.kind === "text" ? `text ${input.text}` : input.kind === "key" ? `${input.code} ${input.down ? "down" : "up"}` : input.kind);
  return { keyboard, sent };
}

it("IME 키는 스캔코드로 보내지 않고 조합이 끝난 글자만 한 번 보낸다", () => {
  const { keyboard, sent } = connectedKeyboard();
  expect(dispatchTrusted(keyboard, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "KeyG", key: "Process", keyCode: 229 }))).toBe(false);
  act(() => { dispatchTrusted(keyboard, new CompositionEvent("compositionstart", { bubbles: true, data: "" })); });
  expect(host.querySelector("textarea")?.dataset.composing).toBe("true");
  dispatchTrusted(keyboard, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "KeyK", key: "ㅏ", isComposing: true }));
  dispatchTrusted(keyboard, new InputEvent("beforeinput", { bubbles: true, inputType: "insertCompositionText", data: "하", isComposing: true }));
  dispatchTrusted(keyboard, new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data: "한" }));
  // 조합 중 keyup은 누른 적 없는 키라 보내지 않는다.
  dispatchTrusted(keyboard, new KeyboardEvent("keyup", { bubbles: true, cancelable: true, code: "KeyS", key: "s", isComposing: true }));
  act(() => { dispatchTrusted(keyboard, new CompositionEvent("compositionend", { bubbles: true, data: "한" })); });
  expect(host.querySelector("textarea")?.dataset.composing).toBeUndefined();
  expect(sent()).toEqual(["text 한"]);
});

it("Chromium처럼 compositionend만 신뢰되지 않으면 직전 신뢰 compositionupdate의 글자를 보낸다", () => {
  const { keyboard, sent } = connectedKeyboard();
  const composition = (type: string, data: string) => new CompositionEvent(type, { bubbles: true, data });
  // 스크립트가 만든 조합 시작·끝은 무시한다.
  keyboard.dispatchEvent(composition("compositionstart", ""));
  keyboard.dispatchEvent(composition("compositionend", "가짜"));
  act(() => { dispatchTrusted(keyboard, composition("compositionstart", "")); });
  dispatchTrusted(keyboard, composition("compositionupdate", "ㄱ"));
  keyboard.dispatchEvent(composition("compositionupdate", "가짜"));
  dispatchTrusted(keyboard, composition("compositionupdate", "글"));
  act(() => { keyboard.dispatchEvent(composition("compositionend", "글")); });
  expect(sent()).toEqual(["text 글"]);
});

it("스캔코드로 보낸 키는 기본 동작을 막아 입력칸 글자로 다시 보내지 않는다", () => {
  const { keyboard, sent } = connectedKeyboard();
  expect(dispatchTrusted(keyboard, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "KeyA", key: "a", keyCode: 65 }))).toBe(true);
  dispatchTrusted(keyboard, new KeyboardEvent("keyup", { bubbles: true, cancelable: true, code: "KeyA", key: "a", keyCode: 65 }));
  dispatchTrusted(keyboard, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "KeyC", key: "c", ctrlKey: true }));
  expect(sent()).toEqual(["KeyA down", "KeyA up", "ControlLeft down", "KeyC down"]);
});

it("조합 밖 insertText는 코드 포인트 단위로 보내고 서러게이트 쌍을 나누지 않는다", () => {
  const { keyboard, sent } = connectedKeyboard();
  const insert = (data: string) => dispatchTrusted(keyboard, new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data }));
  expect(insert("가😀\u0007 1")).toBe(true);
  expect(sent()).toEqual(["text 가", "text 😀", "text  ", "text 1"]);
});

it("IME가 조합을 끝내며 넘긴 Enter는 Enter 키 한 번으로 보낸다", () => {
  const { keyboard, sent } = connectedKeyboard();
  dispatchTrusted(keyboard, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "Enter", key: "Process", keyCode: 229 }));
  dispatchTrusted(keyboard, new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertLineBreak" }));
  expect(sent()).toEqual(["Enter down", "Enter up"]);
});

function pointerCanvas() {
  const canvas = host.querySelector("canvas")!;
  vi.spyOn(canvas, "getBoundingClientRect").mockReturnValue({ left: 0, top: 0, width: 2, height: 1 } as DOMRect);
  Object.assign(canvas, { setPointerCapture: vi.fn(), hasPointerCapture: () => false, releasePointerCapture: vi.fn() });
  const press = (type: string, buttons: number) => {
    const event = new MouseEvent(type, { bubbles: true, cancelable: true, clientX: 1, clientY: 0, buttons });
    Object.assign(event, { pointerId: 1, isPrimary: true });
    act(() => { dispatchTrusted(canvas, event); });
  };
  return { canvas, press };
}

/** 조합을 시작하고, Chromium처럼 포커스를 잃을 때 신뢰되지 않은 compositionend로 확정하게 한다. */
function composeUntilBlur(keyboard: HTMLTextAreaElement, text: string) {
  act(() => { dispatchTrusted(keyboard, new CompositionEvent("compositionstart", { bubbles: true, data: "" })); });
  dispatchTrusted(keyboard, new CompositionEvent("compositionupdate", { bubbles: true, data: text }));
  keyboard.addEventListener("blur", () => keyboard.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: text })), { once: true });
}

it("조합 중 클릭은 조합을 먼저 확정해 글자를 클릭보다 먼저 보내고 다음 작업에서 입력칸 포커스를 돌린다", async () => {
  const { keyboard, sent } = connectedKeyboard();
  const { canvas, press } = pointerCanvas();
  composeUntilBlur(keyboard, "하");
  press("pointerdown", 1);
  // 포커스를 돌리기 전에 오는 이동과 뗌도 같은 클릭으로 보낸다.
  press("pointerup", 0);
  expect(sent()).toEqual(["text 하", "pointer", "pointer"]);
  expect(document.activeElement).not.toBe(keyboard);
  await act(async () => { await new Promise((resolve) => setTimeout(resolve)); });
  expect(document.activeElement).toBe(keyboard);
  expect(sent()).toEqual(["text 하", "pointer", "pointer"]);
  expect(host.querySelector("textarea")?.dataset.composing).toBeUndefined();
  // 일반 클릭은 입력칸에 포커스를 두고 mousedown 기본 동작을 막는다.
  expect(dispatchTrusted(canvas, new MouseEvent("mousedown", { bubbles: true, cancelable: true }))).toBe(true);
});

it("조합 중 클릭 뒤 포커스를 돌려주기 전 틈에 온 키 이벤트도 원격에 보내 눌린 키를 남기지 않는다", async () => {
  const { keyboard, sent } = connectedKeyboard();
  const { press } = pointerCanvas();
  dispatchTrusted(keyboard, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "KeyA", key: "a" }));
  composeUntilBlur(keyboard, "하");
  press("pointerdown", 1);
  expect(document.activeElement).not.toBe(keyboard);
  // 틈에서 뗀 키와 누른 키는 body가 받아도 입력칸 처리로 넘어간다.
  dispatchTrusted(document.body, new KeyboardEvent("keyup", { bubbles: true, cancelable: true, code: "KeyA", key: "a" }));
  dispatchTrusted(document.body, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "KeyB", key: "b" }));
  press("pointerup", 0);
  expect(sent()).toEqual(["KeyA down", "text 하", "pointer", "KeyA up", "KeyB down", "pointer"]);
  await act(async () => { await new Promise((resolve) => setTimeout(resolve)); });
  expect(document.activeElement).toBe(keyboard);
  dispatchTrusted(keyboard, new KeyboardEvent("keyup", { bubbles: true, cancelable: true, code: "KeyB", key: "b" }));
  expect(sent().slice(-1)).toEqual(["KeyB up"]);
  // 틈이 지난 뒤 body가 받는 키 이벤트는 보내지 않는다.
  act(() => keyboard.blur());
  const before = sent().length;
  dispatchTrusted(document.body, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "KeyC", key: "c" }));
  expect(sent()).toHaveLength(before);
});

it("창이 포커스를 잃으면 조합 중인 글자를 한 번 보내고 눌린 상태를 해제한다", () => {
  const { keyboard, sent } = connectedKeyboard();
  dispatchTrusted(keyboard, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "ShiftLeft", key: "Shift", shiftKey: true }));
  composeUntilBlur(keyboard, "하");
  act(() => { window.dispatchEvent(new Event("blur")); });
  expect(sent()).toEqual(["ShiftLeft down", "text 하", "ShiftLeft up", "release-input"]);
});

it("신뢰되지 않은 compositionend는 직전 신뢰 compositionupdate와 같은 글자일 때만 보낸다", () => {
  const { keyboard, sent } = connectedKeyboard();
  const composition = (type: string, data: string) => new CompositionEvent(type, { bubbles: true, data });
  act(() => { dispatchTrusted(keyboard, composition("compositionstart", "")); });
  dispatchTrusted(keyboard, composition("compositionupdate", "가"));
  // 취소한 조합은 빈 글자로 끝난다.
  act(() => { keyboard.dispatchEvent(composition("compositionend", "")); });
  act(() => { dispatchTrusted(keyboard, composition("compositionstart", "")); });
  dispatchTrusted(keyboard, composition("compositionupdate", "가"));
  // 스크립트가 다른 글자로 앞당겨 끝낸 조합도 보내지 않는다.
  act(() => { keyboard.dispatchEvent(composition("compositionend", "각")); });
  expect(sent()).toEqual([]);
});

it("첫 화면 전에도 원격 화면을 누르면 입력칸에 포커스를 두고 키는 보내지 않는다", () => {
  const { session } = fakeSession();
  act(() => root.render(<RemoteDesktopView session={session} active label="원격 데스크톱" />));
  const keyboard = host.querySelector("textarea")!;
  const canvas = host.querySelector("canvas")!;
  const down = new MouseEvent("pointerdown", { bubbles: true, cancelable: true, buttons: 1 });
  Object.assign(down, { pointerId: 1, isPrimary: true });
  act(() => { dispatchTrusted(canvas, down); });
  expect(document.activeElement).toBe(keyboard);
  expect(canvas.dataset.keyboard).toBeUndefined();
  dispatchTrusted(keyboard, new KeyboardEvent("keydown", { bubbles: true, cancelable: true, code: "KeyA", key: "a" }));
  expect(session.send).not.toHaveBeenCalled();
});

it("연결된 화면의 입력칸에 포커스가 있을 때만 테두리 상태를 둔다", () => {
  const { keyboard } = connectedKeyboard();
  const canvas = host.querySelector("canvas")!;
  expect(canvas.dataset.keyboard).toBe("focused");
  act(() => keyboard.blur());
  expect(canvas.dataset.keyboard).toBeUndefined();
});

it("입력칸에 쌓인 확정 글자가 길어지면 조합이 끝날 때 바로 비운다", () => {
  const { keyboard } = connectedKeyboard();
  expect(keyboard.getAttribute("wrap")).toBe("off");
  keyboard.value = "가".repeat(300);
  act(() => { dispatchTrusted(keyboard, new CompositionEvent("compositionstart", { bubbles: true, data: "" })); });
  act(() => { dispatchTrusted(keyboard, new CompositionEvent("compositionend", { bubbles: true, data: "가" })); });
  expect(keyboard.value).toBe("");
});
