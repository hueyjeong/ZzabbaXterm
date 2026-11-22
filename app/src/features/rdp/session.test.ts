// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { createRemoteDesktopViewSession } from "../remoteDesktop/session";
import { connect, type Connection } from "./session";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke,
  convertFileSrc: (path: string) => path,
  Channel: class { constructor(public onmessage: (ticket: string) => void) { } },
}));
const proof = "a".repeat(32);
const id = { sessionId: "b".repeat(32), generation: 3 };
const input = { kind: "key", down: true, code: "KeyA", key: "a", location: 0, repeat: false } as const;

afterEach(() => { vi.resetAllMocks(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

async function fixture() {
  vi.stubGlobal("__ZZABBA_RDP_DOCUMENT__", { proof });
  const opened = Promise.withResolvers<Connection>();
  const ended = vi.fn();
  const ready = vi.fn();
  const caps = vi.fn();
  const clipboard = { readText: vi.fn().mockResolvedValue("격리 문자열"), writeText: vi.fn().mockResolvedValue(undefined) };
  let notify: (ticket: string) => void = () => { throw new Error("output channel missing"); };
  const sendInput = vi.fn().mockResolvedValue(true);
  invoke.mockImplementation((name, args) => {
    if (name === "rdp_prepare") return Promise.resolve(id);
    if (name === "rdp_connect") {
      notify = args.output.onmessage;
      return Promise.withResolvers<void>().promise;
    }
    if (name === "rdp_input") return sendInput();
    return Promise.resolve();
  });
  const fetch = vi.fn();
  vi.stubGlobal("fetch", fetch);
  const close = connect({ host: "localhost", port: 3389 }, opened.resolve, caps, ready, vi.fn(), ended, clipboard);
  const connection = await opened.promise;
  const calls = (name: string) => invoke.mock.calls.filter(([command]) => command === name).map(([, args]) => args);
  function packet(value: Record<string, unknown> | ArrayBuffer) {
    const binary = value instanceof ArrayBuffer;
    fetch.mockResolvedValueOnce({
      ok: true, headers: { get: () => binary ? "application/octet-stream" : "application/json" },
      arrayBuffer: () => Promise.resolve(value), json: () => Promise.resolve(value)
    });
    notify("c".repeat(32));
  }
  const header = (sequence: number, frameSequence: number, width = 1, displayGeneration = 1) => packet({
    sequence, kind: "frame", full: true, frameSequence, displayGeneration, width, height: 1, x: 0, y: 0, w: width, h: 1
  });
  const display = (sequence: number, displayGeneration = 1) => packet({ kind: "display", sequence, displayGeneration });
  const pixels = (sequence: number, width = 1) => {
    const buffer = new ArrayBuffer(12 + 4 * width);
    const binary = new DataView(buffer);
    binary.setBigUint64(0, BigInt(sequence), true);
    new Uint8Array(buffer, 12).fill(255);
    packet(buffer);
  };
  const canvas = document.createElement("canvas");
  const draw = vi.fn();
  vi.spyOn(canvas, "getContext").mockReturnValue({ putImageData: draw } as unknown as CanvasRenderingContext2D);
  vi.stubGlobal("ImageData", class { constructor(readonly rgba: Uint8ClampedArray) { } });
  return { calls, packet, header, pixels, display, sendInput, close, connection, ended, ready, caps, clipboard, canvas, draw };
}

it("Canvas 적용 ACK 뒤에만 키를 전달하고 서버가 보낸 크기와 클립보드 결과를 사용한다", async () => {
  const f = await fixture();
  const view = createRemoteDesktopViewSession(f.connection, f.canvas, vi.fn());
  f.connection.setActive(true);
  view.send(input);
  expect(f.calls("rdp_input")).toEqual([]);
  f.header(1, 1);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toEqual([{ id, sequence: 1, offset: 0 }]));
  f.pixels(1);
  await vi.waitFor(() => expect(f.calls("rdp_ack")[1]).toEqual({ id, sequence: 1, offset: 1 }));
  expect(f.draw).toHaveBeenCalledOnce();
  expect(f.draw.mock.invocationCallOrder[0]).toBeLessThan(Math.max(...invoke.mock.invocationCallOrder));
  expect(f.ready).not.toHaveBeenCalled();
  f.display(2);
  await vi.waitFor(() => expect(f.ready).toHaveBeenCalledWith(true));
  view.send(input);
  await vi.waitFor(() => expect(f.calls("rdp_input")[1]).toMatchObject({ id, input: { sequence: 2, action: { kind: "key", code: 0x1e, down: true } } }));
  f.packet({ kind: "channels", sequence: 3, copy: true, paste: true, resize: true });
  await vi.waitFor(() => expect(f.caps).toHaveBeenCalledWith({ copy: true, paste: true, resize: true }));
  f.connection.resize(800, 600);
  expect(f.canvas.width).toBe(1);
  f.header(4, 2, 2, 2);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toContainEqual({ id, sequence: 4, offset: 0 }));
  f.pixels(4, 2);
  await vi.waitFor(() => expect(f.canvas.width).toBe(2));
  f.display(5, 2);
  await vi.waitFor(() => expect(f.calls("rdp_input")[3]).toMatchObject({ input: { displayGeneration: 2, action: { kind: "active" } } }));
  f.connection.clipboard("paste");
  f.connection.clipboard("copy");
  await vi.waitFor(() => expect(f.calls("rdp_input").map((call) => call.input.action.kind)).toEqual(["active", "key", "resize", "active", "copy", "paste"]));
  f.packet({ kind: "clipboard", sequence: 6, epoch: f.calls("rdp_input")[4].input.epoch, text: "원격 시험 문자열" });
  await vi.waitFor(() => expect(f.clipboard.writeText).toHaveBeenCalledWith("원격 시험 문자열"));
  f.connection.setActive(false);
  view.send(input);
  await vi.waitFor(() => expect(f.calls("rdp_input").slice(-1)[0]).toMatchObject({ input: { action: { kind: "active", active: false } } }));
  expect(f.calls("rdp_input").filter((call) => call.input.action.kind === "key")).toHaveLength(1);
  f.close();
  view.dispose();
});

it("이전 화면 입력은 폐기하고 같은 크기의 새 화면 ACK 후 입력을 재개한다", async () => {
  const f = await fixture();
  const view = createRemoteDesktopViewSession(f.connection, f.canvas, vi.fn());
  f.connection.setActive(true);
  f.header(1, 1);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toHaveLength(1));
  f.pixels(1);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toHaveLength(2));
  f.display(2);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toHaveLength(3));
  const discarded = Promise.withResolvers<boolean>();
  f.sendInput.mockReturnValueOnce(discarded.promise);
  view.send(input);
  view.send(input);
  discarded.resolve(false);
  await discarded.promise;
  view.send(input);
  f.header(3, 2, 1, 2);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toHaveLength(4));
  view.send(input);
  f.pixels(3);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toHaveLength(5));
  view.send(input);
  expect(f.calls("rdp_input")).toHaveLength(2);
  expect(f.ended).not.toHaveBeenCalled();
  f.display(4, 2);
  await vi.waitFor(() => expect(f.calls("rdp_input")[2]).toMatchObject({ input: { sequence: 3, displayGeneration: 2, action: { kind: "active" } } }));
  f.sendInput.mockRejectedValueOnce("RDP 전송 연결이 끊겼습니다");
  view.send(input);
  await vi.waitFor(() => expect(f.ended).toHaveBeenCalledWith("RDP 전송 연결이 끊겼습니다"));
  expect(f.calls("rdp_input")[3]).toMatchObject({ input: { sequence: 4, displayGeneration: 2, action: { kind: "key" } } });
  view.dispose();
});

