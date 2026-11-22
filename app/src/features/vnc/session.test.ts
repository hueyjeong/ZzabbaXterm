// @vitest-environment jsdom

import { expect, it, vi } from "vitest";
import { createRemoteDesktopViewSession, type RemoteDesktopViewSession } from "../remoteDesktop/session";
import { connect, type Connection } from "./session";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn<(name: string, args: Record<string, unknown>) => Promise<void>>() }));
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
  const buffer = new ArrayBuffer(37);
  const view = new DataView(buffer);
  view.setUint8(0, 1);
  view.setBigUint64(1, BigInt(sequence), true);
  view.setBigUint64(9, BigInt(epoch), true);
  for (const offset of [17, 19, 25, 27]) view.setUint16(offset, 1, true);
  new Uint8Array(buffer, 33).set([20, 40, 60, 255]);
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

it("Canvas에 그린 뒤에만 마지막 화면 조각을 ACK한다", async () => {
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
