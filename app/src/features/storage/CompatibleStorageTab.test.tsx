// @vitest-environment jsdom
import { act, createRef, type RefObject } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { CompatibleStorageTab } from "./CompatibleStorageTab";
import type { CompatibleSession, CompatibleSettingsView } from "./compatible-settings";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const ipc = vi.mocked(invoke);
const settings: CompatibleSettingsView = {
  serviceId: "fixture", alias: "호환 설정", origin: "https://storage.example.test", region: "fixture-1", addressing: "path",
};
let host: HTMLDivElement;
let root: Root;
let closeRef: RefObject<{ confirmClose(): Promise<void> } | null>;
const fields = () => Array.from(host.querySelectorAll("input"));
const fill = (index: number, value: string) => act(async () => {
  const field = fields()[index];
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(field, value);
  field.dispatchEvent(new Event("input", { bubbles: true }));
});
const click = (label: string) => act(async () => {
  const button = Array.from(host.querySelectorAll("button")).find(button => button.textContent === label);
  if (!button) throw new Error(`버튼 없음: ${label}`);
  button.click();
});

function sessionIdFrom(payload: Parameters<typeof invoke>[1]) {
  const request = payload && "request" in payload ? payload.request : undefined;
  if (!request || typeof request !== "object" || !("sessionId" in request) || typeof request.sessionId !== "string") {
    throw new Error("연결 세션 ID가 없습니다");
  }
  return request.sessionId;
}

beforeEach(async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
  closeRef = createRef();
  await act(async () => root.render(<CompatibleStorageTab closing={false} closeRef={closeRef} />));
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.unstubAllGlobals();
  ipc.mockReset();
});

it("출처 변경 시 키링 참조를 비우고, 종료 후 도착한 연결 성공은 무시한다", async () => {
  const pending = Promise.withResolvers<{ ok: true; value: CompatibleSession }>();
  ipc.mockReturnValueOnce(pending.promise).mockResolvedValueOnce(true);
  await fill(0, settings.serviceId);
  await fill(1, settings.alias);
  await fill(2, "https://old.example.test");
  await fill(3, settings.region);
  await fill(4, "old-reference");
  await fill(2, settings.origin);
  expect(fields()[4].value).toBe("");
  await fill(4, "new-reference");
  await click("설정 저장 및 연결");
  const sessionId = sessionIdFrom(ipc.mock.calls[0][1]);
  const { origin, ...fieldsSent } = settings;
  expect(ipc).toHaveBeenNthCalledWith(1, "storage_compatible_open", {
    request: { sessionId, target: { kind: "select", itemId: "new-reference", settings: { ...fieldsSent, endpoint: origin } } },
  });
  await act(async () => {
    await Promise.all([closeRef.current!.confirmClose(), closeRef.current!.confirmClose()]);
    pending.resolve({ ok: true, value: { sessionId, target: settings } });
  });
  expect(ipc).toHaveBeenLastCalledWith("storage_close", { request: { sessionId } });
  expect(ipc).toHaveBeenCalledTimes(2);
  expect(host.textContent).not.toContain("버킷 목록");
});

it("저장된 설정은 키링 참조 없이 열고 동일 세션으로 버킷 목록, 객체 목록, 다운로드를 요청한다", async () => {
  let sessionId: string | undefined;
  ipc.mockImplementation(async (command, payload) => {
    switch (command) {
      case "storage_compatible_load": return { ok: true, value: settings };
      case "storage_compatible_open":
        sessionId = sessionIdFrom(payload);
        return { ok: true, value: { sessionId, target: settings } };
      case "storage_list_buckets": return { ok: true, value: { rows: [{ name: "fixture-bucket", region: settings.region }], nextToken: null } };
      case "storage_list_objects": return { ok: true, value: { rows: [{ kind: "object", key: "한글.txt", bytes: 3 }], nextToken: null } };
      case "storage_select_local_file": return { kind: "approved", approvalId: "fixture-approval" };
      case "storage_download": return { ok: true, value: { bytes: 3, integrity: "lengthOnly" } };
      case "storage_close": return true;
      default: throw new Error(`예상하지 않은 요청: ${command}`);
    }
  });
  await fill(0, settings.serviceId);
  await click("저장된 설정 불러오기");
  expect(fields().map(field => field.value)).toEqual([settings.serviceId, settings.alias, settings.origin, settings.region]);
  await click("호환 스토리지 연결");
  expect(host.textContent).toContain(settings.origin);
  expect(host.textContent).not.toContain("SSO");
  expect(ipc).toHaveBeenNthCalledWith(2, "storage_compatible_open", { request: { sessionId, target: { kind: "saved", expected: settings } } });
  await click("버킷 목록");
  await click("fixture-bucket");
  await act(async () => host.querySelector<HTMLButtonElement>('button[title="한글.txt"]')?.click());
  await click("다운로드");
  expect(host.textContent).toContain("다운로드 완료: 3 바이트");
  const target = { sessionId, bucket: "fixture-bucket", region: settings.region, key: "한글.txt" };
  expect(ipc).toHaveBeenNthCalledWith(3, "storage_list_buckets", { request: { sessionId, token: null } });
  expect(ipc).toHaveBeenNthCalledWith(4, "storage_list_objects", { request: { sessionId, token: null, bucket: target.bucket, region: target.region, prefix: "" } });
  expect(ipc).toHaveBeenNthCalledWith(5, "storage_select_local_file", { request: { ...target, direction: "download" } });
  expect(ipc).toHaveBeenNthCalledWith(6, "storage_download", { request: { ...target, approvalId: "fixture-approval" } });
  await act(async () => closeRef.current!.confirmClose());
  expect(ipc).toHaveBeenNthCalledWith(7, "storage_close", { request: { sessionId } });
  expect(ipc).toHaveBeenCalledTimes(7);
});

it("종료 응답이 유실되면 같은 세션 ID로 다시 종료를 확인한다", async () => {
  const pending = Promise.withResolvers<{ ok: false; error: "cancelled" }>();
  ipc.mockResolvedValueOnce({ ok: true, value: settings })
    .mockReturnValueOnce(pending.promise)
    .mockRejectedValueOnce(new Error("종료 응답 유실"))
    .mockResolvedValueOnce(false);
  await fill(0, settings.serviceId);
  await click("저장된 설정 불러오기");
  await click("호환 스토리지 연결");
  const sessionId = sessionIdFrom(ipc.mock.calls[1][1]);
  await act(async () => {
    await expect(closeRef.current!.confirmClose()).rejects.toThrow("종료 응답 유실");
    pending.resolve({ ok: false, error: "cancelled" });
  });
  expect(host.querySelector('[role="alert"]')).toBeNull();
  await act(async () => closeRef.current!.confirmClose());
  expect(ipc).toHaveBeenNthCalledWith(3, "storage_close", { request: { sessionId } });
  expect(ipc).toHaveBeenNthCalledWith(4, "storage_close", { request: { sessionId } });
  expect(ipc).toHaveBeenCalledTimes(4);
});
