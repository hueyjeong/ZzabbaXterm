import xterm from "@xterm/xterm";
import { expect, it, vi } from "vitest";
import { connectLocalTerminal, type LocalTerminalIpc } from "./session";

it("분할된 한글과 ANSI를 xterm 버퍼에 반영하고 파서 완료 위치만 ACK한다", async () => {
  const size = { cols: 80, rows: 24 };
  const terminal = new xterm.Terminal(size);
  let parserEntered = false;
  const resumeParser = Promise.withResolvers<boolean>();
  const handler = terminal.parser.registerCsiHandler({ final: "m" }, ([param]) => {
    if (param !== 31) return false;
    parserEntered = true;
    return resumeParser.promise;
  });
  const status = vi.fn();
  const open = vi.fn<LocalTerminalIpc["open"]>().mockResolvedValue("parser");
  const ack = vi.fn<LocalTerminalIpc["ack"]>().mockResolvedValue(undefined);
  const unused = async () => { };
  const ipc: LocalTerminalIpc = { open, ack, start: unused, input: unused, resize: unused, close: unused };
  const session = connectLocalTerminal(size, (bytes, parsed) => terminal.write(bytes, parsed), status, ipc);

  try {
    await vi.waitFor(() => expect(status).toHaveBeenLastCalledWith({ kind: "running" }, "parser"), 1_000);
    const [, receive] = open.mock.calls[0];
    for (const chunk of [
      [0xed, 0x95], // '한'의 UTF-8 앞 두 바이트
      [0x9c, 0x1b, 0x5b, 0x33], // '한'의 마지막 바이트와 ESC[3
      [0x31, 0x6d, 0x21], // 1m!
      [0x1b, 0x5b, 0x30, 0x6d, 0x45, 0x4e, 0x44], // ESC[0mEND
    ]) {
      receive(Uint8Array.of(...chunk).buffer);
    }

    expect(ack).not.toHaveBeenCalled();
    await vi.waitFor(() => expect(parserEntered).toBe(true), 1_000);
    expect(terminal.buffer.active.getLine(0)?.translateToString(true)).toBe("한");
    expect(ack.mock.calls).toEqual([["parser", 2], ["parser", 6]]);

    resumeParser.resolve(false); // 기본 SGR 처리기로 넘겨 실제 셀 색을 바꾼다.
    await vi.waitFor(() => expect(ack).toHaveBeenCalledTimes(4));
    expect(ack.mock.calls).toEqual([["parser", 2], ["parser", 6], ["parser", 9], ["parser", 16]]);
    const line = terminal.buffer.active.getLine(0);
    expect(line?.translateToString(true)).toBe("한!END");
    expect(line?.getCell(2)?.isFgPalette()).toBe(true);
    expect(line?.getCell(2)?.getFgColor()).toBe(1);
    expect(line?.getCell(3)?.isFgDefault()).toBe(true);
  } finally {
    resumeParser.resolve(false);
    session.close();
    handler.dispose();
    terminal.dispose();
  }
});

it("SSM 시작 응답을 기다리는 동안 실제 xterm의 VT 응답과 ACK만 보낸다", async () => {
  const size = { cols: 80, rows: 24 };
  const terminal = new xterm.Terminal(size);
  const starting = Promise.withResolvers<void>();
  const open = vi.fn<LocalTerminalIpc["open"]>().mockResolvedValue("ssm-app");
  const start = vi.fn<LocalTerminalIpc["start"]>().mockReturnValue(starting.promise);
  const ack = vi.fn<LocalTerminalIpc["ack"]>().mockResolvedValue(undefined);
  const input = vi.fn<LocalTerminalIpc["input"]>().mockResolvedValue(undefined);
  const status = vi.fn();
  const close = vi.fn().mockResolvedValue(null);
  const session = connectLocalTerminal(size, (bytes, parsed) => terminal.write(bytes, parsed), status,
    { open, start, ack, input, resize: async () => { }, close }, "saved-ssm", true);
  const subscription = terminal.onData(data => session.input(data, "terminal"));
  try {
    await vi.waitFor(() => expect(start).toHaveBeenCalledWith("ssm-app"));
    const [, receive] = open.mock.calls[0];
    receive({ kind: "connecting" });
    expect(status).toHaveBeenLastCalledWith({ kind: "connecting" }, "ssm-app");
    session.input("준비 전 사용자 입력");
    const query = new TextEncoder().encode("\x1b[6n");
    receive(query.buffer);
    await vi.waitFor(() => expect(ack).toHaveBeenCalledWith("ssm-app", query.byteLength));
    const asText = () => input.mock.calls.map(([id, bytes]) => [id, new TextDecoder().decode(bytes)]);
    expect(asText()).toEqual([["ssm-app", "\x1b[1;1R"]]);
    starting.resolve();
    await starting.promise;
    expect(status).toHaveBeenLastCalledWith({ kind: "connecting" }, "ssm-app");
    receive({ kind: "ready" });
    receive({ kind: "connecting" });
    expect(status).toHaveBeenLastCalledWith({ kind: "running" }, "ssm-app");
    session.input("ready");
    await vi.waitFor(() => expect(asText()).toEqual([["ssm-app", "\x1b[1;1R"], ["ssm-app", "ready"]]));
    receive({ kind: "disconnected", value: "SSM 연결 끊김" });
    receive({ kind: "ready" });
    expect(status).toHaveBeenLastCalledWith({ kind: "disconnected", value: "SSM 연결 끊김" }, "ssm-app");
    await session.confirmClose();
    expect(close).toHaveBeenCalledExactlyOnceWith("ssm-app");
  } finally {
    starting.resolve();
    session.close();
    subscription.dispose();
    terminal.dispose();
  }
});
