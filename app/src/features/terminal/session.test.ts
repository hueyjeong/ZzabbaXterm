import { invoke } from "@tauri-apps/api/core";
import { describe, expect, it, vi } from "vitest";
import { connectLocalTerminal, type LocalTerminalIpc, type TerminalStatus } from "./session";

vi.mock("@tauri-apps/api/core", () => ({ Channel: class {}, invoke: vi.fn() }));

const dimensions = { rows: 24, cols: 80 };

function nativePeer() {
  const firstInput = Promise.withResolvers<void>();
  const opening = Promise.withResolvers<string>();
  const start = vi.fn(async (_id: string) => { });
  const ack = vi.fn(async (_id: string, _position: number) => { });
  const close = vi.fn(async (_id: string) => { });
  const input = vi.fn<LocalTerminalIpc["input"]>().mockResolvedValue(undefined);
  const open = vi.fn<LocalTerminalIpc["open"]>(() => opening.promise);
  const resize = vi.fn<LocalTerminalIpc["resize"]>().mockResolvedValue(undefined);
  const ipc: LocalTerminalIpc = { open, start, ack, close, input, resize };
  const callbacks: (() => void)[] = [];
  const write = vi.fn((_bytes: Uint8Array, parsed: () => void) => callbacks.push(parsed));
  const status = vi.fn<(status: TerminalStatus, sessionId?: string) => void>();
  const session = connectLocalTerminal(dimensions, write, status, ipc);
  async function openAs(id: string) {
    await vi.waitFor(() => expect(open).toHaveBeenCalledTimes(1));
    opening.resolve(id);
  }
  function send(bytes: ArrayBuffer | { kind: "exited"; value: number } | { kind: "disconnected"; value: string } | { kind: "notice"; value: string }) {
    const receiver = open.mock.calls[0]?.[1];
    if (!receiver) throw new Error("세션 수신기가 없습니다");
    receiver(bytes);
  }
  return { open, start, input, ack, close, resize, write, status, callbacks, session, send, openAs, firstInput };
}

