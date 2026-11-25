// @vitest-environment jsdom

import { act, createElement, createRef } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { createRemoteDesktopViewSession, type RemoteDesktopViewSession } from "../remoteDesktop/session";
import { connect, DROPPED_NOTICE, type Connection } from "./session";
import { VncView, type VncTabActions } from "./VncView";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn<(name: string, args: Record<string, unknown>) => Promise<unknown>>() }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke, Channel: class {
    constructor(readonly onmessage: (message: unknown) => void) { }
  }
}));
vi.mock("./document", () => ({
  documentProof: () => "a".repeat(32),
  invokeVnc: (name: string, args: Record<string, unknown>) => invoke(name, args),
  readPacket: async (_proof: string, packet: unknown) => packet,
}));

function full(sequence: number, epoch: number) {
  const buffer = new ArrayBuffer(33);
  const view = new DataView(buffer);
  view.setUint8(0, 1);
  view.setBigUint64(1, BigInt(sequence), true);
  view.setBigUint64(9, BigInt(epoch), true);
  for (const offset of [17, 19, 25, 27]) view.setUint16(offset, 1, true);
  new Uint8Array(buffer, 29).set([20, 40, 60, 255]);
  return buffer;
}

it("첫 전체 프레임 전에는 입력을 막고 폐기한 세대의 붙여넣기와 프레임을 버린다", async () => {
  invoke.mockResolvedValue(undefined);
  const identity = { sessionId: "1", generation: 1 };
  const { promise, resolve } = Promise.withResolvers<string>();
  const clipboard = { readText: () => promise, writeText: vi.fn(async () => { }) };
  let session: Connection | undefined;
  const dispose = connect({ tab: "tab", document: 1 }, { host: "localhost", port: 5900, password: false },
    (value) => { session = value; }, vi.fn(), vi.fn(), clipboard);
  const { output } = invoke.mock.calls[0][1] as { output: { onmessage(message: unknown): void } };
  output.onmessage({ identity, kind: "started", serial: 0 });
  await Promise.resolve();
  expect(invoke).toHaveBeenLastCalledWith("vnc_ack", expect.objectContaining({ sequence: 0, offset: 0 }));
  if (!session) throw new Error("연결 없음");
  const { send, requestClipboard, subscribe } = session;
  const receive = vi.fn<Parameters<Connection["subscribe"]>[0]>(() => true);
  session.setActive(true);
  output.onmessage({ identity, kind: "waiting", value: 1 });
  await Promise.resolve();
  send({ ...identity, kind: "pointer", buttons: 2, x: 0, y: 0, sequence: 1 });
  expect(invoke.mock.calls.map(([name]) => name)).toEqual(["vnc_connect", "vnc_ack"]);
  output.onmessage(full(1, 1));
  await Promise.resolve();
  expect(invoke.mock.calls.map(([name]) => name)).toEqual(["vnc_connect", "vnc_ack"]);
  subscribe(receive);
  expect(receive).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "full", rgba: new Uint8Array([20, 40, 60, 255]) }));
  expect(invoke).toHaveBeenLastCalledWith("vnc_ack", expect.objectContaining({ sequence: 1, offset: 0 }));
  send({ ...identity, kind: "pointer", buttons: 2, x: 0, y: 0, sequence: 2 });
  expect(invoke).toHaveBeenLastCalledWith("vnc_input", expect.objectContaining({
    input: {
      ...identity, epoch: 1, sequence: 1, action: { kind: "pointer", buttons: 4, x: 0, y: 0 },
    }
  }));
  output.onmessage({ identity, kind: "clipboard", value: "serveur" });
  await Promise.resolve();
  requestClipboard?.({ ...identity, action: "copy" });
  expect(clipboard.writeText).toHaveBeenCalledWith("serveur");
  requestClipboard?.({ ...identity, action: "paste" });
  dispose();
  output.onmessage(full(2, 1));
  resolve("ancien");
  await promise;
  expect(receive.mock.calls.map(([event]) => event.kind)).toEqual(["waiting", "full"]);
  expect(invoke.mock.calls.filter(([name]) => name === "vnc_input")).toHaveLength(1);
});

