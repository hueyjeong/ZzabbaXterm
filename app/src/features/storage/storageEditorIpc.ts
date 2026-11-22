import { invoke } from "@tauri-apps/api/core";
import type { ApprovedEditorFile, EditorFileError, EditorFileHandle, EditorReadResult, EditorSaveResult, EditorTransport } from "../editor/contract";

function ipcMessage(error: unknown): string {
  if (error && typeof error === "object" && "message" in error && typeof error.message === "string") return error.message;
  return typeof error === "string" ? error : "S3 편집 응답을 받지 못했습니다";
}

export function bindStorageEditor(sessionId: string) {
  const transport: EditorTransport = {
    compare: (handle) => invoke<EditorReadResult>("storage_edit_compare", { request: { sessionId, handle } }),
    reload: (handle) => invoke<EditorReadResult>("storage_edit_reload", { request: { sessionId, handle } }),
    async save(handle, text) {
      try {
        return await invoke<EditorSaveResult>("storage_edit_save", { request: { sessionId, handle, text } });
      } catch (error) {
        // Rust가 저장을 마친 뒤 응답만 유실됐을 수 있다.
        return { kind: "unknown", message: ipcMessage(error) };
      }
    },
  };
  return {
    async open(rowId: string): Promise<ApprovedEditorFile> {
      const result = await invoke<ApprovedEditorFile | EditorFileError>("storage_edit_open", { request: { sessionId, rowId } })
        .catch((error: unknown) => { throw new Error(ipcMessage(error)); });
      if ("kind" in result) throw new Error(result.message);
      return result;
    },
    transport,
    async close(handle: EditorFileHandle): Promise<void> {
      await invoke<boolean>("storage_edit_close", { request: { sessionId, handle } });
    },
  };
}
