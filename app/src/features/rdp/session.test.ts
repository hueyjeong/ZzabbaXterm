// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { createRemoteDesktopViewSession } from "../remoteDesktop/session";
import { scanCode } from "./keys";
import { APPROVAL_NOTICE } from "../remoteDesktop/session";
import { BUSY_NOTICE, connect, DROPPED_NOTICE, type Connection } from "./session";

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
  const notice = vi.fn();
  const sendInput = vi.fn().mockResolvedValue("ok");
  const sendBatch = vi.fn((inputs: unknown[]) => Promise.resolve(inputs.map(() => "ok")));
  invoke.mockImplementation((name, args) => {
    if (name === "rdp_prepare") return Promise.resolve(id);
    if (name === "rdp_connect") {
      notify = args.output.onmessage;
      return Promise.withResolvers<void>().promise;
    }
    if (name === "rdp_input") return sendInput();
    if (name === "rdp_input_batch") return sendBatch(args.inputs);
    return Promise.resolve();
  });
  const fetch = vi.fn();
  vi.stubGlobal("fetch", fetch);
  const close = connect({ host: "localhost", port: 3389 }, opened.resolve, caps, ready, notice, ended, clipboard);
  const connection = await opened.promise;
  const calls = (name: string) => invoke.mock.calls.filter(([command]) => command === name).map(([, args]) => args);
  /** 단건과 묶음으로 보낸 입력을 보낸 순서대로 펼친다. */
  const inputs = () => invoke.mock.calls.flatMap(([command, args]) =>
    command === "rdp_input" ? [args.input] : command === "rdp_input_batch" ? args.inputs : []);
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
  /** 첫 화면을 적용하고 활성 입력(순번 1)까지 보낸 상태로 만든다. */
  async function live() {
    const view = createRemoteDesktopViewSession(connection, canvas, vi.fn());
    connection.setActive(true);
    header(1, 1);
    await vi.waitFor(() => expect(calls("rdp_ack")).toHaveLength(1));
    pixels(1);
    await vi.waitFor(() => expect(calls("rdp_ack")).toHaveLength(2));
    display(2);
    await vi.waitFor(() => expect(inputs()).toHaveLength(1));
    return view;
  }
  return {
    calls, inputs, live, packet, header, pixels, display, sendInput, sendBatch, close, connection, ended, ready, caps, notice,
    clipboard, canvas, draw, signal: (ticket: string) => notify(ticket),
  };
}

const keys = (count: number) => Array.from({ length: count }, (_, index) => ({
  ...input, code: `Key${String.fromCharCode(65 + (Math.floor(index / 2) % 26))}`, down: index % 2 === 0,
}));

it("한 번에 쌓인 입력 100개를 끊지 않고 순서대로 묶어 보낸다", async () => {
  const f = await fixture();
  const view = await f.live();
  const held = Promise.withResolvers<string>();
  f.sendInput.mockReturnValueOnce(held.promise);
  const events = keys(100);
  for (const event of events) view.send(event);
  held.resolve("ok");
  await vi.waitFor(() => expect(f.inputs()).toHaveLength(101));
  expect(f.ended).not.toHaveBeenCalled();
  const sent = f.inputs().slice(1);
  expect(sent.map((item) => item.sequence)).toEqual(Array.from({ length: 100 }, (_, index) => index + 2));
  expect(sent.map((item) => [item.action.code, item.action.down])).toEqual(events.map((event) => [scanCode(event.code)?.code, event.down]));
  expect(f.calls("rdp_input_batch").map((call) => call.inputs.length)).toEqual([64, 35]);
  view.dispose();
});

it("busy 응답을 받은 입력은 같은 순번으로 다시 보내고 세션을 유지한다", async () => {
  const f = await fixture();
  const view = await f.live();
  f.sendInput.mockResolvedValueOnce("busy");
  view.send(input);
  await vi.waitFor(() => expect(f.calls("rdp_input")).toHaveLength(3));
  const [busy, retried] = f.calls("rdp_input").slice(1);
  expect(busy.input.sequence).toBe(2);
  expect(retried.input).toEqual(busy.input);

  const held = Promise.withResolvers<string>();
  f.sendInput.mockReturnValueOnce(held.promise);
  f.sendBatch.mockResolvedValueOnce(["ok", "busy"]);
  for (const event of keys(4)) view.send(event);
  held.resolve("ok");
  await vi.waitFor(() => expect(f.inputs()).toHaveLength(9));
  const [partial, rest] = f.calls("rdp_input_batch");
  expect(partial.inputs.map((item: { sequence: number }) => item.sequence)).toEqual([4, 5, 6]);
  expect(rest.inputs).toEqual(partial.inputs.slice(1));
  expect(f.ended).not.toHaveBeenCalled();
  view.dispose();
});

