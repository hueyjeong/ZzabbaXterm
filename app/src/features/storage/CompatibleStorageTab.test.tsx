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
const saved = { ...settings, revision: "fixture-revision" };
const { origin, ...fieldsSent } = settings;
const input = { ...fieldsSent, endpoint: origin };
const firstPage = { rows: [{ name: "fixture-bucket", region: settings.region }], nextToken: "page-2" };
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
  vi.useRealTimers();
});

async function fillSettings() {
  await fill(0, settings.serviceId);
  await fill(1, settings.alias);
  await fill(2, settings.origin);
  await fill(3, settings.region);
}

it("키 등록은 비밀 없는 설정만 전송하고 키링 ID를 채운 뒤 별도 연결을 기다린다", async () => {
  ipc.mockResolvedValueOnce({ ok: true, value: { itemId: "new-key" } })
    .mockResolvedValueOnce({ ok: false, error: "key_missing" });
  await fillSettings();
  await click("키 등록");
  const enrollmentId = sessionIdFrom(ipc.mock.calls[0][1]);
  expect(ipc).toHaveBeenCalledExactlyOnceWith("storage_compatible_enroll", { request: { sessionId: enrollmentId, settings: input } });
  expect(fields()[4].value).toBe("new-key");
  await act(async () => closeRef.current!.confirmClose());
  expect(ipc).toHaveBeenCalledTimes(1);

  await click("설정 저장 및 연결");
  const sessionId = sessionIdFrom(ipc.mock.calls[1][1]);
  expect(sessionId).not.toBe(enrollmentId);
  expect(ipc).toHaveBeenNthCalledWith(2, "storage_compatible_open", {
    request: { sessionId, target: { kind: "select", settings: input, itemId: "new-key" } },
  });
});

it("등록 결과를 확인할 수 없으면 항목 ID와 오류를 표시한다", async () => {
  ipc.mockResolvedValueOnce({ ok: false, error: { outcomeUnknown: { itemId: "uncertain-key" } } });
  await fillSettings();
  await click("키 등록");
  expect(fields()[4].value).toBe("uncertain-key");
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("키링 등록 결과 확인 불가");
  expect(ipc).toHaveBeenCalledTimes(1);
});

it("등록 중 종료하면 같은 ID로 취소하고 늦은 등록 결과를 무시한다", async () => {
  const pending = Promise.withResolvers<{ ok: true; value: { itemId: string } }>();
  ipc.mockReturnValueOnce(pending.promise).mockResolvedValueOnce(true);
  await fillSettings();
  await click("키 등록");
  const sessionId = sessionIdFrom(ipc.mock.calls[0][1]);
  await act(async () => {
    await closeRef.current!.confirmClose();
    pending.resolve({ ok: true, value: { itemId: "late-key" } });
  });
  expect(ipc).toHaveBeenNthCalledWith(2, "storage_close", { request: { sessionId } });
  expect(ipc).toHaveBeenCalledTimes(2);
  expect(fields()[4].value).toBe("");
});

it.each([
  { reply: { ok: true, value: { itemId: "saved-key" } }, itemId: "saved-key", message: undefined, replyFirst: false },
  { reply: { ok: false, error: { outcomeUnknown: { itemId: "uncertain-key" } } }, itemId: "uncertain-key", message: "키링 등록 결과 확인 불가", replyFirst: true },
])("취소할 수 없는 등록은 $itemId 결과가 도착할 때까지 닫기를 보류한다", async ({ reply, itemId, message, replyFirst }) => {
  const registration = Promise.withResolvers<typeof reply>();
  const cancellation = Promise.withResolvers<boolean>();
  ipc.mockReturnValueOnce(registration.promise).mockReturnValueOnce(cancellation.promise);
  await fillSettings();
  await click("키 등록");

  let close: Promise<void> | undefined;
  let settled = false;
  await act(async () => {
    close = expect(closeRef.current!.confirmClose()).rejects.toThrow("키링 등록 결과 확인 필요").then(() => { settled = true; });
    if (replyFirst) registration.resolve(reply);
    else cancellation.resolve(false);
  });
  expect(settled).toBe(false);
  expect(fields()[4].value).toBe("");

  await act(async () => {
    if (replyFirst) cancellation.resolve(false);
    else registration.resolve(reply);
    await close;
  });
  expect(fields()[4].value).toBe(itemId);
  expect(host.querySelector('[role="alert"]')?.textContent).toBe(message);
  await act(async () => closeRef.current!.confirmClose());
  expect(ipc).toHaveBeenCalledTimes(2);
});

it.each(["만료", "응답 유실"])("취소할 수 없는 등록의 %s 후 결과 확인 불가를 표시하고 재등록하지 않는다", async failure => {
  vi.useFakeTimers();
  const registration = Promise.withResolvers<unknown>();
  ipc.mockReturnValueOnce(registration.promise).mockResolvedValueOnce(false);
  await fillSettings();
  await click("키 등록");

  await act(async () => {
    const close = expect(closeRef.current!.confirmClose()).rejects.toThrow("키링 등록 결과 확인 필요");
    if (failure === "만료") {
      await vi.advanceTimersByTimeAsync(35_000);
      registration.resolve({ ok: true, value: { itemId: "expired-key" } });
    } else registration.reject(new Error("등록 응답 유실"));
    await close;
  });
  expect(fields()[4].value).toBe("");
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("키링 등록 결과 확인 불가");
  await act(async () => closeRef.current!.confirmClose());
  expect(ipc).toHaveBeenCalledTimes(2);
});