it("비활성화는 보내지 않은 입력과 늦은 붙여넣기를 버리고 전달한 누름을 해제한다", async () => {
  invoke.mockClear();
  const delivery = Promise.withResolvers<void>();
  const paste = Promise.withResolvers<string>();
  invoke.mockImplementation((name) => name === "vnc_input" ? delivery.promise : Promise.resolve());
  const identity = { sessionId: "2", generation: 1 };
  let session!: Connection;
  connect({ tab: "tab", document: 1 }, { host: "localhost", port: 5900, password: false },
    (value) => { session = value; }, vi.fn(), vi.fn(), { readText: () => paste.promise, writeText: vi.fn() });
  const { output } = invoke.mock.calls[0][1] as { output: { onmessage(message: unknown): void } };
  output.onmessage({ identity, kind: "started", serial: 0 });
  await Promise.resolve();
  session.subscribe(() => true);
  session.setActive(true);
  output.onmessage({ identity, kind: "waiting", value: 1, serial: 1 });
  output.onmessage(full(1, 1));
  await Promise.resolve();
  const key = { ...identity, kind: "key", down: true, code: "KeyA", key: "a", location: 0, repeat: false } as const;
  session.send({ ...key, sequence: 1 });
  session.send({ ...key, code: "KeyB", key: "b", sequence: 2 });
  session.requestClipboard?.({ ...identity, action: "paste" });
  session.setActive(false);
  session.setActive(true);
  paste.resolve("폐기할 텍스트");
  delivery.resolve();
  await Promise.all([paste.promise, delivery.promise]);
  expect(invoke.mock.calls.filter(([name]) => name === "vnc_input").map(([, args]) => args.input)).toEqual([
    { ...identity, epoch: 1, sequence: 1, action: { kind: "key", keysym: 97, down: true } },
    { ...identity, epoch: 1, sequence: 2, action: { kind: "release" } },
  ]);
});

it("Canvas에 그린 뒤에만 화면 패킷을 ACK한다", async () => {
  invoke.mockReset().mockResolvedValue(undefined);
  const owner = { tab: "viewer", document: 1 };
  const identity = { sessionId: "viewer", generation: 1 };
  const canvas = document.createElement("canvas");
  const draw = vi.fn(() => {
    expect(invoke).not.toHaveBeenCalledWith("vnc_ack", { owner, generation: 1, sequence: 1, offset: 0 });
  });
  vi.spyOn(canvas, "getContext").mockReturnValue({ putImageData: draw } as unknown as CanvasRenderingContext2D);
  vi.stubGlobal("ImageData", class {
    constructor(readonly data: Uint8ClampedArray, readonly width: number, readonly height: number) { }
  });
  let view: RemoteDesktopViewSession | undefined;
  const dispose = connect(owner, { host: "localhost", port: 5900, password: false }, (session) => {
    view = createRemoteDesktopViewSession(session, canvas, vi.fn());
  }, vi.fn(), vi.fn(), { readText: vi.fn(), writeText: vi.fn() });
  try {
    const { output } = invoke.mock.calls[0][1] as { output: { onmessage(message: unknown): void } };
    output.onmessage({ identity, kind: "started", serial: 0 });
    await Promise.resolve();
    output.onmessage({ identity, kind: "waiting", value: 1, serial: 1 });
    await Promise.resolve();
    output.onmessage(full(1, 1));
    await Promise.resolve();
    expect(draw).toHaveBeenCalledOnce();
    expect(draw).toHaveBeenCalledWith(expect.objectContaining({ data: Uint8ClampedArray.of(20, 40, 60, 255) }), 0, 0);
    expect(invoke).toHaveBeenLastCalledWith("vnc_ack", { owner, generation: 1, sequence: 1, offset: 0 });
  } finally {
    dispose();
    view?.dispose();
    vi.unstubAllGlobals();
  }
});

function frame(kind: number, sequence: number, size: [number, number], rect: [number, number, number, number]) {
  const buffer = new ArrayBuffer(29 + rect[2] * rect[3] * 4);
  const view = new DataView(buffer);
  view.setUint8(0, kind);
  view.setBigUint64(1, BigInt(sequence), true);
  view.setBigUint64(9, 1n, true);
  [...size, ...rect].forEach((value, index) => view.setUint16(17 + index * 2, value, true));
  new Uint8Array(buffer, 29).fill(kind * 100);
  return buffer;
}