it("큐가 넘쳐 키 입력을 버리면 알리고 큐가 빈 뒤 한 번 해제한다", async () => {
  const f = await fixture();
  const view = await f.live();
  const held = Promise.withResolvers<string>();
  f.sendInput.mockReturnValueOnce(held.promise);
  for (const event of keys(520)) view.send(event);
  expect(f.notice).toHaveBeenCalledExactlyOnceWith(DROPPED_NOTICE);
  held.resolve("ok");
  await vi.waitFor(() => expect(f.inputs().slice(-1)[0]).toMatchObject({ action: { kind: "release" } }));
  const sent = f.inputs();
  expect(sent).toHaveLength(1 + 1 + 512 + 1);
  expect(sent.filter((item) => item.action.kind === "release")).toHaveLength(1);
  expect(f.ended).not.toHaveBeenCalled();
  view.dispose();
});

it("네이티브 승인 창이 열려 있는 동안 승인 대기를 알린다", async () => {
  const f = await fixture();
  f.signal("approval");
  expect(f.notice).toHaveBeenLastCalledWith(APPROVAL_NOTICE);
  f.signal("approval-done");
  expect(f.notice).toHaveBeenLastCalledWith("");
  expect(f.ended).not.toHaveBeenCalled();
});

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
  const discarded = Promise.withResolvers<string>();
  f.sendInput.mockReturnValueOnce(discarded.promise);
  view.send(input);
  view.send(input);
  discarded.resolve("stale");
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
  await vi.waitFor(() => expect(f.ended).toHaveBeenCalledWith("RDP 전송 연결이 끊겼습니다", true));
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
  await vi.waitFor(() => expect(ended).toHaveBeenCalledWith("승인된 터널 목적지가 일치하지 않습니다", true));
  expect(invoke.mock.calls.map(([name]) => name)).toEqual(["rdp_prepare", "rdp_connect", "rdp_close"]);
  expect(invoke.mock.calls[1]?.[1]).toMatchObject({ id, draft });
  expect(invoke.mock.calls[1]?.[2]).toEqual({ headers: { "x-zzabba-rdp-document": proof } });
});

it("준비 중 닫기는 발급된 세션의 종료를 기다리고 연결을 시작하지 않는다", async () => {
  vi.stubGlobal("__ZZABBA_RDP_DOCUMENT__", { proof });
  const prepared = Promise.withResolvers<typeof id>();
  const released = Promise.withResolvers<void>();
  invoke.mockImplementation((name) => name === "rdp_prepare" ? prepared.promise : released.promise);
  const opened = vi.fn();
  const settled = vi.fn();
  const close = connect({ host: "localhost", port: 3389 }, opened, vi.fn(), vi.fn(), vi.fn(), vi.fn());
  const pending = close().then(settled);
  expect(invoke.mock.calls.map(([name]) => name)).toEqual(["rdp_prepare"]);
  prepared.resolve(id);
  await vi.waitFor(() => expect(invoke.mock.calls.map(([name]) => name)).toEqual(["rdp_prepare", "rdp_close"]));
  expect(settled).not.toHaveBeenCalled();
  expect(opened).not.toHaveBeenCalled();
  released.resolve();
  await pending;
  expect(settled).toHaveBeenCalledOnce();
});

it("종료 실패 후 다시 확인하면 같은 세션으로만 요청한다", async () => {
  const f = await fixture();
  const first = Promise.withResolvers<void>();
  invoke.mockReturnValueOnce(first.promise);
  const pending = f.close();
  expect(f.close()).toBe(pending);
  first.reject("RDP 종료 확인 불가");
  await expect(pending).rejects.toBe("RDP 종료 확인 불가");
  await f.close();
  expect(f.calls("rdp_close")).toEqual([{ id }, { id }]);
  expect(f.calls("rdp_prepare")).toHaveLength(1);
  expect(f.calls("rdp_connect")).toHaveLength(1);
});

it("TLS 전용 opt-in을 연결 초안에 전달한다", async () => {
  vi.stubGlobal("__ZZABBA_RDP_DOCUMENT__", { proof });
  const ended = vi.fn();
  invoke.mockImplementation((name) => {
    if (name === "rdp_prepare") return Promise.resolve(id);
    if (name === "rdp_connect") return Promise.withResolvers<void>().promise;
    return Promise.resolve();
  });
  connect({ host: "rdp.example.invalid", port: 3389, tlsOnly: true }, vi.fn(), vi.fn(), vi.fn(), vi.fn(), ended);
  await vi.waitFor(() => expect(invoke.mock.calls.map(([name]) => name)).toContain("rdp_connect"));
  const draft = invoke.mock.calls.find(([name]) => name === "rdp_connect")?.[1]?.draft;
  expect(draft).toEqual({ host: "rdp.example.invalid", port: 3389, tlsOnly: true });
});

