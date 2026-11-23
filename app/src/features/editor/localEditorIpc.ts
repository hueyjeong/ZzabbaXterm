import { invoke } from "@tauri-apps/api/core";
import type { ApprovedEditorFile, EditorFileError, EditorFileHandle, EditorReadResult, EditorTransport } from "./contract";

type Selection = { status: "cancelled" } | { status: "opened"; handle: EditorFileHandle; name: string; text: string };

export function localEditorError(error: unknown): EditorFileError {
  const details = error && typeof error === "object" ? error as { kind?: string; stage?: string; osCode?: number } : null;
  const message = [details?.kind ?? String(error), details?.stage, details?.osCode].filter((part) => part != null).join(" · ");
  return { kind: details?.kind === "invalidHandle" || details?.kind === "documentClosed" ? "denied" : "failed", message };
}

export async function openLocalFile(): Promise<ApprovedEditorFile | null> {
  const result = await invoke<Selection>("editor_local_open");
  return result.status === "cancelled" ? null : { handle: result.handle, label: result.name, text: result.text };
}

async function read(command: "editor_local_compare" | "editor_local_reload", handle: EditorFileHandle): Promise<EditorReadResult> {
  try {
    return { kind: "read", text: await invoke<string>(command, { handle }) };
  } catch (error) {
    if (error && typeof error === "object" && "kind" in error) return localEditorError(error);
    throw error;
  }
}

export const localEditorTransport: EditorTransport = {
  compare: (handle) => read("editor_local_compare", handle),
  reload: (handle) => read("editor_local_reload", handle),
  async save(handle, text) {
    try {
      await invoke("editor_local_save", { handle, text });
      return { kind: "saved" };
    } catch (error) {
      if (!error || typeof error !== "object" || !("kind" in error)) throw error;
      if (error.kind === "publicationUnconfirmed") {
        const path = "recoveryPath" in error ? error.recoveryPath : undefined;
        return { kind: "unknown", message: typeof path === "string" ? `복구 파일: ${path}` : error.kind };
      }
      if (error.kind === "conflict" && "conflict" in error) {
        const { reason, diskText } = error.conflict as { reason: string; diskText?: string | null };
        if (reason === "missing") return { kind: "conflict", current: null };
        if (typeof diskText === "string") return { kind: "conflict", current: diskText };
        return { kind: "conflict", message: reason };
      }
      return localEditorError(error);
    }
  },
};

export const closeLocalFile = (handle: EditorFileHandle) => invoke<void>("editor_local_close", { handle });
