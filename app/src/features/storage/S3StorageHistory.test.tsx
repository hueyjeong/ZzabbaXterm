// @vitest-environment jsdom
import { act, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { S3StoragePanel } from "./S3StoragePanel";
import type { S3Actions, S3Result, S3Session } from "./s3";

const session: S3Session = { sessionId: "first", profile: "sso", region: "ap-northeast-2", endpoint: "https://s3.ap-northeast-2.amazonaws.com" };
const prefix = "실험//";
const bucket = "history-bucket";
const request = { sessionId: session.sessionId, bucket, region: session.region, prefix: "" };
const historyLabel = "이전 버전이 있는 파일: ";
type CountHistory = NonNullable<ComponentProps<typeof S3StoragePanel>["countHistory"]>;
let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.unstubAllGlobals();
});

function unused(): never { throw new Error("예상하지 않은 작업"); }

const actions: S3Actions = {
  listBuckets: async () => ({ ok: true, value: { rows: [{ name: bucket, region: session.region }], nextToken: null } }),
  listObjects: async () => ({ ok: true, value: { rows: [{ kind: "prefix", prefix }], nextToken: null } }),
  selectLocalFile: unused, upload: unused, download: unused,
};

function render(countHistory: CountHistory, openedSession = session) {
  return act(async () => root.render(<S3StoragePanel session={openedSession} actions={actions} countHistory={countHistory} />));
}

function click(text: string) {
  const button = Array.from(host.querySelectorAll("button")).find(node => node.textContent === text);
  if (!button) throw new Error(`버튼 없음: ${text}`);
  return act(async () => button.click());
}

it("객체 목록을 먼저 열고 이동·세션 변경 전에 시작한 이력 결과를 버린다", async () => {
  const previous = Promise.withResolvers<S3Result<number>>();
  const current = Promise.withResolvers<S3Result<number>>();
  const reopened = Promise.withResolvers<S3Result<number>>();
  const countHistory = vi.fn<CountHistory>()
    .mockReturnValueOnce(previous.promise)
    .mockReturnValueOnce(current.promise)
    .mockReturnValueOnce(reopened.promise);
  await render(countHistory);
  await click("버킷 목록");
  expect(countHistory).not.toHaveBeenCalled();
  await click(bucket);
  expect(countHistory).toHaveBeenCalledExactlyOnceWith(request);
  expect(host.textContent).toContain(`${historyLabel}확인 중`);
  await click(prefix);
  expect(countHistory).toHaveBeenLastCalledWith({ ...request, prefix });
  await act(async () => current.resolve({ ok: true, value: 0 }));
  await act(async () => previous.resolve({ ok: true, value: 7 }));
  expect(host.textContent).toContain(`${historyLabel}0개`);
  expect(host.textContent).not.toContain("7개");

  await render(countHistory, { ...session, sessionId: "second" });
  expect(host.textContent).not.toContain(historyLabel);
  await render(countHistory);
  expect(host.textContent).toContain(`${historyLabel}확인 중`);
  expect(host.textContent).not.toContain("0개");
  await act(async () => root.render(null));
  await act(async () => reopened.resolve({ ok: true, value: 9 }));
  await render(countHistory);
  expect(host.textContent).not.toContain(historyLabel);
});

it("권한 거부·미지원·기타 실패를 0개와 구분한다", async () => {
  const countHistory = vi.fn<CountHistory>();
  await render(countHistory);
  for (const [status, code, label] of [
    [403, "AccessDenied", "권한 거부"],
    [501, "NotImplemented", "지원 안 함"],
    [500, "InternalError", "확인 실패"],
  ] as const) {
    countHistory.mockResolvedValueOnce({ ok: false, error: {
      stage: "listObjects", failure: { kind: "service", status, code }, bytes: 0,
    } });
    await click("버킷 목록");
    await click(bucket);
    expect(host.textContent).toContain(`${historyLabel}${label}`);
    expect(host.textContent).not.toContain("0개");
  }
  countHistory.mockRejectedValueOnce(new Error("IPC 연결 끊김"));
  await click("버킷 목록");
  await click(bucket);
  expect(host.textContent).toContain(`${historyLabel}확인 실패`);
});

it("이동을 시작하면 지난 수를 숨기고 SSO 재로그인 전후에 다시 표시하지 않는다", async () => {
  const list = Promise.withResolvers<S3Result<never>>();
  const listObjects = vi.fn<S3Actions["listObjects"]>().mockImplementationOnce(actions.listObjects).mockReturnValueOnce(list.promise);
  const countHistory = vi.fn<CountHistory>().mockResolvedValue({ ok: true, value: 4 });
  const onRelogin = vi.fn();
  await act(async () => root.render(<S3StoragePanel session={session} actions={{ ...actions, listObjects }} countHistory={countHistory} onRelogin={onRelogin} />));
  await click("버킷 목록");
  await click(bucket);
  expect(host.textContent).toContain(`${historyLabel}4개`);
  await click(prefix);
  expect(host.textContent).not.toContain("4개");
  expect(countHistory).toHaveBeenCalledTimes(1);
  await act(async () => list.resolve({ ok: false, error: { stage: "listObjects", failure: { kind: "ssoLoginRequired" }, bytes: 0 } }));
  await click("SSO 재로그인");
  expect(onRelogin).toHaveBeenCalledTimes(1);
  expect(host.textContent).not.toContain(historyLabel);
});
