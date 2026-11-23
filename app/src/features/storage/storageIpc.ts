import { invoke } from "@tauri-apps/api/core";
import type { LocalFileActions, S3Actions, S3Result, S3Session } from "./s3";

export function openStorage(targetId: string, attemptId: string) {
  return invoke<S3Result<S3Session>>("storage_open", { request: { targetId, id: attemptId } });
}

export function reloginStorage(id: string) {
  return invoke<S3Result<null>>("storage_relogin", { request: { id } });
}

export function closeStorage(sessionId: string) {
  return invoke<boolean>("storage_close", { request: { sessionId } });
}

export function countPreviousFiles(request: Readonly<{ sessionId: string; bucket: string; region: string; prefix: string }>) {
  return invoke<S3Result<number>>("storage_count_previous_files", { request });
}

async function useApproval<T>(command: string, request: Parameters<S3Actions["upload"]>[0]) {
  try {
    return await invoke<S3Result<T>>(command, { request });
  } catch (error) {
    const { sessionId, approvalId } = request;
    await invoke("storage_cancel_approval", { request: { sessionId, approvalId } }).catch(() => { });
    throw error;
  }
}

export const storageActions: S3Actions = {
  listBuckets: request => invoke("storage_list_buckets", { request }),
  listObjects: request => invoke("storage_list_objects", { request }),
  selectLocalFile: request => invoke("storage_select_local_file", { request }),
  upload: request => useApproval("storage_upload", request),
  download: request => useApproval("storage_download", request),
};

export const localFileActions: LocalFileActions = {
  openRoot: request => invoke("storage_local_open_root", { request }),
  list: request => invoke("storage_local_list", { request }),
  select: request => invoke("storage_local_select", { request }),
};
