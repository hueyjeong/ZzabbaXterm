import { convertFileSrc, invoke, type InvokeArgs } from "@tauri-apps/api/core";

export function documentProof(): string | undefined {
  if (window !== top) return;
  const proof = (window as Window & { __ZZABBA_VNC_DOCUMENT__?: { proof?: unknown } }).__ZZABBA_VNC_DOCUMENT__?.proof;
  return typeof proof === "string" && /^[a-f0-9]{32}$/.test(proof) ? proof : undefined;
}

export function invokeVnc<T = void>(name: string, args?: InvokeArgs, proof = documentProof()): Promise<T> {
  if (!proof || documentProof() !== proof) return Promise.reject("VNC 문서가 교체되었거나 아직 준비되지 않았습니다");
  return invoke<T>(name, args, { headers: { "x-zzabba-vnc-document": proof } });
}

export async function readPacket(proof: string | undefined, ticket: unknown, signal: AbortSignal): Promise<unknown> {
  if (!proof || documentProof() !== proof || typeof ticket !== "string" || !/^[a-f0-9]{32}$/.test(ticket)) throw new Error();
  const res = await fetch(convertFileSrc(`${proof}-${ticket}`, "vnc-packet"), { cache: "no-store", signal });
  if (!res.ok || signal.aborted || documentProof() !== proof) throw new Error();
  const result: unknown = res.headers.get("content-type") === "application/octet-stream" ? await res.arrayBuffer() : await res.json();
  if (signal.aborted || documentProof() !== proof) throw new Error();
  return result;
}