describe("L01 터미널 세션", () => {
  it("기본 열기 요청의 형태를 유지하고 nano 옵트인에만 remoteEditor를 보낸다", async () => {
    const ipcInvoke = vi.mocked(invoke).mockRejectedValue(new Error("시험 종료"));
    const write = vi.fn();
    const status = vi.fn();
    connectLocalTerminal(dimensions, write, status);
    connectLocalTerminal(dimensions, write, status, undefined, "saved-ssh");
    connectLocalTerminal(dimensions, write, status, undefined, "saved-ssh", false, true, true);
    await vi.waitFor(() => expect(ipcInvoke.mock.calls).toEqual([
      ["terminal_open", { ...dimensions, output: expect.any(Object) }],
      ["terminal_open", { ...dimensions, output: expect.any(Object), targetId: "saved-ssh" }],
      ["terminal_open", { ...dimensions, output: expect.any(Object), targetId: "saved-ssh", remoteEditor: true }],
    ]));
  });

  it("저장 인증 폴백 알림을 실행 상태에 표시한다", async () => {
    const peer = nativePeer();
    const starting = Promise.withResolvers<void>();
    peer.start.mockReturnValueOnce(starting.promise);
    await peer.openAs("a");
    const notice = "저장된 인증을 사용할 수 없어 직접 입력으로 전환했습니다: Bitwarden 금고가 잠겨 있습니다";
    peer.send({ kind: "notice", value: notice });
    starting.resolve();
    await vi.waitFor(() => expect(peer.status).toHaveBeenLastCalledWith({ kind: "running", notice }, "a"));
  });

  it("세션별 바이트를 분리하고 파서 완료까지 누적 ACK를 보류한다", async () => {
    const a = nativePeer();
    const b = nativePeer();
    await Promise.all([a.openAs("a"), b.openAs("b")]);
    await vi.waitFor(() => expect(b.start).toHaveBeenCalledWith("b"));
    const chunk = new Uint8Array([0xed, 0x95]);
    a.send(chunk.buffer);
    a.send(new Uint8Array([0x9c]).buffer);
    b.send(new Uint8Array([0x1b, 0x5b]).buffer);
    expect(a.write.mock.calls.map(([bytes]) => [...bytes])).toEqual([[0xed, 0x95], [0x9c]]);
    expect(b.write.mock.calls.map(([bytes]) => [...bytes])).toEqual([[0x1b, 0x5b]]);
    expect(a.ack).not.toHaveBeenCalled();
    a.callbacks.forEach((parse) => parse());
    expect(a.ack.mock.calls).toEqual([["a", 2], ["a", 3]]);
    expect(b.ack).not.toHaveBeenCalled();
    a.send({ kind: "exited", value: 7 });
    expect(a.status).toHaveBeenLastCalledWith({ kind: "exited", value: 7 }, "a");
    a.session.input("ignored");
    expect(a.input).not.toHaveBeenCalled();
    b.session.close();
  });

  it("시작 응답 전의 입력은 보내지 않고 초기 VT 질의는 시작 뒤에 처리한다", async () => {
    const starting = Promise.withResolvers<void>();
    const peer = nativePeer();
    peer.start.mockReturnValueOnce(starting.promise);
    peer.write.mockImplementation(() => {
      peer.session.input("\x1b[1;1R");
      return 0;
    });
    peer.session.input("ID 발급 전");
    await peer.openAs("a");
    await vi.waitFor(() => expect(peer.start).toHaveBeenCalledWith("a"));
    peer.session.input("시작 응답 전");
    peer.send(new Uint8Array([0x1b, 0x5b, 0x36, 0x6e]).buffer);
    await Promise.resolve();
    expect(peer.status).toHaveBeenLastCalledWith({ kind: "opening" }, "a");
    expect(peer.input).not.toHaveBeenCalled();
    starting.resolve();
    await vi.waitFor(() => expect(peer.status).toHaveBeenLastCalledWith({ kind: "running" }, "a"));
    peer.session.input("준비된 입력");
    await vi.waitFor(() => expect(peer.input).toHaveBeenCalledTimes(2));
    expect(peer.input.mock.calls.map(([id, bytes]) => [id, new TextDecoder().decode(bytes)])).toEqual([
      ["a", "\x1b[1;1R"], ["a", "준비된 입력"],
    ]);
    peer.session.close();
  });

  it("키와 붙여넣기를 한 번씩 순서대로 보내고 닫힌 탭의 대기 키는 버린다", async () => {
    const peer = nativePeer();
    peer.input.mockReturnValueOnce(peer.firstInput.promise);
    await peer.openAs("a");
    await vi.waitFor(() => expect(peer.start).toHaveBeenCalledWith("a"));
    peer.session.input("한\n둘");
    peer.session.input("\x03");
    await vi.waitFor(() => expect(peer.input).toHaveBeenCalledTimes(1));
    peer.firstInput.resolve();
    await vi.waitFor(() => expect(peer.input).toHaveBeenCalledTimes(2));
    expect(peer.input.mock.calls.map(([id, bytes]) => [id, new TextDecoder().decode(bytes)])).toEqual([["a", "한\n둘"], ["a", "\x03"]]);
    peer.session.input("discarded");
    peer.session.close();
    await Promise.resolve();
    expect(peer.input).toHaveBeenCalledTimes(2);
    expect(peer.close).toHaveBeenCalledExactlyOnceWith("a");
  });

  it("열기 응답 전에 닫힌 탭은 받은 Rust ID를 시작 없이 회수한다", async () => {
    const peer = nativePeer();
    await vi.waitFor(() => expect(peer.open).toHaveBeenCalledTimes(1));
    peer.session.close();
    await peer.openAs("opening");
    await vi.waitFor(() => expect(peer.close).toHaveBeenCalledExactlyOnceWith("opening"));
    expect(peer.start).not.toHaveBeenCalled();
  });

  it("오래된 크기 요청 실패는 건너뛰고 현재 세션의 실제 오류를 표시한다", async () => {
    const stale = Promise.withResolvers<void>();
    const peer = nativePeer();
    peer.resize.mockReturnValueOnce(stale.promise).mockRejectedValueOnce("resize: OS 오류");
    await peer.openAs("a");
    await vi.waitFor(() => expect(peer.status).toHaveBeenLastCalledWith({ kind: "running" }, "a"));
    peer.session.resize({ rows: 32, cols: 100 });
    await vi.waitFor(() => expect(peer.resize).toHaveBeenCalledTimes(1));
    peer.session.resize({ rows: 40, cols: 120 });
    stale.reject("resize: 오래된 요청");
    await vi.waitFor(() => expect(peer.status).toHaveBeenLastCalledWith({ kind: "disconnected", value: "resize: OS 오류" }, "a"));
    expect(peer.close).toHaveBeenCalledExactlyOnceWith("a");
  });

  it("닫힌 ID의 늦은 ACK가 실제 단절 이유를 가리지 않는다", async () => {
    const peer = nativePeer();
    peer.ack.mockRejectedValueOnce("세션이 닫혔습니다");
    await peer.openAs("a");
    await vi.waitFor(() => expect(peer.status).toHaveBeenLastCalledWith({ kind: "running" }, "a"));
    peer.send(new Uint8Array([65]).buffer);
    peer.callbacks[0]?.();
    await vi.waitFor(() => expect(peer.ack).toHaveBeenCalledWith("a", 1));
    await Promise.resolve();
    peer.send({ kind: "disconnected", value: "read: OS 오류" });
    expect(peer.status).toHaveBeenLastCalledWith({ kind: "disconnected", value: "read: OS 오류" }, "a");
  });
});
