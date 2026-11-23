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
  const frame = new ArrayBuffer(37);
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
  const canvas = host.querySelector("canvas")!;
  const copy = host.querySelector("button")!;
  canvas.focus();
  canvas.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, code: "KeyA", key: "a" }));
  copy.click();
  expect(session.send).not.toHaveBeenCalled();
  expect(session.requestClipboard).not.toHaveBeenCalled();
  render(false);
  expect(host.querySelector("section")?.hidden).toBe(true);
  expect(document.activeElement).not.toBe(canvas);
  expect(canvas.tabIndex).toBe(-1);
  expect(copy.disabled).toBe(true);
  expect(session.setActive).toHaveBeenLastCalledWith(false);
});

it("Shift keydown 없이 온 shiftKey=true를 실제 키보다 먼저 보낸다", () => {
  const { session, emit } = fakeSession();
  act(() => root.render(<RemoteDesktopView session={session} active label="원격 데스크톱" />));
  act(() => emit(full(1)));
  const canvas = host.querySelector("canvas")!;
  canvas.focus();
  const event = new KeyboardEvent("keydown", { bubbles: true, code: "KeyX", key: "X", shiftKey: true });
  // jsdom의 공개 dispatchEvent는 isTrusted를 지우므로 구현을 직접 호출한다.
  const impl = Object.getOwnPropertySymbols(event).find(symbol => symbol.description === "impl")!;
  const state = Reflect.get(event, impl);
  state.isTrusted = true;
  Reflect.get(canvas, impl)._dispatch(state);
  expect(session.send.mock.calls.map(([input]) => input.kind === "key" ? [input.code, input.down] : [input.kind, false])).toEqual([
    ["ShiftLeft", true],
    ["KeyX", true],
  ]);
});
