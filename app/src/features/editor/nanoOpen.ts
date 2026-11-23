import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { sftpSession } from "../sftp/sftpIpc";
import type { ApprovedEditorFile, EditorFileHandle, EditorTransport } from "./contract";

type NanoOffer = { readonly offerId: string; readonly sessionId: string };
export type RemoteNanoFile = ApprovedEditorFile & NanoOffer & {
  readonly transport: EditorTransport;
  readonly close: () => Promise<void>;
  readonly disconnect: (message: string) => void;
};

export type NanoMountResult =
  | { readonly kind: "mounted" }
  | { readonly kind: "aborted" }
  | { readonly kind: "unavailable" }
  // 백엔드가 응답 유실 전에 수락했을 수 있으므로 같은 탭과 핸들을 유지한다.
  | { readonly kind: "unconfirmed"; readonly error: unknown };

type AttachRemoteNanoFile = (
  file: RemoteNanoFile,
  acknowledgeMounted: () => Promise<NanoMountResult>,
  signal: AbortSignal,
) => Promise<void>;

function isOpaqueId(value: unknown): value is string {
  return typeof value === "string" && value.length === 32 && !/[^0-9a-f]/.test(value);
}

function parseOffer(value: unknown): NanoOffer | undefined {
  if (typeof value !== "object" || !value || Reflect.ownKeys(value).length !== 2) return;
  const offerId: unknown = Object.getOwnPropertyDescriptor(value, "offerId")?.value;
  const sessionId: unknown = Object.getOwnPropertyDescriptor(value, "sessionId")?.value;
  if (isOpaqueId(offerId) && typeof sessionId === "string" && sessionId.length > 0 &&
    sessionId.length <= 20 && !/[^0-9]/.test(sessionId)) {
    return { offerId, sessionId };
  }
}

export function subscribeRemoteNanoOpen(
  attach: AttachRemoteNanoFile,
  onListenError?: (error: unknown) => void,
): () => void {
  const lifetime = new AbortController();
  const { signal } = lifetime;
  const seen = new Set<string>();
  const claimed = new Map<string, { sessionId: string; disconnect: RemoteNanoFile["disconnect"] }>();

  async function claim({ offerId, sessionId }: NanoOffer) {
    let failed = false;
    let acknowledgment: Promise<NanoMountResult> | undefined;
    const sftp = sftpSession(sessionId, { trackDisconnect: true });
    claimed.set(offerId, { sessionId, disconnect: sftp.disconnect });
    function fail() {
      if (failed) return;
      failed = true;
      void invoke<unknown>("editor_remote_nano_failed", { offerId }).catch(() => {});
    }
    function abortBeforeMount() {
      if (!acknowledgment) fail();
    }

    try {
      const opened = await invoke<unknown>("editor_remote_nano_claim", { offerId });
      signal.addEventListener("abort", abortBeforeMount, { once: true });
      if (signal.aborted || typeof opened !== "object" || !opened || Reflect.ownKeys(opened).length !== 3) return;
      const rawHandle: unknown = Object.getOwnPropertyDescriptor(opened, "handle")?.value;
      const label: unknown = Object.getOwnPropertyDescriptor(opened, "name")?.value;
      const text: unknown = Object.getOwnPropertyDescriptor(opened, "text")?.value;
      if (!isOpaqueId(rawHandle) || rawHandle === offerId || typeof label !== "string" || typeof text !== "string") return;

      const handle = rawHandle as EditorFileHandle;
      await attach({ offerId, sessionId, handle, label, text, transport: sftp.transport, disconnect: sftp.disconnect,
        close: () => {
          claimed.delete(offerId);
          return sftp.close(handle);
        } }, () => {
        if (acknowledgment) return acknowledgment;
        if (signal.aborted || failed) return Promise.resolve({ kind: "aborted" });
        acknowledgment = invoke<void>("editor_remote_nano_mounted", { offerId, handle }).then(
          (): NanoMountResult => ({ kind: "mounted" }),
          async (error: unknown): Promise<NanoMountResult> => {
            const status = await invoke<unknown>("editor_remote_nano_failed", { offerId, handle }).catch(() => undefined);
            if (status === "unavailable") claimed.delete(offerId);
            if (status === "mounted" || status === "unavailable") return { kind: status };
            return { kind: "unconfirmed", error };
          },
        );
        return acknowledgment;
      }, signal);
    } finally {
      signal.removeEventListener("abort", abortBeforeMount);
      abortBeforeMount();
      if (!acknowledgment) claimed.delete(offerId);
    }
  }

  function receive(payload: unknown) {
    if (signal.aborted) return;
    const offer = parseOffer(payload);
    if (!offer || seen.has(offer.offerId)) return;
    seen.add(offer.offerId);
    void claim(offer).catch(() => {});
  }

  const target = { kind: "Webview", label: "main" } as const;
  const disconnectRegistration = (async () => {
    if (!isTauri() || getCurrentWebview().label !== "main") return;
    return listen<unknown>("editor-remote-nano-disconnected", ({ payload }) => {
      if (signal.aborted) return;
      const offer = parseOffer(payload);
      if (!offer) return;
      const file = claimed.get(offer.offerId);
      if (file?.sessionId !== offer.sessionId) return;
      claimed.delete(offer.offerId);
      file.disconnect("원격 nano 연결이 종료되었습니다");
    }, { target });
  })();
  const openRegistration = disconnectRegistration.then(unlisten => {
    if (!unlisten || signal.aborted) return;
    return listen<unknown>("editor-remote-nano-open", ({ payload }) => receive(payload), { target });
  });
  void openRegistration.then(async (unlisten) => {
    if (!unlisten || signal.aborted) return;
    const pending = await invoke<unknown>("editor_remote_nano_pending");
    if (Array.isArray(pending)) pending.forEach(receive);
  }).catch((error: unknown) => {
    if (!signal.aborted) onListenError?.(error);
  });

  return () => {
    if (signal.aborted) return;
    lifetime.abort();
    seen.clear();
    claimed.clear();
    for (const registration of [disconnectRegistration, openRegistration]) {
      void registration.then((unlisten) => unlisten?.()).catch(() => {});
    }
  };
}
