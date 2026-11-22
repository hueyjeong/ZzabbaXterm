import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWebview, type Webview } from "@tauri-apps/api/webview";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { subscribeLocalCliOpen } from "./cliOpen";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), isTauri: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("@tauri-apps/api/webview", () => ({ getCurrentWebview: vi.fn() }));

const event = "editor-local-cli-open";
const requestId = "1".repeat(32);
const handle = "2".repeat(32);
const opened = { status: "opened", handle, name: "한글.txt", text: "본문" };
const ipc = vi.mocked(invoke);
const register = vi.mocked(listen);
const unlisten = vi.fn();
let unsubscribe: (() => void) | undefined;

function subscribe(...args: Parameters<typeof subscribeLocalCliOpen>) {
  unsubscribe = subscribeLocalCliOpen(...args);
  return unsubscribe;
}

function deliver(payload: unknown = { requestId }) {
  register.mock.calls[0][1]({ event, id: 1, payload });
}

beforeEach(() => {
  vi.resetAllMocks();
  ipc.mockResolvedValue(undefined);
  vi.mocked(isTauri).mockReturnValue(true);
  vi.mocked(getCurrentWebview).mockReturnValue({ label: "main" } as Webview);
  register.mockResolvedValue(unlisten);
});

afterEach(() => {
  unsubscribe?.();
  unsubscribe = undefined;
});

it("현재 메인 웹뷰만 구독하고 실제 부착 응답 전에는 승인하지 않는다", async () => {
  const mounted = Promise.withResolvers<void>();
  const attached = Promise.withResolvers<void>();
  const attach = vi.fn<Parameters<typeof subscribeLocalCliOpen>[0]>(async (_file, respond) => {
    mounted.resolve();
    await attached.promise;
    await Promise.all([respond({ attached: true }), respond({ attached: true })]);
  });
  ipc.mockResolvedValueOnce(opened).mockResolvedValueOnce(undefined);
  subscribe(attach);
  expect(register).toHaveBeenCalledWith(event, expect.any(Function), { target: { kind: "Webview", label: "main" } });

  deliver();
  deliver();
  await mounted.promise;
  expect(ipc.mock.calls).toEqual([["editor_local_cli_claim", { requestId }]]);
  expect(attach).toHaveBeenCalledExactlyOnceWith({ handle, label: "한글.txt", text: "본문" }, expect.any(Function), expect.any(AbortSignal));
  attached.resolve();
  await attach.mock.results[0].value;
  deliver();
  expect(ipc.mock.calls).toEqual([
    ["editor_local_cli_claim", { requestId }],
    ["editor_local_cli_respond", { requestId, handle, attached: true }],
  ]);
});

it("동시 요청의 응답은 각각 클레임한 핸들과 연결된다", async () => {
  const otherId = "3".repeat(32);
  const otherHandle = "4".repeat(32);
  const first = Promise.withResolvers<unknown>();
  const attach = vi.fn<Parameters<typeof subscribeLocalCliOpen>[0]>(async (file, respond) => {
    if (file.handle === otherHandle) await respond({ attached: true });
  });
  ipc.mockReturnValueOnce(first.promise).mockResolvedValueOnce({ ...opened, handle: otherHandle });
  subscribe(attach);
  deliver();
  deliver({ requestId: otherId });
  await vi.waitFor(() => expect(attach).toHaveBeenCalledTimes(1));
  first.resolve(opened);
  await vi.waitFor(() => expect(ipc.mock.calls).toEqual([
    ["editor_local_cli_claim", { requestId }],
    ["editor_local_cli_claim", { requestId: otherId }],
    ["editor_local_cli_respond", { requestId: otherId, handle: otherHandle, attached: true }],
    ["editor_local_cli_respond", { requestId, handle, attached: false }],
  ]));
});

it("브라우저와 다른 웹뷰에서는 구독하지 않는다", () => {
  const attach = vi.fn();
  vi.mocked(isTauri).mockReturnValueOnce(false);
  vi.mocked(getCurrentWebview).mockReturnValueOnce({ label: "other" } as Webview);
  subscribeLocalCliOpen(attach)();
  subscribeLocalCliOpen(attach)();
  expect(getCurrentWebview).toHaveBeenCalledTimes(1);
  expect(register).not.toHaveBeenCalled();
  expect(ipc).not.toHaveBeenCalled();
});

it("정확한 단일 요청 ID 필드만 받는다", () => {
  subscribe(vi.fn());
  for (const payload of [null, requestId, [requestId], {}, { requestId: 1 }, { requestId: "" },
    { requestId: "A".repeat(32) }, { requestId: `${requestId}\n` }, { requestId, path: "한글.txt" },
    Object.create({ requestId }), { get requestId() { throw new Error("접근자 호출"); } }]) {
    deliver(payload);
  }
  expect(ipc).not.toHaveBeenCalled();
});