it("부분 화면 패킷은 사각형 위치에 그린 뒤 ACK한다", async () => {
  invoke.mockReset().mockResolvedValue(undefined);
  const owner = { tab: "viewer", document: 1 };
  const identity = { sessionId: "viewer", generation: 1 };
  const canvas = document.createElement("canvas");
  const draw = vi.fn();
  vi.spyOn(canvas, "getContext").mockReturnValue({ putImageData: draw } as unknown as CanvasRenderingContext2D);
  vi.stubGlobal("ImageData", class {
    constructor(readonly data: Uint8ClampedArray, readonly width: number, readonly height: number) { }
  });
  let view: RemoteDesktopViewSession | undefined;
  const dispose = connect(owner, { host: "localhost", port: 5900, password: false }, (session) => {
    view = createRemoteDesktopViewSession(session, canvas, vi.fn());
  }, vi.fn(), vi.fn(), { readText: vi.fn(), writeText: vi.fn() });
  try {
    const { output } = invoke.mock.calls[0][1] as { output: { onmessage(message: unknown): void } };
    output.onmessage({ identity, kind: "started", serial: 0 });
    await Promise.resolve();
    output.onmessage({ identity, kind: "waiting", value: 1, serial: 1 });
    await Promise.resolve();
    output.onmessage(frame(1, 1, [3, 2], [0, 0, 3, 2]));
    await Promise.resolve();
    expect(invoke).toHaveBeenLastCalledWith("vnc_ack", { owner, generation: 1, sequence: 1, offset: 0 });
    output.onmessage(frame(2, 2, [3, 2], [1, 1, 2, 1]));
    await Promise.resolve();
    expect(draw).toHaveBeenLastCalledWith(expect.objectContaining({ data: new Uint8ClampedArray(8).fill(200), width: 2, height: 1 }), 1, 1);
    expect(invoke).toHaveBeenLastCalledWith("vnc_ack", { owner, generation: 1, sequence: 2, offset: 0 });
  } finally {
    dispose();
    view?.dispose();
    vi.unstubAllGlobals();
  }
});

it.each([
  ["길이가 사각형과 다른 화면 패킷", full(1, 1).slice(0, 32)],
  ["머리만 있는 화면 패킷", full(1, 1).slice(0, 29)],
])("%s는 그리거나 ACK하지 않고 세션을 닫는다", async (_, packet) => {
  invoke.mockReset().mockResolvedValue(undefined);
  const owner = { tab: "viewer", document: 1 };
  const identity = { sessionId: "viewer", generation: 1 };
  const receive = vi.fn<Parameters<Connection["subscribe"]>[0]>(() => true);
  connect(owner, { host: "localhost", port: 5900, password: false }, (session) => { session.subscribe(receive); },
    vi.fn(), vi.fn(), { readText: vi.fn(), writeText: vi.fn() });
  const { output } = invoke.mock.calls[0][1] as { output: { onmessage(message: unknown): void } };
  output.onmessage({ identity, kind: "started", serial: 0 });
  await Promise.resolve();
  output.onmessage({ identity, kind: "waiting", value: 1, serial: 1 });
  await Promise.resolve();
  output.onmessage(packet);
  await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("vnc_close", { owner, generation: 1 }));
  expect(receive.mock.calls.map(([event]) => event.kind)).toEqual(["waiting", "disconnected"]);
  expect(invoke).not.toHaveBeenCalledWith("vnc_ack", expect.objectContaining({ sequence: 1 }));
});

it("터널 요청은 원격 목적지만 전달하고 실패 후 직접 TLS를 시도하지 않는다", async () => {
  invoke.mockReset().mockRejectedValue("터널 종료");
  const owner = { tab: "viewer", document: 1 };
  const source = { tabHandle: "a".repeat(32), tunnelId: "7", targetId: "server", destination: { host: "::1", port: 5900 } };
  const failed = vi.fn();
  connect(owner, { source, password: true }, vi.fn(), vi.fn(), failed);
  await Promise.resolve();
  expect(invoke).toHaveBeenCalledTimes(1);
  expect(invoke).toHaveBeenCalledWith("vnc_connect", { owner, draft: { source, password: true }, output: expect.anything() });
  expect(failed).toHaveBeenCalledWith("터널 종료");
});