it("복사가 busy이거나 진행 중이어도 다시 보내지 않고 뒤 키 입력을 계속 보낸다", async () => {
  const f = await fixture();
  const view = await f.live();
  f.packet({ kind: "channels", sequence: 3, copy: true, paste: true, resize: true });
  await vi.waitFor(() => expect(f.caps).toHaveBeenCalled());
  f.sendInput.mockResolvedValueOnce("busy");
  f.connection.clipboard("copy");
  view.send(input);
  await vi.waitFor(() => expect(f.inputs()).toHaveLength(3));
  expect(f.notice).toHaveBeenCalledWith(BUSY_NOTICE);
  expect(f.inputs().slice(1).map((item) => [item.action.kind, item.sequence])).toEqual([["copy", 2], ["key", 2]]);

  f.sendInput.mockRejectedValueOnce({ message: "RDP 복사가 진행 중입니다" });
  f.connection.clipboard("copy");
  view.send({ ...input, down: false });
  await vi.waitFor(() => expect(f.inputs()).toHaveLength(5));
  expect(f.notice).toHaveBeenCalledWith("RDP 복사가 진행 중입니다");
  expect(f.inputs().slice(3).map((item) => [item.action.kind, item.sequence])).toEqual([["copy", 3], ["key", 4]]);
  expect(f.calls("rdp_input_batch")).toEqual([]);
  expect(f.ended).not.toHaveBeenCalled();
  view.dispose();
});

it("3초 넘게 busy인 입력은 버리고 큐가 빈 뒤 해제한다", async () => {
  const f = await fixture();
  const view = await f.live();
  let now = 1_000;
  vi.spyOn(Date, "now").mockImplementation(() => now);
  f.sendInput.mockResolvedValue("busy");
  view.send(input);
  await vi.waitFor(() => expect(f.calls("rdp_input").length).toBeGreaterThan(2));
  now += 3_000;
  f.sendInput.mockResolvedValue("ok");
  f.sendInput.mockResolvedValueOnce("busy");
  await vi.waitFor(() => expect(f.inputs().slice(-1)[0]).toMatchObject({ action: { kind: "release" }, sequence: 2 }));
  expect(f.notice).toHaveBeenCalledWith(DROPPED_NOTICE);
  expect(f.inputs().filter((item) => item.action.kind === "key").every((item) => item.sequence === 2)).toBe(true);
  expect(f.ended).not.toHaveBeenCalled();
  view.dispose();
});

it("큐가 넘쳐도 마지막 포인터 위치는 버리지 않는다", async () => {
  const f = await fixture();
  const view = await f.live();
  const held = Promise.withResolvers<string>();
  f.sendInput.mockReturnValueOnce(held.promise);
  view.send(input);
  const pointer = (x: number) => ({ kind: "pointer", x, y: 0, buttons: 0 }) as const;
  // 버릴 수 있는 이동은 큐의 마지막 포인터뿐이다. 그 위치는 남기고 새 키를 버린다.
  view.send(pointer(0));
  for (const event of keys(510)) view.send(event);
  view.send(pointer(0));
  view.send({ ...input, code: "Digit1" });
  expect(f.notice).toHaveBeenCalledExactlyOnceWith(DROPPED_NOTICE);
  held.resolve("ok");
  await vi.waitFor(() => expect(f.inputs().slice(-1)[0]).toMatchObject({ action: { kind: "release" } }));
  expect(f.inputs().filter((item) => item.action.kind === "pointer")).toHaveLength(2);
  expect(f.inputs().some((item) => item.action.code === scanCode("Digit1")?.code)).toBe(false);
  expect(f.ended).not.toHaveBeenCalled();
  view.dispose();
});

it("연결 종료 이벤트의 전송 끊김 종류를 탭에 넘긴다", async () => {
  vi.stubGlobal("__ZZABBA_RDP_DOCUMENT__", { proof });
  const ended = vi.fn();
  invoke.mockImplementation((name) => {
    if (name === "rdp_prepare") return Promise.resolve(id);
    if (name === "rdp_connect") return Promise.reject({ kind: "transport", message: "RDP 전송 연결이 끊겼습니다" });
    return Promise.resolve();
  });
  connect({ host: "rdp.example.invalid", port: 3389 }, vi.fn(), vi.fn(), vi.fn(), vi.fn(), ended);
  await vi.waitFor(() => expect(ended).toHaveBeenCalledWith("RDP 전송 연결이 끊겼습니다", true, "transport"));
});
