import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import type { ApprovedEditorFile, EditorFileHandle } from "./contract";

type AttachLocalCliFile = (
  file: ApprovedEditorFile,
  respond: (result: { readonly attached: true }) => Promise<void>,
  signal: AbortSignal,
) => Promise<void>;

function isEditorId(value: unknown): value is string {
  return typeof value === "string" && value.length === 32 && /^[0-9a-f]+$/.test(value);
}

export function subscribeLocalCliOpen(
  attach: AttachLocalCliFile,
  onListenError?: (error: unknown) => void,
): () => void {
  const lifetime = new AbortController();
  const { signal } = lifetime;
  const requests = new Set<string>();

  async function claim(requestId: string) {
    requests.add(requestId);
    const opened = await invoke<unknown>("editor_local_cli_claim", { requestId }).catch(() => undefined);
    if (typeof opened !== "object" || !opened || !("handle" in opened) || !isEditorId(opened.handle) || opened.handle === requestId) {
      return;
    }
    const handle = opened.handle as EditorFileHandle;
    const reply = (attached: boolean) => invoke<void>("editor_local_cli_respond", { requestId, handle, attached });
    let declined = false;
    let response: Promise<void> | undefined;
    function decline() {
      declined = true;
      return reply(false).catch(() => {});
    }
    function cancel() {
      response ??= decline();
    }

    signal.addEventListener("abort", cancel, { once: true });
    try {
      if (signal.aborted || !("status" in opened) || opened.status !== "opened" ||
        !("name" in opened) || typeof opened.name !== "string" || !("text" in opened) || typeof opened.text !== "string") return;
      await attach({ handle, label: opened.name, text: opened.text }, ({ attached }) => {
        if (signal.aborted || declined || attached !== true) return Promise.reject(new Error("편집 탭 부착 요청이 종료되었습니다"));
        if (response) return response;
        const accepted: Promise<void> = reply(true);
        response = accepted;
        void accepted.catch(() => { void decline(); });
        return accepted;
      }, signal);
    } catch {
      // 부착 실패는 같은 요청과 핸들의 거절 응답으로 정리한다.
    } finally {
      signal.removeEventListener("abort", cancel);
      cancel();
    }
  }

  function receive(requestId: unknown) {
    if (!signal.aborted && isEditorId(requestId) && !requests.has(requestId)) void claim(requestId);
  }

  const registration = (async () => {
    if (!isTauri() || getCurrentWebview().label !== "main") return;
    return listen<unknown>("editor-local-cli-open", ({ payload }) => {
      if (signal.aborted || typeof payload !== "object" || !payload || Reflect.ownKeys(payload).length !== 1) return;
      receive(Object.getOwnPropertyDescriptor(payload, "requestId")?.value);
    }, { target: { kind: "Webview", label: "main" } });
  })();
  void registration.then(async (unlisten) => {
    if (!unlisten || signal.aborted) return;
    // 구독이 설치되기 전에 발행된 요청도 같은 경로로 전달한다.
    const pending = await invoke<unknown>("editor_local_cli_pending");
    if (Array.isArray(pending)) pending.forEach(receive);
  }).catch((error: unknown) => {
    if (!signal.aborted) onListenError?.(error);
  });

  return () => {
    if (signal.aborted) return;
    lifetime.abort();
    requests.clear();
    void registration.then((unlisten) => unlisten?.()).catch(() => {});
  };
}