it("등록 IPC가 거부되면 자동 재등록 없이 등록 결과 확인 불가를 표시한다", async () => {
  ipc.mockRejectedValueOnce(new Error("등록 응답 유실"));
  await fillSettings();
  await click("키 등록");
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("키링 등록 결과 확인 불가");
  expect(ipc).toHaveBeenCalledTimes(1);
});

it("동일 출처의 설정 수정은 저장된 전체 스냅샷을 보내고 실제 접속 실패를 표시한다", async () => {
  ipc.mockResolvedValueOnce({ ok: true, value: saved }).mockResolvedValueOnce({
    ok: false, error: { connection: { stage: "listBuckets", failure: { kind: "service", status: 403, code: "AccessDenied" }, bytes: 0 } },
  });
  await fill(0, settings.serviceId);
  await click("저장된 설정 불러오기");
  await fill(1, "수정한 별칭");
  const endpoint = "https://STORAGE.example.test:443/";
  await fill(2, endpoint);
  expect(fields()).toHaveLength(4);
  await click("호환 스토리지 연결");
  const sessionId = sessionIdFrom(ipc.mock.calls[1][1]);
  expect(ipc).toHaveBeenNthCalledWith(2, "storage_compatible_open", {
    request: { sessionId, target: { kind: "revise", expected: saved, settings: { ...input, alias: "수정한 별칭", endpoint } } },
  });
  expect(host.querySelector('[role="alert"]')?.textContent).toContain("버킷 목록: 권한 거부 (403 AccessDenied)");
  await fill(2, "https://other.example.test");
  expect(fields()[4].value).toBe("");
});

it("서비스 ID가 바뀌면 같은 출처라도 항목을 다시 지정한다", async () => {
  ipc.mockResolvedValueOnce({ ok: true, value: saved }).mockResolvedValueOnce({ ok: false, error: "key_missing" });
  await fill(0, settings.serviceId);
  await click("저장된 설정 불러오기");
  await fill(0, "other-service");
  expect(fields()[4].value).toBe("");
  await fill(4, "other-service-key");
  await click("설정 저장 및 연결");
  const sessionId = sessionIdFrom(ipc.mock.calls[1][1]);
  expect(ipc).toHaveBeenNthCalledWith(2, "storage_compatible_open", {
    request: { sessionId, target: { kind: "select", settings: { ...input, serviceId: "other-service" }, itemId: "other-service-key" } },
  });
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
  expect(ipc).toHaveBeenNthCalledWith(1, "storage_compatible_open", {
    request: { sessionId, target: { kind: "select", itemId: "new-reference", settings: input } },
  });
  await act(async () => {
    await Promise.all([closeRef.current!.confirmClose(), closeRef.current!.confirmClose()]);
    pending.resolve({ ok: true, value: { sessionId, target: settings, initialBuckets: firstPage } });
  });
  expect(ipc).toHaveBeenLastCalledWith("storage_close", { request: { sessionId } });
  expect(ipc).toHaveBeenCalledTimes(2);
  expect(host.textContent).not.toContain("버킷 목록");
});

it("저장된 설정은 키링 참조 없이 열고 동일 세션으로 버킷 목록, 객체 목록, 다운로드를 요청한다", async () => {
  let sessionId: string | undefined;
  ipc.mockImplementation(async (command, payload) => {
    switch (command) {
      case "storage_compatible_load": return { ok: true, value: saved };
      case "storage_compatible_open":
        sessionId = sessionIdFrom(payload);
        return { ok: true, value: { sessionId, target: settings, initialBuckets: firstPage } };
      case "storage_list_buckets": return { ok: true, value: { rows: [{ name: "second-bucket", region: settings.region }], nextToken: null } };
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
  expect(ipc).toHaveBeenNthCalledWith(2, "storage_compatible_open", { request: { sessionId, target: { kind: "saved", expected: saved } } });
  expect(host.textContent).toContain("fixture-bucket");
  expect(ipc).toHaveBeenCalledTimes(2);
  await click("다음 페이지");
  expect(host.textContent).toContain("second-bucket");
  await click("fixture-bucket");
  await act(async () => host.querySelector<HTMLButtonElement>('button[title="한글.txt"]')?.click());
  await click("다운로드");
  expect(host.textContent).toContain("다운로드 완료: 3 바이트");
  const target = { sessionId, bucket: "fixture-bucket", region: settings.region, key: "한글.txt" };
  expect(ipc).toHaveBeenNthCalledWith(3, "storage_list_buckets", { request: { sessionId, token: "page-2" } });
  expect(ipc).toHaveBeenNthCalledWith(4, "storage_list_objects", { request: { sessionId, token: null, bucket: target.bucket, region: target.region, prefix: "" } });
  expect(ipc).toHaveBeenNthCalledWith(5, "storage_select_local_file", { request: { ...target, direction: "download" } });
  expect(ipc).toHaveBeenNthCalledWith(6, "storage_download", { request: { ...target, approvalId: "fixture-approval" } });
  await act(async () => closeRef.current!.confirmClose());
  expect(ipc).toHaveBeenNthCalledWith(7, "storage_close", { request: { sessionId } });
  expect(ipc).toHaveBeenCalledTimes(7);
});

it("종료 응답이 유실되면 같은 세션 ID로 다시 종료를 확인한다", async () => {
  const pending = Promise.withResolvers<{ ok: false; error: "cancelled" }>();
  ipc.mockResolvedValueOnce({ ok: true, value: saved })
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
