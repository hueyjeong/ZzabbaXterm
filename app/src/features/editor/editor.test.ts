// @vitest-environment jsdom
import { editor, Uri } from "monaco-editor/editor";
import { describe, expect, it, vi } from "vitest";
import type { ApprovedEditorFile, EditorFileHandle, EditorReadResult, EditorSaveResult, EditorTransport } from "./contract";
import { connectEditor, type EditorState } from "./editor";
import { createFileModel } from "./monaco";

function file(id: string): ApprovedEditorFile {
  return { handle: id as EditorFileHandle, label: "한글.txt", text: "\uFEFF처음\r\n" };
}

function transport(...files: ApprovedEditorFile[]) {
  const backing = new Map(files.map(({ handle, text }) => [handle, text]));
  const baseline = new Map(backing);
  async function compare(handle: EditorFileHandle): Promise<EditorReadResult> {
    const text = backing.get(handle);
    return text === undefined ? { kind: "denied", message: "unapproved handle" } : { kind: "read", text };
  }
  return {
    backing,
    compare,
    async reload(handle: EditorFileHandle): Promise<EditorReadResult> {
      const result = await compare(handle);
      if (result.kind === "read") baseline.set(handle, result.text);
      return result;
    },
    async save(handle: EditorFileHandle, text: string): Promise<EditorSaveResult> {
      const original = backing.get(handle);
      if (original === undefined) return { kind: "denied", message: "unapproved handle" };
      if (original !== baseline.get(handle)) return { kind: "conflict", current: original };
      backing.set(handle, text);
      baseline.set(handle, text);
      return { kind: "saved" };
    },
  };
}

function open(approved: ApprovedEditorFile, io: EditorTransport) {
  const model = createFileModel(approved);
  const states: EditorState[] = [];
  const session = connectEditor(approved, model, io, (state) => states.push(state));
  return {
    model, session, states,
    get state() { return states[states.length - 1]; },
    text: () => model.getValue(undefined, true),
    close() { session.dispose(); model.dispose(); },
  };
}