it("Canvas 적용 실패 시 최종 ACK 없이 세션을 닫는다", async () => {
  const f = await fixture();
  const view = createRemoteDesktopViewSession(f.connection, f.canvas, vi.fn());
  f.draw.mockImplementationOnce(() => { throw new Error("draw error"); });
  f.header(1, 1);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toHaveLength(1));
  f.pixels(1);
  await vi.waitFor(() => expect(f.ended).toHaveBeenCalledOnce());
  expect(f.calls("rdp_ack")).toEqual([{ id, sequence: 1, offset: 0 }]);
  expect(f.calls("rdp_close")).toEqual([{ id }]);
  expect(f.canvas.width).toBe(0);
  view.dispose();
});

it("폐기한 탭의 픽셀은 Canvas에 그리거나 ACK하지 않는다", async () => {
  const f = await fixture();
  const view = createRemoteDesktopViewSession(f.connection, f.canvas, vi.fn());
  f.header(1, 1);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toHaveLength(1));
  f.pixels(1);
  f.close();
  view.dispose();
  await vi.waitFor(() => expect(f.calls("rdp_close")).toEqual([{ id }]));
  expect(f.draw).not.toHaveBeenCalled();
  expect(f.calls("rdp_ack")).toEqual([{ id, sequence: 1, offset: 0 }]);
});

it("터널 연결 거부 시 목적지와 TLS DNS를 구분해 전달하고 직접 연결로 재시도하지 않는다", async () => {
  vi.stubGlobal("__ZZABBA_RDP_DOCUMENT__", { proof });
  const ended = vi.fn();
  const source = { tabHandle: "owned-tab", tunnelId: "7", targetId: "ssh-server", destination: { host: "::1", port: 3390 } };
  const draft = { host: "rdp.example.invalid", port: 3390, source };
  invoke.mockImplementation((name) => {
    if (name === "rdp_prepare") return Promise.resolve(id);
    if (name === "rdp_connect") return Promise.reject("승인된 터널 목적지가 일치하지 않습니다");
    return Promise.resolve();
  });
  connect(draft, vi.fn(), vi.fn(), vi.fn(), vi.fn(), ended);
  await vi.waitFor(() => expect(ended).toHaveBeenCalledWith("승인된 터널 목적지가 일치하지 않습니다"));
  expect(invoke.mock.calls.map(([name]) => name)).toEqual(["rdp_prepare", "rdp_connect", "rdp_close"]);
  expect(invoke.mock.calls[1]?.[1]).toMatchObject({ id, draft });
  expect(invoke.mock.calls[1]?.[2]).toEqual({ headers: { "x-zzabba-rdp-document": proof } });
});
