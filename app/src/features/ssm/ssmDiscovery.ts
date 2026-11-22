import { invoke } from "@tauri-apps/api/core";
import type { SsmDiscovery, SsmOnlineNode, SsmResult, SsmScope } from "./ssm";

const documentEvent = "zzabba:ssm-document";

declare global {
  interface Window {
    __ZZABBA_SSM_DOCUMENT__?: string;
  }
}

type Operation = { kind: "listProfiles" }
  | { kind: "configuredRegion"; profile: string }
  | ({ kind: "listOnlineNodes" } & SsmScope);

type Request = { action: "reserve" }
  | { action: "run"; requestId: string; operation: Operation }
  | { action: "relogin" | "cancel"; requestId: string };

type ExpiredLookup = { document: string; requestId: string; detach: () => void };

const unknown = { ok: false, error: "Unknown" } as const;

function failure(error: unknown, allowsRelogin: boolean): SsmResult<never> {
  const code = error && typeof error === "object" && "code" in error ? error.code : null;
  if (code === "AccessDenied" || code === "PluginMissing" || (allowsRelogin && code === "SsoLoginRequired")) {
    return { ok: false, error: code };
  }
  return unknown;
}

function waitForDocument(signal: AbortSignal): Promise<string | undefined> {
  if (signal.aborted || typeof window === "undefined") return Promise.resolve(undefined);
  const current = window.__ZZABBA_SSM_DOCUMENT__;
  if (typeof current === "string" && current) return Promise.resolve(current);

  const { promise, resolve } = Promise.withResolvers<string | undefined>();
  function finish(value?: string) {
    window.removeEventListener(documentEvent, onDocument);
    signal.removeEventListener("abort", onAbort);
    resolve(value);
  }
  function onDocument() {
    const value = window.__ZZABBA_SSM_DOCUMENT__;
    if (typeof value === "string" && value) finish(value);
  }
  function onAbort() { finish(); }
  window.addEventListener(documentEvent, onDocument);
  signal.addEventListener("abort", onAbort, { once: true });
  return promise;
}

function send(document: string, request: Request) {
  return invoke<unknown>("ssm_discovery", { document, request });
}

function cancelOnAbort(document: string, requestId: string, signal: AbortSignal) {
  const cancel = () => { void send(document, { action: "cancel", requestId }).catch(() => undefined); };
  if (signal.aborted) cancel();
  else signal.addEventListener("abort", cancel, { once: true });
  return () => signal.removeEventListener("abort", cancel);
}

export function createSsmDiscovery(): SsmDiscovery {
  // 동일한 값을 가진 새 범위가 폐기된 조회의 재로그인 권한을 가져가지 못하게 한다.
  const expired = new WeakMap<SsmScope, ExpiredLookup>();

  async function run<T>(operation: Operation, signal: AbortSignal, isValue: (value: unknown) => value is T,
    scope?: SsmScope): Promise<SsmResult<T>> {
    const document = await waitForDocument(signal);
    if (!document || signal.aborted) return unknown;
    let detach: (() => void) | undefined;
    try {
      const requestId = await send(document, { action: "reserve" });
      if (typeof requestId !== "string" || !requestId) return unknown;
      detach = cancelOnAbort(document, requestId, signal);
      if (signal.aborted) return unknown;
      try {
        const value = await send(document, { action: "run", requestId, operation });
        return !signal.aborted && isValue(value) ? { ok: true, value } : unknown;
      } catch (error) {
        const result = failure(error, scope !== undefined && !signal.aborted);
        if (scope && !result.ok && result.error === "SsoLoginRequired") {
          const detachCancel = detach;
          const forget = () => {
            if (expired.get(scope)?.requestId === requestId) expired.delete(scope);
          };
          signal.addEventListener("abort", forget, { once: true });
          expired.set(scope, {
            document, requestId, detach: () => {
              detachCancel();
              signal.removeEventListener("abort", forget);
            }
          });
          detach = undefined;
        }
        return signal.aborted ? unknown : result;
      }
    } catch (error) {
      return signal.aborted ? unknown : failure(error, false);
    } finally {
      detach?.();
    }
  }

  return {
    listProfiles: (signal) => run({ kind: "listProfiles" }, signal, (value): value is string[] =>
      Array.isArray(value) && value.every((profile) => typeof profile === "string" && profile.length > 0)),
    configuredRegion: (profile, signal) => run({ kind: "configuredRegion", profile }, signal,
      (value): value is string | null => value === null || (typeof value === "string" && value.length > 0)),
    listOnlineNodes: (scope, signal) => run({ kind: "listOnlineNodes", profile: scope.profile, region: scope.region }, signal,
      (value): value is SsmOnlineNode[] => Array.isArray(value) && value.every((node: unknown) =>
        !!node && typeof node === "object" && "instanceId" in node && typeof node.instanceId === "string"
        && node.instanceId.length > 0 && (!("name" in node) || typeof node.name === "string")), scope),
    async relogin(scope, cause, signal) {
      const lookup = expired.get(scope);
      if (cause !== "SsoLoginRequired" || !lookup || signal.aborted) return unknown;
      expired.delete(scope);
      lookup.detach();
      const { document, requestId } = lookup;
      const detach = cancelOnAbort(document, requestId, signal);
      try {
        const value = await send(document, { action: "relogin", requestId });
        return !signal.aborted && value === null ? { ok: true, value: undefined } : unknown;
      } catch (error) {
        return signal.aborted ? unknown : failure(error, false);
      } finally {
        detach();
      }
    },
  };
}
