import { describe, expect, it, vi } from "vitest";
import { rightClick, type RightClickTerminal, type TerminalClipboard } from "./rightClick";

function fakeTerminal(selection: string, mouseTrackingMode = "none") {
  let selected = selection;
  const pasted: string[] = [];
  const term: RightClickTerminal = {
    hasSelection: () => selected !== "",
    getSelection: () => selected,
    clearSelection: () => { selected = ""; },
    paste: (data) => { pasted.push(data); },
    modes: { mouseTrackingMode },
  };
  return { term, pasted, selection: () => selected };
}

function fakeClipboard(text = "붙여넣기") {
  return {
    readText: vi.fn<TerminalClipboard["readText"]>().mockResolvedValue(text),
    writeText: vi.fn<TerminalClipboard["writeText"]>().mockResolvedValue(undefined),
  };
}

describe("터미널 우클릭", () => {
  it("선택 영역이 있으면 복사하고 선택을 지우며 붙여넣지 않는다", async () => {
    const { term, pasted, selection } = fakeTerminal("ls -al");
    const clipboard = fakeClipboard();
    rightClick(term, clipboard, () => true);
    await Promise.resolve();
    expect(clipboard.writeText).toHaveBeenCalledWith("ls -al");
    expect(selection()).toBe("");
    expect(clipboard.readText).not.toHaveBeenCalled();
    expect(pasted).toEqual([]);
  });

  it("선택 영역이 없으면 클립보드를 붙여넣는다", async () => {
    const { term, pasted } = fakeTerminal("");
    const clipboard = fakeClipboard("echo 한글");
    rightClick(term, clipboard, () => true);
    await vi.waitFor(() => expect(pasted).toEqual(["echo 한글"]));
  });

  it("입력을 받지 않거나 클립보드를 읽는 사이 입력이 막히면 붙여넣지 않는다", async () => {
    const blocked = fakeTerminal("");
    const clipboard = fakeClipboard();
    rightClick(blocked.term, clipboard, () => false);
    expect(clipboard.readText).not.toHaveBeenCalled();

    const late = fakeTerminal("");
    let open = true;
    const slow = fakeClipboard();
    rightClick(late.term, slow, () => open);
    open = false;
    await Promise.resolve();
    await Promise.resolve();
    expect(slow.readText).toHaveBeenCalled();
    expect(late.pasted).toEqual([]);
  });

  it("마우스 보고 모드에서는 붙여넣지 않고 선택 복사만 한다", async () => {
    const vim = fakeTerminal("", "vt200");
    const clipboard = fakeClipboard();
    rightClick(vim.term, clipboard, () => true);
    expect(clipboard.readText).not.toHaveBeenCalled();

    const selected = fakeTerminal("word", "vt200");
    rightClick(selected.term, clipboard, () => true);
    expect(clipboard.writeText).toHaveBeenCalledWith("word");
  });

  it("클립보드 오류는 무시한다", async () => {
    const { term } = fakeTerminal("");
    const clipboard = { readText: vi.fn().mockRejectedValue(new Error("denied")), writeText: vi.fn() };
    expect(() => rightClick(term, clipboard, () => true)).not.toThrow();
    await Promise.resolve();
  });
});