it("탭 종료가 미확인이면 같은 소유자로만 재확인한다", async () => {
  const owner = { tab: "viewer", document: 1 };
  const close = Promise.withResolvers<boolean>();
  invoke.mockReset().mockImplementation((name) => {
    switch (name) {
      case "vnc_create": return Promise.resolve(owner);
      case "vnc_connect": return Promise.resolve();
      case "vnc_close": return close.promise;
      default: throw new Error(name);
    }
  });
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const host = document.createElement("div");
  const root = createRoot(host);
  const ref = createRef<VncTabActions>();
  const source = { tabHandle: "a".repeat(32), tunnelId: "7", targetId: "server", destination: { host: "::1", port: 5900 } };
  try {
    await act(async () => root.render(createElement(VncView, { active: true, source, ref })));
    const form = host.querySelector("form");
    const actions = ref.current;
    if (!form || !actions) throw new Error("VNC 탭 없음");
    await act(async () => form.requestSubmit());

    expect(host.querySelector("[role=alert], dialog")).toBeNull();
    await act(async () => {
      const pending = actions.requestClose();
      expect(actions.requestClose()).toBe(pending);
      close.resolve(false);
      await expect(pending).resolves.toBe(false);
    });
    expect(host.querySelector('[role="alert"]')?.textContent).toBe("종료 확인 불가");
    expect(form.querySelector("button")?.disabled).toBe(true);
    await act(async () => form.requestSubmit());
    expect(invoke.mock.calls.map(([name]) => name)).toEqual(["vnc_create", "vnc_connect", "vnc_close"]);

    invoke.mockResolvedValueOnce(true);
    await act(async () => { await expect(actions.requestClose()).resolves.toBe(true); });
    expect(host.querySelector('[role="alert"]')).toBeNull();
    expect(invoke.mock.calls.filter(([name]) => name === "vnc_close")).toEqual([
      ["vnc_close", { owner, generation: null }],
      ["vnc_close", { owner, generation: null, retry: true }],
    ]);
  } finally {
    await act(async () => root.unmount());
    vi.unstubAllGlobals();
  }
});

it("연결 준비 중 발견한 정리 미확인을 연결 오류와 함께 전달한다", async () => {
  invoke.mockReset().mockRejectedValue("종료 확인 불가");
  const failed = vi.fn();
  const unconfirmed = vi.fn();
  connect({ tab: "viewer", document: 1 }, { host: "localhost", port: 5900, password: false },
    vi.fn(), vi.fn(), failed, undefined, { closeOpening: vi.fn(), unconfirmed });
  await Promise.resolve();
  expect(failed).toHaveBeenCalledWith("종료 확인 불가");
  expect(unconfirmed).toHaveBeenCalledOnce();
  expect(invoke.mock.calls.map(([name]) => name)).toEqual(["vnc_connect"]);
});

it("확정된 글자는 유니코드 keysym을 누르고 떼며 큐를 넘기면 끊지 않고 버린다", async () => {
  invoke.mockClear();
  const delivery = Promise.withResolvers<void>();
  invoke.mockImplementation((name) => name === "vnc_input" ? delivery.promise : Promise.resolve());
  const identity = { sessionId: "3", generation: 1 };
  const notice = vi.fn();
  const failed = vi.fn();
  let session!: Connection;
  connect({ tab: "tab", document: 1 }, { host: "localhost", port: 5900, password: false },
    (value) => { session = value; }, notice, failed, { readText: vi.fn(), writeText: vi.fn() });
  const { output } = invoke.mock.calls[0][1] as { output: { onmessage(message: unknown): void } };
  output.onmessage({ identity, kind: "started", serial: 0 });
  await Promise.resolve();
  session.subscribe(() => true);
  session.setActive(true);
  output.onmessage({ identity, kind: "waiting", value: 1, serial: 1 });
  output.onmessage(full(1, 1));
  await Promise.resolve();
  const text = (value: string, sequence: number) => session.send({ ...identity, kind: "text", text: value, sequence });
  text("한", 1);
  text("😀", 2);
  text("é", 3);
  for (let sequence = 4; sequence < 40; sequence++) text("a", sequence);
  expect(failed).not.toHaveBeenCalled();
  expect(notice.mock.calls).toEqual([[DROPPED_NOTICE]]);
  const actions = () => invoke.mock.calls.filter(([name]) => name === "vnc_input")
    .map(([, args]) => (args.input as { action: unknown }).action);
  expect(actions()).toEqual([{ kind: "key", keysym: 0x0100d55c, down: true }]);
  delivery.resolve();
  await vi.waitFor(() => expect(actions()).toHaveLength(32));
  expect(actions().slice(0, 6)).toEqual([
    { kind: "key", keysym: 0x0100d55c, down: true }, { kind: "key", keysym: 0x0100d55c, down: false },
    { kind: "key", keysym: 0x0101f600, down: true }, { kind: "key", keysym: 0x0101f600, down: false },
    { kind: "key", keysym: 0xe9, down: true }, { kind: "key", keysym: 0xe9, down: false },
  ]);
});