describe("파일 편집 버퍼", () => {
  it("동명 파일의 모델과 저장 내용을 핸들별로 격리하고 한글·CRLF·BOM을 보존한다", async () => {
    const left = file("left:/#one");
    const right = file("right:/#one");
    const io = transport(left, right);
    const a = open(left, io);
    const b = open(right, io);
    try {
      expect(a.model.uri.toString()).not.toBe(b.model.uri.toString());
      a.model.applyEdits([{ range: a.model.getFullModelRange(), text: "왼쪽\n" }]);
      b.model.setValue("오른쪽\n");
      await a.session.save();
      expect(io.backing.get(left.handle)).toBe("\uFEFF왼쪽\r\n");
      expect(io.backing.get(right.handle)).toBe(right.text);
      expect(a.state).toMatchObject({ dirty: false, status: { kind: "saved" } });
      expect(b.text()).toBe("오른쪽\n");
      expect(b.state?.dirty).toBe(true);
      await b.session.save();
      expect(io.backing.get(right.handle)).toBe("오른쪽\n");
    } finally {
      a.close();
      b.close();
    }
  });

  it("Monaco가 정규화한 혼합 개행과 BOM을 저장 때 복원한다", async () => {
    const approved = { ...file("mixed-eol"), text: "\uFEFFa\r\nb\nc" };
    const io = transport(approved);
    const tab = open(approved, io);
    try {
      tab.model.applyEdits([{ range: { startLineNumber: 3, startColumn: 1, endLineNumber: 3, endColumn: 2 }, text: "C" }]);
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe("\uFEFFa\r\nb\nC");
      const removed = { startLineNumber: 1, startColumn: 2, endLineNumber: 2, endColumn: 1 };
      tab.model.pushEditOperations(null, [{ range: removed, text: "" }], () => null);
      tab.model.pushStackElement();
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe("\uFEFFab\nC");
      await tab.model.undo();
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe("\uFEFFa\r\nb\nC");
      await tab.model.redo();
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe("\uFEFFab\nC");
      io.backing.set(approved.handle, "x\ny\r\nz");
      await tab.session.reload(async () => true);
      tab.model.applyEdits([{ range: { startLineNumber: 3, startColumn: 1, endLineNumber: 3, endColumn: 2 }, text: "Z" }]);
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe("x\ny\r\nZ");
    } finally {
      tab.close();
    }
  });

  it("Monaco가 병합한 1,000개 교체는 원본을 그대로 두고 편집 버퍼를 유지한다", async () => {
    const original = Array.from({ length: 1000 }, (_, i) => `a${i}${i % 2 ? "\n" : "\r\n"}`).join("");
    const approved = { ...file("merged-edits"), text: original };
    const io = transport(approved);
    const save = vi.fn(io.save);
    const tab = open(approved, { ...io, save });
    const events: number[] = [];
    const observer = tab.model.onDidChangeContent((event) => events.push(event.changes.length));
    try {
      tab.model.pushEditOperations(null, Array.from({ length: 1000 }, (_, i) => ({
        range: { startLineNumber: i + 1, startColumn: 1, endLineNumber: i + 1, endColumn: 2 }, text: "B",
      })), () => null);
      tab.model.pushStackElement();
      expect(events).toEqual([1]);
      const edited = original.replace(/a/g, "B").replace(/\r\n|\n/g, tab.model.getEOL());
      await tab.session.save();
      expect(save).not.toHaveBeenCalled();
      expect(io.backing.get(approved.handle)).toBe(original);
      expect(tab.text()).toBe(edited);
      expect(tab.state).toMatchObject({ dirty: true, status: { kind: "failed", message: "원래 줄바꿈을 확인할 수 없어 저장할 수 없습니다" } });
      await tab.model.undo();
      tab.model.applyEdits([{ range: { startLineNumber: 1000, startColumn: 1, endLineNumber: 1000, endColumn: 2 }, text: "C" }]);
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe(original.replace("a999", "C999"));
    } finally {
      observer.dispose();
      tab.close();
    }
  });

  it("혼합 개행 밖의 같은 형식 교체는 해당 범위만 바꾼다", async () => {
    const approved = { ...file("bounded-replace"), text: "a\nB\r\nC\nD" };
    const io = transport(approved);
    const tab = open(approved, io);
    try {
      tab.model.applyEdits([{ range: { startLineNumber: 1, startColumn: 1, endLineNumber: 2, endColumn: 2 }, text: "X\nY\nZ" }]);
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe("X\nY\nZ\r\nC\nD");
      tab.model.applyEdits([{ range: { startLineNumber: 3, startColumn: 1, endLineNumber: 4, endColumn: 2 }, text: "z\nc" }]);
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe("X\nY\nz\r\nc\nD");
    } finally {
      tab.close();
    }
  });

  it("비교나 다시 읽기가 실패하면 이전 원본을 숨기고 미저장 버퍼와 오류를 유지한다", async () => {
    const approved = file("stale-comparison");
    const io = transport(approved);
    const compare = vi.fn(io.compare);
    const tab = open(approved, { ...io, compare });
    try {
      tab.model.setValue("미저장");
      await tab.session.compare();
      expect(tab.state?.comparison).toBe(approved.text);
      compare.mockResolvedValueOnce({ kind: "failed", message: "missing" });
      await tab.session.compare();
      expect(tab.state).toMatchObject({ dirty: true, comparison: undefined, status: { kind: "failed", message: "missing" } });
      await tab.session.compare();
      compare.mockRejectedValueOnce(new Error("응답 유실"));
      await tab.session.compare();
      expect(tab.state).toMatchObject({ dirty: true, comparison: undefined, status: { kind: "failed", message: "응답 유실" } });
      await tab.session.compare();
      io.backing.delete(approved.handle);
      await tab.session.reload(async () => true);
      expect(tab.state).toMatchObject({ dirty: true, comparison: undefined, status: { kind: "denied", message: "unapproved handle" } });
      expect(tab.text()).toBe("미저장");
    } finally {
      tab.close();
    }
  });

  it("같은 길이의 외부 변경을 비교만 하거나 다시 읽기를 취소하면 원본과 버퍼를 그대로 둔다", async () => {
    const approved = file("conflict");
    const io = transport(approved);
    const tab = open(approved, io);
    try {
      const external = "\uFEFF외부\r\n";
      tab.model.setValue("내 편집\r\n");
      io.backing.set(approved.handle, external);
      await tab.session.save();
      expect(tab.state).toMatchObject({ dirty: true, status: { kind: "conflict" }, comparison: external });
      await tab.session.compare();
      await tab.session.reload(async () => false);
      expect(tab.text()).toBe("내 편집\r\n");
      expect(io.backing.get(approved.handle)).toEqual(external);
      await tab.session.save();
      expect(tab.state?.status.kind).toBe("conflict");
      await tab.session.reload(async () => true);
      expect(tab.text()).toBe(external);
      expect(tab.state?.dirty).toBe(false);
      tab.model.setValue("승인 후 편집\n");
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe("승인 후 편집\n");
    } finally {
      tab.close();
    }
  });

  it("편집하지 않은 탭도 다시 읽기 확인을 취소하면 기존 보기를 유지한다", async () => {
    const approved = file("clean-reload");
    const io = transport(approved);
    const tab = open(approved, io);
    try {
      const external = "외부";
      io.backing.set(approved.handle, external);
      await tab.session.reload(async () => false);
      expect(tab.text()).toBe(approved.text);
      expect(tab.state).toMatchObject({ dirty: false, comparison: undefined });
      await tab.session.reload(async () => true);
      expect(tab.text()).toBe(external);
      expect(tab.state?.comparison).toBeUndefined();
    } finally {
      tab.close();
    }
  });


  it("활성 세대와 닫기 이후에는 대기 응답이 보이거나 저장을 시작하지 않는다", async () => {
    const approved = file("stale-response");
    const replies = [Promise.withResolvers<EditorReadResult>(), Promise.withResolvers<EditorReadResult>()];
    const compare = vi.fn<EditorTransport["compare"]>()
      .mockReturnValueOnce(replies[0].promise).mockReturnValueOnce(replies[1].promise);
    const save = vi.fn(transport(approved).save);
    const tab = open(approved, { ...transport(approved), compare, save });
    const close = vi.fn();
    try {
      tab.model.setValue("미저장");
      const first = tab.session.compare();
      tab.session.setActive(false);
      const inactiveState = tab.state;
      await tab.session.save();
      replies[0].resolve({ kind: "read", text: "이전 활성 세대" });
      await first;
      expect(tab.state).toBe(inactiveState);
      tab.session.setActive(true);
      expect(tab.state?.comparison).toBeUndefined();
      const second = tab.session.compare();
      await tab.session.requestClose(async () => true, close);
      expect(close).toHaveBeenCalledWith(approved.handle);
      const closedState = tab.state;
      replies[1].resolve({ kind: "read", text: "닫힌 탭" });
      await second;
      await tab.session.save();
      expect(tab.state).toBe(closedState);
      expect(save).not.toHaveBeenCalled();
    } finally {
      tab.close();
    }
  });

  it("다시 읽기 확인 이후 편집하거나 탭을 전환하면 이전 요청이 버퍼를 바꾸지 못한다", async () => {
    const approved = file("stale-reload");
    const confirmation = Promise.withResolvers<boolean>();
    const reply = Promise.withResolvers<EditorReadResult>();
    const reload = vi.fn(() => reply.promise);
    const tab = open(approved, { ...transport(approved), reload });
    try {
      tab.model.setValue("편집 1");
      const first = tab.session.reload(() => confirmation.promise);
      tab.model.setValue("편집 2");
      confirmation.resolve(true);
      await first;
      expect(reload).not.toHaveBeenCalled();
      const second = tab.session.reload(async () => true);
      await vi.waitFor(() => expect(reload).toHaveBeenCalledWith(approved.handle));
      tab.session.setActive(false);
      tab.session.setActive(true);
      reply.resolve({ kind: "read", text: "디스크" });
      await second;
      expect(tab.text()).toBe("편집 2");
      expect(tab.state).toMatchObject({ dirty: true, needsReload: true });
    } finally {
      tab.close();
    }
  });

  it("저장 중 추가 입력은 늦은 성공 뒤에도 미저장으로 남긴다", async () => {
    const approved = file("slow-save");
    const io = transport(approved);
    const gate = Promise.withResolvers<void>();
    const tab = open(approved, { ...io, save: async (...args) => { await gate.promise; return io.save(...args); } });
    try {
      tab.model.setValue("저장 대상");
      const saving = tab.session.save();
      tab.model.setValue("다음 편집");
      gate.resolve();
      await saving;
      expect(io.backing.get(approved.handle)).toBe("저장 대상");
      expect(tab.text()).toBe("다음 편집");
      expect(tab.state?.dirty).toBe(true);
      await tab.session.save();
      expect(io.backing.get(approved.handle)).toBe("다음 편집");
      expect(tab.state?.dirty).toBe(false);
    } finally {
      tab.close();
    }
  });

  it("탭을 바꾼 사이 도착한 저장 결과는 다른 탭이나 원래 버퍼에 적용되지 않는다", async () => {
    const first = file("delayed-left");
    const second = file("delayed-right");
    const io = transport(first, second);
    const gate = Promise.withResolvers<void>();
    const a = open(first, { ...io, save: async (...args) => { await gate.promise; return io.save(...args); } });
    const b = open(second, io);
    try {
      b.session.setActive(false);
      a.model.setValue("첫 파일 저장본");
      const saving = a.session.save();
      a.session.setActive(false);
      b.session.setActive(true);
      b.model.setValue("둘째 파일 편집본");
      const inactiveState = a.state;
      const otherState = b.state;
      gate.resolve();
      await saving;
      expect(a.state).toBe(inactiveState);
      expect(b.state).toBe(otherState);
      expect(io.backing.get(first.handle)).toBe("첫 파일 저장본");
      expect(io.backing.get(second.handle)).toBe(second.text);
      a.session.setActive(true);
      expect(a.state).toMatchObject({ saving: false, dirty: true, status: { kind: "unknown" } });
      expect(b.text()).toBe("둘째 파일 편집본");
    } finally {
      a.close();
      b.close();
    }
  });

  it("저장 응답이 오기 전에는 닫기 확정으로 넘어가지 않는다", async () => {
    const approved = file("save-close");
    const io = transport(approved);
    const gate = Promise.withResolvers<void>();
    const tab = open(approved, { ...io, save: async (...args) => { await gate.promise; return io.save(...args); } });
    const confirm = vi.fn(async () => true);
    const close = vi.fn(() => tab.close());
    try {
      tab.model.setValue("저장할 편집본");
      const saving = tab.session.save();
      await tab.session.requestClose(confirm, close);
      expect(tab.state).toMatchObject({ saving: true, busy: "save" });
      expect(close).not.toHaveBeenCalled();
      expect(confirm).not.toHaveBeenCalled();
      expect(io.backing.get(approved.handle)).toBe(approved.text);
      gate.resolve();
      await saving;
      expect(tab.state).toMatchObject({ saving: false, busy: null, dirty: false, status: { kind: "saved" } });
      expect(io.backing.get(approved.handle)).toBe("저장할 편집본");
      await tab.session.requestClose(confirm, close);
      expect(tab.model.isDisposed()).toBe(true);
      expect(confirm).not.toHaveBeenCalled();
    } finally {
      if (!tab.model.isDisposed()) tab.close();
    }
  });

  it.each([true, false])("비교를 닫아도 충돌 시 원본 존재 상태(%s)는 유지한다", async (originalExists) => {
    const approved = file("dismiss-conflict");
    const io = transport(approved);
    const original = "디스크 원본";
    const tab = open(approved, {
      ...io,
      async save() { return { kind: "conflict", current: originalExists ? original : null }; },
    });
    try {
      tab.model.setValue("미저장 편집본");
      await tab.session.save();
      expect(tab.state?.comparison).toEqual(originalExists ? original : null);
      tab.session.dismissComparison();
      expect(tab.state).toMatchObject({ comparison: undefined, status: { kind: "conflict", originalExists }, dirty: true });
      expect(tab.text()).toBe("미저장 편집본");
    } finally {
      tab.close();
    }
  });

  it("단절 뒤 늦은 저장 성공은 무시하고 다시 읽어서 실제 원본과 비교한다", async () => {
    const approved = file("disconnect");
    const io = transport(approved);
    const reply = Promise.withResolvers<void>();
    let disconnect: ((message: string) => void) | undefined;
    const tab = open(approved, {
      ...io,
      async save(...args) { await reply.promise; return io.save(...args); },
      subscribeDisconnect(_handle, notify) { disconnect = notify; return () => { disconnect = undefined; }; },
    });
    try {
      disconnect?.("idle");
      expect(tab.state?.status.kind).toBe("disconnected");
      tab.model.setValue("서버가 받은 내용");
      const saving = tab.session.save();
      tab.model.setValue("미저장 편집본");
      disconnect?.("lost final response");
      expect(tab.state).toMatchObject({ saving: true, busy: null, dirty: true, status: { kind: "unknown" } });
      const close = vi.fn();
      await tab.session.requestClose(async () => true, close);
      expect(close).not.toHaveBeenCalled();
      await tab.session.compare();
      expect(tab.state).toMatchObject({ comparison: approved.text, status: { kind: "compare" }, dirty: true });
      reply.resolve();
      await saving;
      expect(io.backing.get(approved.handle)).toBe("서버가 받은 내용");
      expect(tab.state).toMatchObject({ saving: false, status: { kind: "compare" }, dirty: true });
      await tab.session.compare();
      expect(tab.state?.comparison).toBe("서버가 받은 내용");
      expect(tab.text()).toBe("미저장 편집본");
    } finally {
      tab.close();
    }
    expect(disconnect).toBeUndefined();
    expect(editor.getModel(Uri.from({ scheme: "zzabba-editor", path: "/file", query: approved.handle }))).toBeNull();
  });

  it("확정 실패, 접근 거부, 단절과 결과 미확인을 구분한다", async () => {
    const approved = file("errors");
    const save = vi.fn<EditorTransport["save"]>();
    const tab = open(approved, { ...transport(approved), save });
    try {
      tab.model.setValue("미저장");
      for (const kind of ["failed", "denied", "disconnected", "unknown"] as const) {
        save.mockResolvedValueOnce({ kind, message: "전송 응답" });
        await tab.session.save();
        expect(tab.state).toMatchObject({ dirty: true, status: { kind, message: "전송 응답" } });
        expect(tab.text()).toBe("미저장");
      }
      save.mockRejectedValueOnce(new Error("응답 유실"));
      await tab.session.save();
      expect(tab.state?.status).toEqual({ kind: "unknown", message: "응답 유실" });
    } finally {
      tab.close();
    }
  });

  it("저장, 다른 탭 닫기, 폐기 취소 뒤에는 대상 탭의 닫기 대기를 유지한다", async () => {
    const target = file("wait-target");
    const other = file("wait-other");
    const io = transport(target, other);
    const a = open(target, io);
    const b = open(other, io);
    const waiting = new Set([target.handle, other.handle]);
    const close = (handle: EditorFileHandle) => {
      waiting.delete(handle);
      (handle === target.handle ? a : b).close();
    };
    try {
      a.model.setValue("저장 대상");
      await a.session.save();
      await b.session.requestClose(async () => true, close);
      expect(waiting).toEqual(new Set([target.handle]));
      a.model.setValue("미저장");
      await a.session.requestClose(async () => false, close);
      expect(waiting.has(target.handle)).toBe(true);
      expect(a.text()).toBe("미저장");
      await a.session.requestClose(async () => true, close);
      expect(waiting.size).toBe(0);
      expect(a.model.isDisposed()).toBe(true);
    } finally {
      if (!a.model.isDisposed()) a.close();
      if (!b.model.isDisposed()) b.close();
    }
  });
});
