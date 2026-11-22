import { invoke } from "@tauri-apps/api/core";
import type { ApprovedEditorFile, EditorFileError, EditorFileHandle, EditorReadResult, EditorTransport } from "../editor/contract";
import type { FileEntry } from "../files/FileBrowser";

const errors = {
  documentClosed: "파일이 닫혔습니다",
  invalidHandle: "유효하지 않은 파일 핸들",
  invalidEntry: "목록에서 사라진 항목",
  unavailable: "SFTP 연결을 열 수 없습니다",
  disconnected: "SSH 연결이 끊겼습니다",
  timedOut: "SFTP 응답 시간 초과",
  notRegular: "일반 파일이 아닙니다",
  notUtf8: "UTF-8 파일이 아닙니다",
  limitExceeded: "SFTP 작업 크기 제한 초과",
  pathChanged: "원격 경로 변경",
  readChanged: "읽는 중 원격 파일 변경",
  missing: "원격 파일 없음",
  permissionDenied: "권한 거부",
  atomicReplaceUnavailable: "서버에서 원자적 교체를 지원하지 않습니다",
  reopenRequired: "원격 파일을 다시 읽어야 합니다",
  remoteFailure: "원격 작업 실패",
} as const;

type SftpError = { readonly kind: keyof typeof errors };
type Cleanup = { readonly kind: "removed" | "skippedDirectoryChanged" } | { readonly kind: "failed"; readonly error: SftpError };
const stages = {
  prepare: "저장 준비", create: "임시 파일 생성", write: "임시 파일 쓰기", permissions: "권한 적용",
  close: "임시 파일 닫기", recheck: "원격 파일 재확인", replace: "원격 파일 교체",
} as const;
const unreadable = {
  notRegular: errors.notRegular, notUtf8: errors.notUtf8, tooLarge: errors.limitExceeded,
  directoryChanged: errors.pathChanged, readChanged: errors.readChanged,
} as const;
type SaveResult = { readonly status: "saved" }
  | { readonly status: "unknown"; readonly error: SftpError }
  | { readonly status: "failed"; readonly stage: keyof typeof stages; readonly error: SftpError; readonly cleanup?: Cleanup | null }
  | { readonly status: "conflict"; readonly remote: { readonly kind: "text"; readonly text: string }
      | { readonly kind: "missing" | keyof typeof unreadable }; readonly cleanup: Cleanup };

type Listing = {
  readonly entries: readonly { readonly entryId: string; readonly name: string; readonly isDirectory: boolean }[];
  readonly parentEntryId: string | null;
};

type NamedEntry = FileEntry & { readonly name: string };

export type SftpListing = {
  readonly entries: readonly NamedEntry[];
  readonly parentEntryId: string | null;
};

export interface SftpSession {
  attach(): Promise<SftpListing>;
  list(entryId: string | null): Promise<SftpListing>;
  open(entryId: string): Promise<ApprovedEditorFile>;
  close(handle: EditorFileHandle): Promise<void>;
  detach(): Promise<void>;
  readonly transport: EditorTransport;
}

export function sftpError(error: unknown): EditorFileError {
  const kind = error && typeof error === "object" && "kind" in error ? error.kind : null;
  const message = typeof kind === "string" && Object.prototype.hasOwnProperty.call(errors, kind)
    ? errors[kind as keyof typeof errors] : error instanceof Error ? error.message : typeof error === "string" ? error : "SFTP 응답 수신 실패";
  return {
    kind: kind === "disconnected" ? "disconnected" : ["permissionDenied", "documentClosed", "invalidHandle"].includes(String(kind)) ? "denied" : "failed",
    message,
  };
}

function cleanupMessage(cleanup?: Cleanup | null) {
  if (cleanup?.kind === "failed") return ` · 임시 파일 정리 실패: ${sftpError(cleanup.error).message}`;
  if (cleanup?.kind === "skippedDirectoryChanged") return " · 원격 경로 변경으로 임시 파일 정리 미완료";
  return "";
}

function listing(result: Listing): SftpListing {
  const entries = result.entries.map<NamedEntry>(({ entryId, name, isDirectory }) => isDirectory
    ? { kind: "folder", key: entryId, name }
    : { kind: "file", key: entryId, name, bytes: null });
  return { entries, parentEntryId: result.parentEntryId };
}

export function sftpSession(sessionId: string): SftpSession {
  const args = { sessionId };
  const listeners = new Set<(message: string) => void>();
  let disconnect: string | undefined;
  async function read(command: "editor_sftp_compare" | "editor_sftp_reload", handle: EditorFileHandle): Promise<EditorReadResult> {
    if (disconnect) return { kind: "disconnected", message: disconnect };
    try {
      return { kind: "read", text: await invoke<string>(command, { ...args, handle }) };
    } catch (error) {
      return sftpError(error);
    }
  }
  const transport: EditorTransport = {
    compare: (handle) => read("editor_sftp_compare", handle),
    reload: (handle) => read("editor_sftp_reload", handle),
    subscribeDisconnect(_handle, notify) {
      if (disconnect) notify(disconnect);
      else listeners.add(notify);
      return () => { listeners.delete(notify); };
    },
    async save(handle, text) {
      if (disconnect) return { kind: "disconnected", message: disconnect };
      // IPC 응답이 없으면 교체 여부는 호출자가 미확인으로 처리한다.
      let result: SaveResult;
      try {
        result = await invoke<SaveResult>("editor_sftp_save", { ...args, handle, text });
      } catch (error) {
        if (error && typeof error === "object" && "kind" in error) return sftpError(error);
        return { kind: "unknown", message: sftpError(error).message };
      }
      switch (result.status) {
        case "saved": return { kind: "saved" };
        case "unknown": return { kind: "unknown", message: sftpError(result.error).message };
        case "failed": {
          const error = sftpError(result.error);
          return { ...error, message: `${stages[result.stage]}: ${error.message}${cleanupMessage(result.cleanup)}` };
        }
        case "conflict": {
          const { remote, cleanup } = result;
          const current = remote.kind === "text" ? remote.text : remote.kind === "missing" ? null : undefined;
          const message = remote.kind === "text" || remote.kind === "missing" ? "" : unreadable[remote.kind];
          const cleanupDetail = cleanupMessage(cleanup);
          return { kind: "conflict", current, message: cleanupDetail ? `${message || "원격 파일 변경"}${cleanupDetail}` : message || undefined };
        }
        default: return { kind: "unknown", message: "알 수 없는 SFTP 저장 결과" };
      }
    },
  };
  return {
    async attach() { return listing(await invoke<Listing>("editor_sftp_attach", args)); },
    async list(entryId: string | null) { return listing(await invoke<Listing>("editor_sftp_list", { ...args, entryId })); },
    async open(entryId: string): Promise<ApprovedEditorFile> {
      const { handle, name, text } = await invoke<{ handle: EditorFileHandle; name: string; text: string }>("editor_sftp_open", { ...args, entryId });
      return { handle, label: name, text };
    },
    async close(handle) {
      try {
        await invoke<void>("editor_sftp_close", { ...args, handle });
      } catch (error) {
        throw new Error(sftpError(error).message);
      }
    },
    async detach() {
      disconnect = "SFTP 세션을 닫는 중입니다";
      try {
        await invoke<void>("editor_sftp_detach", args);
        disconnect = "SFTP 세션이 종료되었습니다";
      } catch (error) {
        disconnect = `SFTP 세션 종료 확인 불가: ${sftpError(error).message}`;
        throw error;
      } finally {
        for (const notify of listeners) notify(disconnect);
      }
    },
    transport,
  };
}
