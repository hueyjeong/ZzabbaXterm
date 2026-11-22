import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWebview, type Webview } from "@tauri-apps/api/webview";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { subscribeRemoteNanoOpen } from "./nanoOpen";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), isTauri: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("@tauri-apps/api/webview", () => ({ getCurrentWebview: vi.fn() }));

const event = "editor-remote-nano-open";
const pending = "editor_remote_nano_pending";
const claim = "editor_remote_nano_claim";
const mounted = "editor_remote_nano_mounted";
const failed = "editor_remote_nano_failed";
const offerId = "a".repeat(32);
const sessionId = "18446744073709551615";
const handle = "b".repeat(32);
const offer = { offerId, sessionId };
const opened = { handle, name: "한글.txt", text: "본문" };
const ipc = vi.fn<(command: string, ...args: unknown[]) => Promise<unknown>>();
const register = vi.mocked(listen);
const unlisten = vi.fn();
let unsubscribe: (() => void) | undefined;
type Attach = Parameters<typeof subscribeRemoteNanoOpen>[0];

function subscribe(attach: Attach) {
  unsubscribe = subscribeRemoteNanoOpen(attach);
  return unsubscribe;
}

function deliver(payload: unknown = offer) {
  register.mock.calls[0][1]({ event, id: 1, payload });
}

beforeEach(() => {
  vi.resetAllMocks();
  ipc.mockImplementation(async (command) => command === claim ? opened : command === pending ? [] : undefined);
  vi.mocked(invoke).mockImplementation(ipc);
  vi.mocked(isTauri).mockReturnValue(true);
  vi.mocked(getCurrentWebview).mockReturnValue({ label: "main" } as Webview);
  register.mockResolvedValue(unlisten);
});

afterEach(() => {
  unsubscribe?.();
  unsubscribe = undefined;
});

it("구독 후 대기 목록을 읽고 중복 이벤트를 한 번만 클레임하며, 실제 부착 뒤 정확한 핸들을 승인한다", async () => {
  const registration = Promise.withResolvers<UnlistenFn>();
  const snapshot = Promise.withResolvers<unknown>();
  register.mockReturnValueOnce(registration.promise);
  ipc.mockImplementation(async (command) => command === pending ? snapshot.promise : command === claim ? opened : undefined);
  const editorMounted = Promise.withResolvers<void>();
  const attach = vi.fn<Attach>(async (file, acknowledge) => {
    await editorMounted.promise;
    expect(await Promise.all([acknowledge(), acknowledge()])).toEqual([{ kind: "mounted" }, { kind: "mounted" }]);
    await file.transport.compare(file.handle);
    await file.close();
  });
  subscribe(attach);
  expect(register).toHaveBeenCalledExactlyOnceWith(event, expect.any(Function), { target: { kind: "Webview", label: "main" } });
  expect(ipc).not.toHaveBeenCalled();
  registration.resolve(unlisten);
  await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith(pending));
  deliver();
  deliver();
  snapshot.resolve([offer]);
  await vi.waitFor(() => expect(attach).toHaveBeenCalledTimes(1));
  expect(attach.mock.calls[0][0]).toMatchObject({ offerId, sessionId, handle, label: opened.name, text: opened.text });
  expect(ipc.mock.calls).toEqual([[pending], [claim, { offerId }]]);
  editorMounted.resolve();
  await attach.mock.results[0].value;
  expect(ipc.mock.calls).toEqual([
    [pending], [claim, { offerId }], [mounted, { offerId, handle }],
    ["editor_sftp_compare", { sessionId, handle }], ["editor_sftp_close", { sessionId, handle }],
  ]);
});

it("대기 목록에만 남은 정상 오퍼를 처리하고 잘못된 페이로드는 무시한다", async () => {
  ipc.mockResolvedValueOnce([null, { ...offer, sessionId: 7 }, { ...offer, sessionId: "7\n" }, offer]);
  const attach = vi.fn<Attach>(async () => {});
  subscribe(attach);
  deliver({ ...offer, offerId: "a".repeat(31) + "\n" });
  deliver({ ...offer, path: "/etc/example" });
  await vi.waitFor(() => expect(ipc.mock.calls).toEqual([[pending], [claim, { offerId }], [failed, { offerId }]]));
  expect(attach).toHaveBeenCalledTimes(1);
});

it("잘못된 클레임 응답은 부착하지 않고 동일 오퍼를 실패 처리한다", async () => {
  const attach = vi.fn<Attach>();
  ipc.mockImplementation(async (command) => command === pending ? [offer] : { ...opened, text: 1 });
  subscribe(attach);
  await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith(failed, { offerId }));
  expect(attach).not.toHaveBeenCalled();
});

it("부착 전 구독을 해제하면 뒤늦은 부착 보고를 승인하지 않는다", async () => {
  const lateMount = Promise.withResolvers<void>();
  const attach = vi.fn<Attach>(async (_file, acknowledge, signal) => {
    await lateMount.promise;
    expect(signal.aborted).toBe(true);
    expect(await acknowledge()).toEqual({ kind: "aborted" });
  });
  const stop = subscribe(attach);
  deliver();
  await vi.waitFor(() => expect(attach).toHaveBeenCalledTimes(1));
  stop();
  expect(ipc).toHaveBeenCalledWith(failed, { offerId });
  lateMount.resolve();
  await attach.mock.results[0].value;
  expect(ipc.mock.calls.filter(([command]) => command === failed)).toEqual([[failed, { offerId }]]);
  expect(ipc).not.toHaveBeenCalledWith(mounted, expect.anything());
  expect(unlisten).toHaveBeenCalledTimes(1);
});

it("mount 응답이 유실되면 이미 수락되었을 가능성을 호출자에게 반환한다", async () => {
  const error = new Error("응답 유실");
  const response = Promise.withResolvers<void>();
  ipc.mockImplementation(async (command) => command === pending ? [offer] : command === claim ? opened : command === mounted ? response.promise : undefined);
  const attach = vi.fn<Attach>(async (_file, acknowledge) => {
    expect(await acknowledge()).toEqual({ kind: "unconfirmed", error });
  });
  const stop = subscribe(attach);
  await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith(mounted, { offerId, handle }));
  stop();
  expect(ipc).not.toHaveBeenCalledWith(failed, expect.anything());
  response.reject(error);
  await attach.mock.results[0].value;
  expect(ipc.mock.calls).toEqual([[pending], [claim, { offerId }], [mounted, { offerId, handle }], [failed, { offerId }]]);
});

it("클레임 중 구독을 해제하면 클레임 응답 뒤 같은 오퍼를 실패 처리한다", async () => {
  const response = Promise.withResolvers<unknown>();
  ipc.mockImplementation(async (command) => command === pending ? [offer] : command === claim ? response.promise : undefined);
  const attach = vi.fn<Attach>();
  const stop = subscribe(attach);
  await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith(claim, { offerId }));
  stop();
  expect(ipc).not.toHaveBeenCalledWith(failed, expect.anything());
  response.resolve(opened);
  await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith(failed, { offerId }));
  expect(attach).not.toHaveBeenCalled();
});

it("보조 웹뷰에서는 구독도 대기 목록 조회도 시작하지 않는다", async () => {
  vi.mocked(getCurrentWebview).mockReturnValue({ label: "editor" } as Webview);
  subscribe(vi.fn<Attach>());
  await Promise.resolve();
  expect(register).not.toHaveBeenCalled();
  expect(ipc).not.toHaveBeenCalled();
});