it.each(["return", "throw", "invalid-claim"] as const)("부착 실패(%s)는 정확한 핸들로 거절한다", async (failure) => {
  const attach = vi.fn(async () => {
    if (failure === "throw") throw new Error("탭 부착 실패");
  });
  ipc.mockResolvedValueOnce(failure === "invalid-claim" ? { ...opened, text: 42 } : opened);
  subscribe(attach);
  deliver();
  await vi.waitFor(() => expect(ipc.mock.calls).toEqual([
    ["editor_local_cli_claim", { requestId }],
    ["editor_local_cli_respond", { requestId, handle, attached: false }],
  ]));
  expect(attach).toHaveBeenCalledTimes(failure === "invalid-claim" ? 0 : 1);
});

it("거부된 claim에서 탭을 열거나 다른 IPC를 호출하지 않는다", async () => {
  const rejected = Promise.reject(new Error("요청 만료"));
  ipc.mockReturnValueOnce(rejected);
  const attach = vi.fn();
  subscribe(attach);
  deliver();
  await rejected.catch(() => {});
  expect(attach).not.toHaveBeenCalled();
  expect(ipc.mock.calls).toEqual([["editor_local_cli_claim", { requestId }]]);
});

it("성공 응답 실패를 부착 콜백에 전달하고 같은 핸들만 거절한다", async () => {
  const error = new Error("응답 거부");
  const response = Promise.withResolvers<unknown>();
  const attach = vi.fn<Parameters<typeof subscribeLocalCliOpen>[0]>(async (_file, respond) => {
    try {
      await respond({ attached: true });
    } catch (reason) {
      response.resolve(reason);
    }
  });
  ipc.mockResolvedValueOnce(opened).mockRejectedValueOnce(error);
  subscribe(attach);
  deliver();
  await expect(response.promise).resolves.toBe(error);
  expect(ipc.mock.calls).toEqual([
    ["editor_local_cli_claim", { requestId }],
    ["editor_local_cli_respond", { requestId, handle, attached: true }],
    ["editor_local_cli_respond", { requestId, handle, attached: false }],
  ]);
});

it("구독 등록 전 해제하면 늦게 등록된 리스너도 한 번만 해제한다", async () => {
  const registration = Promise.withResolvers<UnlistenFn>();
  register.mockReturnValueOnce(registration.promise);
  const stop = subscribe(vi.fn());
  stop();
  stop();
  deliver();
  registration.resolve(unlisten);
  await vi.waitFor(() => expect(unlisten).toHaveBeenCalledTimes(1));
  expect(ipc).not.toHaveBeenCalled();
});

it("해제 뒤 도착한 claim은 탭을 열지 않고 정확한 핸들을 거절한다", async () => {
  const pending = Promise.withResolvers<unknown>();
  ipc.mockReturnValueOnce(pending.promise);
  const attach = vi.fn();
  const stop = subscribe(attach);
  deliver();
  stop();
  pending.resolve(opened);
  await vi.waitFor(() => expect(ipc.mock.calls).toEqual([
    ["editor_local_cli_claim", { requestId }],
    ["editor_local_cli_respond", { requestId, handle, attached: false }],
  ]));
  expect(attach).not.toHaveBeenCalled();
});

it("탭 부착 중 해제하면 신호를 중단하고 뒤늦은 성공 응답을 막는다", async () => {
  const mounted = Promise.withResolvers<void>();
  const attached = Promise.withResolvers<void>();
  const attach = vi.fn<Parameters<typeof subscribeLocalCliOpen>[0]>(async (_file, respond, signal) => {
    mounted.resolve();
    await attached.promise;
    expect(signal.aborted).toBe(true);
    await expect(respond({ attached: true })).rejects.toThrow();
  });
  ipc.mockResolvedValueOnce(opened);
  const stop = subscribe(attach);
  deliver();
  await mounted.promise;
  stop();
  expect(ipc.mock.calls).toEqual([
    ["editor_local_cli_claim", { requestId }],
    ["editor_local_cli_respond", { requestId, handle, attached: false }],
  ]);
  attached.resolve();
  await attach.mock.results[0].value;
  expect(ipc).toHaveBeenCalledTimes(2);
});

it("구독 등록 오류는 활성 구독의 호출자에게 전달한다", async () => {
  const error = new Error("구독 거부");
  register.mockRejectedValueOnce(error);
  const received = Promise.withResolvers<unknown>();
  subscribe(vi.fn(), received.resolve);
  await expect(received.promise).resolves.toBe(error);
  expect(ipc).not.toHaveBeenCalled();
});
