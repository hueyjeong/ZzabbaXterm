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
