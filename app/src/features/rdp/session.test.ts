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
  invoke.mockImplementation((name, args) => {
    if (name === "rdp_prepare") return Promise.resolve(id);
    if (name === "rdp_connect") {
      notify = args.output.onmessage;
      return Promise.withResolvers<void>().promise;
    }
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
  const header = (sequence: number, frameSequence: number, width = 1) => packet({ sequence, kind: "frame", full: true, frameSequence, width, height: 1, x: 0, y: 0, w: width, h: 1 });
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
  return { calls, packet, header, pixels, close, connection, ended, ready, caps, clipboard, canvas, draw };
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
  f.packet({ kind: "display", sequence: 2 });
  await vi.waitFor(() => expect(f.ready).toHaveBeenCalledWith(true));
  view.send(input);
  await vi.waitFor(() => expect(f.calls("rdp_input")[1]).toMatchObject({ id, input: { sequence: 2, action: { kind: "key", code: 0x1e, down: true } } }));
  f.packet({ kind: "channels", sequence: 3, copy: true, paste: true, resize: true });
  await vi.waitFor(() => expect(f.caps).toHaveBeenCalledWith({ copy: true, paste: true, resize: true }));
  f.connection.resize(800, 600);
  expect(f.canvas.width).toBe(1);
  f.header(4, 2, 2);
  await vi.waitFor(() => expect(f.calls("rdp_ack")).toContainEqual({ id, sequence: 4, offset: 0 }));
  f.pixels(4, 2);
  await vi.waitFor(() => expect(f.canvas.width).toBe(2));
  f.connection.clipboard("paste");
  f.connection.clipboard("copy");
  await vi.waitFor(() => expect(f.calls("rdp_input").map((call) => call.input.action.kind)).toEqual(["active", "key", "resize", "copy", "paste"]));
  f.packet({ kind: "clipboard", sequence: 5, epoch: 1, text: "원격 시험 문자열" });
  await vi.waitFor(() => expect(f.clipboard.writeText).toHaveBeenCalledWith("원격 시험 문자열"));
  f.connection.setActive(false);
  view.send(input);
  await vi.waitFor(() => expect(f.calls("rdp_input").slice(-1)[0]).toMatchObject({ input: { action: { kind: "active", active: false } } }));
  expect(f.calls("rdp_input").filter((call) => call.input.action.kind === "key")).toHaveLength(1);
  f.close();
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
