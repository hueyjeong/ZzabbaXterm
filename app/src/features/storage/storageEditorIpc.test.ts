import { invoke } from "@tauri-apps/api/core";
import { beforeEach, expect, it, vi } from "vitest";
import { bindStorageEditor } from "./storageEditorIpc";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
const ipc = vi.mocked(invoke);

beforeEach(() => { ipc.mockReset(); });

it("목록 행 ID로만 열고 같은 세션에서 Rust 편집 핸들로 작업한다", async () => {
  const editor = bindStorageEditor("session-1");
  const file = { handle: "edit-1", label: "설정/한글.txt", text: "본문" };
  const read = { kind: "read", text: "현재 원본" };
  const conflict = { kind: "conflict", message: "S3 조건부 저장 충돌 (HTTP 412)" };
  ipc.mockResolvedValueOnce(file).mockResolvedValueOnce(conflict)
    .mockResolvedValueOnce(read).mockResolvedValueOnce(read).mockResolvedValueOnce(false);

  const approved = await editor.open("row-1");
  expect(approved).toEqual(file);
  await expect(editor.transport.save(approved.handle, "초안")).resolves.toStrictEqual(conflict);
  await expect(editor.transport.compare(approved.handle)).resolves.toEqual(read);
  await expect(editor.transport.reload(approved.handle)).resolves.toEqual(read);
  await expect(editor.close(approved.handle)).resolves.toBeUndefined();

  expect(ipc.mock.calls).toEqual([
    ["storage_edit_open", { request: { sessionId: "session-1", rowId: "row-1" } }],
    ["storage_edit_save", { request: { sessionId: "session-1", handle: "edit-1", text: "초안" } }],
    ["storage_edit_compare", { request: { sessionId: "session-1", handle: "edit-1" } }],
    ["storage_edit_reload", { request: { sessionId: "session-1", handle: "edit-1" } }],
    ["storage_edit_close", { request: { sessionId: "session-1", handle: "edit-1" } }],
  ]);
});

it("Rust 열기 오류를 전달하고, 저장 응답이 거부되면 재전송 없이 미확인으로 반환한다", async () => {
  const editor = bindStorageEditor("session-2");
  ipc.mockResolvedValueOnce({ kind: "denied", message: "편집할 객체 또는 핸들을 확인할 수 없습니다" });
  await expect(editor.open("stale-row")).rejects.toThrow("편집할 객체 또는 핸들을 확인할 수 없습니다");

  ipc.mockResolvedValueOnce({ handle: "edit-2", label: "메모.txt", text: "본문" });
  const { handle } = await editor.open("row-2");
  ipc.mockRejectedValueOnce({ kind: "disconnected", message: "IPC 응답 소실" });
  await expect(editor.transport.save(handle, "초안")).resolves.toEqual({ kind: "unknown", message: "IPC 응답 소실" });
  expect(ipc.mock.calls.slice(2)).toEqual([
    ["storage_edit_save", { request: { sessionId: "session-2", handle: "edit-2", text: "초안" } }],
  ]);
});
