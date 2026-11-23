// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { S3StoragePanel } from "./S3StoragePanel";
import { LOCAL_DRAG_TYPE, S3_DRAG_TYPE } from "./LocalPane";
import type { LocalFileActions, LocalListing, S3Actions, S3Session } from "./s3";
import { control, dblclick, fileRow, findControl } from "../files/testing";
import { TAB_DRAG_TYPE } from "../workspace/TabStrip";

const session: S3Session = { sessionId: "local-session", profile: "dev-sso", region: "ap-northeast-2", endpoint: "https://s3.ap-northeast-2.amazonaws.com" };
const objectKey = "원격 폴더/받을 파일 +%.txt";
const rootListing: LocalListing = {
  truncated: false,
  entries: [
    { name: "하위 폴더", kind: "folder", bytes: null, modified: null },
    { name: "올릴 파일.txt", kind: "file", bytes: 12, modified: Date.UTC(2026, 0, 2, 3, 4) },
    { name: "바깥 링크", kind: "link", bytes: null, modified: null },
  ],
};
const childListing: LocalListing = { truncated: true, entries: [{ name: "깊은 파일.bin", kind: "file", bytes: 3, modified: null }] };
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

function s3Actions(overrides: Partial<S3Actions> = {}): S3Actions {
  return {
    listBuckets: unused,
    listObjects: vi.fn<S3Actions["listObjects"]>().mockResolvedValue({
      ok: true, value: { rows: [{ kind: "object", key: objectKey, bytes: 9 }], nextToken: null },
    }),
    selectLocalFile: unused,
    upload: vi.fn<S3Actions["upload"]>().mockResolvedValue({ ok: true, value: 12 }),
    download: vi.fn<S3Actions["download"]>().mockResolvedValue({ ok: true, value: { bytes: 9, integrity: "lengthOnly" } }),
    ...overrides,
  };
}

function localActions(overrides: Partial<LocalFileActions> = {}): LocalFileActions {
  return {
    openRoot: vi.fn<LocalFileActions["openRoot"]>().mockResolvedValue({ kind: "approved", rootId: "root-1", label: "C:\\작업 폴더" }),
    list: vi.fn<LocalFileActions["list"]>(async ({ path }) => ({ ok: true, value: path ? childListing : rootListing })),
    select: vi.fn<LocalFileActions["select"]>().mockResolvedValue({ kind: "approved", approvalId: "local-approval" }),
    ...overrides,
  };
}

function render(actions: S3Actions, local: LocalFileActions) {
  return act(async () => root.render(<S3StoragePanel session={session} actions={actions} localActions={local} />));
}

function localPane() {
  return host.querySelector<HTMLElement>(".s3-local")!;
}

function click(name: string, scope: ParentNode = host) {
  return act(async () => (control(scope, name) as HTMLButtonElement).click());
}

function fill(label: string, value: string) {
  const node = Array.from(host.querySelectorAll("label")).find(node => node.textContent === label)?.querySelector("input");
  if (!node) throw new Error(`입력란 없음: ${label}`);
  return act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(node, value);
    node.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function openBucket() {
  await fill("버킷", "local-bucket");
  await fill("접두어", "원격 폴더/");
  await click("버킷 열기");
}

/** jsdom에는 DataTransfer가 없으므로 끌기에 필요한 부분만 흉내 낸다. */
function transfer(initial: Record<string, string> = {}) {
  const data = new Map(Object.entries(initial));
  return {
    get types() { return [...data.keys()]; },
    setData: (type: string, value: string) => { data.set(type, value); },
    getData: (type: string) => data.get(type) ?? "",
    dropEffect: "none",
    effectAllowed: "all",
  };
}

async function drag(node: Element, type: string, data: ReturnType<typeof transfer>) {
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperty(event, "dataTransfer", { value: data });
  await act(async () => { node.dispatchEvent(event); });
  return event;
}

it("루트 미승인 상태에서는 로컬 폴더 열기 단추만 보이고 승인 뒤 상대 경로로 목록을 연다", async () => {
  const local = localActions();
  await render(s3Actions(), local);
  expect(Array.from(localPane().querySelectorAll("button")).map(button => button.getAttribute("aria-label") ?? button.textContent)).toEqual(["로컬 폴더 열기"]);
  expect(localPane().querySelector('[role="grid"]')).toBeNull();
  expect(local.list).not.toHaveBeenCalled();

  await click("로컬 폴더 열기", localPane());
  expect(local.openRoot).toHaveBeenCalledExactlyOnceWith({ sessionId: session.sessionId });
  expect(local.list).toHaveBeenCalledExactlyOnceWith({ sessionId: session.sessionId, rootId: "root-1", path: "" });
  expect(fileRow(host, "올릴 파일.txt").querySelector(".file-name")?.textContent).toBe("올릴 파일.txt");
  expect(fileRow(host, "바깥 링크").querySelector('.file-icon[data-type="link"]')).not.toBeNull();
  expect(localPane().querySelector('[aria-label="로컬 위치"]')?.textContent).toBe("작업 폴더");

  await dblclick(fileRow(host, "바깥 링크"));
  expect(local.list).toHaveBeenCalledTimes(1);
  await dblclick(fileRow(host, "하위 폴더"));
  expect(local.list).toHaveBeenLastCalledWith({ sessionId: session.sessionId, rootId: "root-1", path: "하위 폴더" });
  expect(fileRow(host, "깊은 파일.bin")).toBeDefined();
  expect(localPane().querySelector(".s3-status")?.textContent).toBe("1개 항목 이상");
  await click("로컬 상위 폴더", localPane());
  expect(local.list).toHaveBeenLastCalledWith({ sessionId: session.sessionId, rootId: "root-1", path: "" });
});

it("취소한 루트 선택은 상태를 바꾸지 않고 폐기된 루트는 다시 고르게 한다", async () => {
  const openRoot = vi.fn<LocalFileActions["openRoot"]>().mockResolvedValueOnce({ kind: "cancelled" })
    .mockResolvedValueOnce({ kind: "approved", rootId: "root-1", label: "/home/시험" });
  const list = vi.fn<LocalFileActions["list"]>().mockResolvedValueOnce({ ok: true, value: rootListing })
    .mockResolvedValueOnce({ ok: false, error: { kind: "denied" } });
  await render(s3Actions(), localActions({ openRoot, list }));
  await click("로컬 폴더 열기", localPane());
  expect(list).not.toHaveBeenCalled();
  expect(localPane().querySelector('[role="alert"]')).toBeNull();
  await click("로컬 폴더 열기", localPane());
  await click("로컬 새로고침", localPane());
  expect(localPane().querySelector('[role="alert"]')?.textContent).toBe("로컬 폴더: 승인 거부");
  expect(Array.from(localPane().querySelectorAll(".s3-local-start button")).map(button => button.textContent))
    .toEqual(["로컬 폴더 열기"]);
});

it("로컬 파일을 현재 S3 접두어로 올리고 완료 뒤 S3 목록을 새로 읽는다", async () => {
  const actions = s3Actions();
  const local = localActions();
  await render(actions, local);
  await click("로컬 폴더 열기", localPane());
  await act(async () => fileRow(host, "바깥 링크").click());
  expect((control(localPane(), "S3로 업로드") as HTMLButtonElement).disabled).toBe(true);
  await act(async () => fileRow(host, "올릴 파일.txt").click());
  expect((control(localPane(), "S3로 업로드") as HTMLButtonElement).disabled).toBe(true);
  await openBucket();
  expect(actions.listObjects).toHaveBeenCalledTimes(1);
  await click("S3로 업로드", localPane());
  const target = { sessionId: session.sessionId, bucket: "local-bucket", region: session.region, key: "원격 폴더/올릴 파일.txt" };
  expect(local.select).toHaveBeenCalledExactlyOnceWith({ ...target, rootId: "root-1", path: "올릴 파일.txt", direction: "upload" });
  expect(actions.upload).toHaveBeenCalledExactlyOnceWith({ ...target, approvalId: "local-approval" });
  expect(actions.listObjects).toHaveBeenCalledTimes(2);
  const row = host.querySelector(".transfer-table tbody tr")!;
  expect(row.textContent).toContain("완료");
  expect(row.querySelector(".transfer-key")?.getAttribute("title")).toBe("local-bucket · ap-northeast-2 · C:\\작업 폴더\\올릴 파일.txt");
});

it("S3 객체를 현재 로컬 폴더로 내려받고 완료 뒤 로컬 목록을 새로 읽는다", async () => {
  const actions = s3Actions();
  const local = localActions();
  await render(actions, local);
  await openBucket();
  await act(async () => fileRow(host, objectKey).click());
  expect(findControl(host, "로컬 폴더로 다운로드")).toBeDefined();
  expect((control(host, "로컬 폴더로 다운로드") as HTMLButtonElement).disabled).toBe(true);
  await click("로컬 폴더 열기", localPane());
  await dblclick(fileRow(host, "하위 폴더"));
  expect(local.list).toHaveBeenCalledTimes(2);
  await click("로컬 폴더로 다운로드");
  const target = { sessionId: session.sessionId, bucket: "local-bucket", region: session.region, key: objectKey };
  expect(local.select).toHaveBeenCalledExactlyOnceWith({ ...target, rootId: "root-1", path: "하위 폴더", direction: "download" });
  expect(actions.download).toHaveBeenCalledExactlyOnceWith({ ...target, approvalId: "local-approval" });
  expect(local.list).toHaveBeenCalledTimes(3);
  expect(local.list).toHaveBeenLastCalledWith({ sessionId: session.sessionId, rootId: "root-1", path: "하위 폴더" });
});

it("교체를 거절한 다운로드는 전송하지 않고 취소로 표시한다", async () => {
  const actions = s3Actions();
  const local = localActions({ select: vi.fn<LocalFileActions["select"]>().mockResolvedValue({ kind: "cancelled" }) });
  await render(actions, local);
  await openBucket();
  await click("로컬 폴더 열기", localPane());
  await act(async () => fileRow(host, objectKey).click());
  await click("로컬 폴더로 다운로드");
  expect(actions.download).not.toHaveBeenCalled();
  expect(host.querySelector(".transfer-table tbody tr")?.textContent).toContain("취소");
  expect(local.list).toHaveBeenCalledTimes(1);
});

it("두 창 사이 끌어 놓기는 전용 형식만 받아 업로드와 다운로드를 시작한다", async () => {
  const actions = s3Actions();
  const local = localActions();
  await render(actions, local);
  await openBucket();
  await click("로컬 폴더 열기", localPane());

  const fromS3 = transfer();
  const start = await drag(fileRow(host, objectKey), "dragstart", fromS3);
  expect(start.defaultPrevented).toBe(false);
  expect(fromS3.types).toEqual([S3_DRAG_TYPE]);
  const over = await drag(localPane(), "dragover", fromS3);
  expect(over.defaultPrevented).toBe(true);
  expect(localPane().hasAttribute("data-drop")).toBe(true);
  await drag(localPane(), "drop", fromS3);
  expect(localPane().hasAttribute("data-drop")).toBe(false);
  const target = { sessionId: session.sessionId, bucket: "local-bucket", region: session.region };
  expect(local.select).toHaveBeenCalledExactlyOnceWith({ ...target, key: objectKey, rootId: "root-1", path: "", direction: "download" });

  const fromLocal = transfer();
  await drag(fileRow(host, "올릴 파일.txt"), "dragstart", fromLocal);
  expect(fromLocal.types).toEqual([LOCAL_DRAG_TYPE]);
  const files = host.querySelector(".s3-files")!;
  expect((await drag(files, "dragover", fromLocal)).defaultPrevented).toBe(true);
  await drag(files, "drop", fromLocal);
  expect(local.select).toHaveBeenLastCalledWith({ ...target, key: "원격 폴더/올릴 파일.txt", rootId: "root-1", path: "올릴 파일.txt", direction: "upload" });
  expect(actions.upload).toHaveBeenCalledTimes(1);

  // 폴더·링크는 끌 수 없고, 다른 형식이나 다른 세션의 항목은 놓지 않는다.
  expect((await drag(fileRow(host, "바깥 링크"), "dragstart", transfer())).defaultPrevented).toBe(true);
  expect((await drag(fileRow(host, "하위 폴더"), "dragstart", transfer())).defaultPrevented).toBe(true);
  for (const foreign of [
    transfer({ Files: "" }),
    transfer({ [TAB_DRAG_TYPE]: "1" }),
    transfer({ "text/plain": objectKey }),
  ]) {
    expect((await drag(localPane(), "dragover", foreign)).defaultPrevented).toBe(false);
    expect((await drag(files, "dragover", foreign)).defaultPrevented).toBe(false);
  }
  await drag(localPane(), "drop", transfer({ [S3_DRAG_TYPE]: JSON.stringify({ sessionId: "other", key: objectKey }) }));
  await drag(files, "drop", transfer({ [LOCAL_DRAG_TYPE]: JSON.stringify({ sessionId: session.sessionId, rootId: "old-root", path: "x", name: "x", bytes: 1 }) }));
  expect(local.select).toHaveBeenCalledTimes(2);
});

it("760px보다 좁으면 두 창을 위아래로 쌓고 구분선을 숨긴다", async () => {
  let notify: ((width: number) => void) | undefined;
  vi.stubGlobal("ResizeObserver", class {
    constructor(callback: (entries: { contentRect: { width: number } }[]) => void) {
      notify = width => callback([{ contentRect: { width } }]);
    }
    observe() { notify?.(600); }
    disconnect() { notify = undefined; }
  });
  await render(s3Actions(), localActions());
  const split = host.querySelector(".s3-split")!;
  expect(split.hasAttribute("data-stacked")).toBe(true);
  expect(host.querySelector('[aria-label="로컬 창 크기"]')).toBeNull();
  await act(async () => notify?.(1200));
  expect(split.hasAttribute("data-stacked")).toBe(false);
  const separator = host.querySelector('[aria-label="로컬 창 크기"]')!;
  expect(separator.getAttribute("aria-valuemax")).toBe("720");
  expect(separator.getAttribute("aria-valuenow")).toBe("360");
  await act(async () => notify?.(500 / 0.6));
  expect(split.hasAttribute("data-stacked")).toBe(false);
  expect(separator.getAttribute("aria-valuemax")).toBe("500");
  expect(separator.getAttribute("aria-valuenow")).toBe("360");
  await act(async () => notify?.(780));
  expect(separator.getAttribute("aria-valuemax")).toBe("468");
});

it("시스템·자격 증명 폴더는 루트로 쓰지 않고 상태만 표시한다", async () => {
  const local = localActions({ openRoot: vi.fn<LocalFileActions["openRoot"]>().mockResolvedValue({ kind: "restricted" }) });
  await render(s3Actions(), local);
  await click("로컬 폴더 열기", localPane());
  expect(localPane().querySelector('[role="alert"]')?.textContent).toBe("로컬 폴더: 이 폴더는 쓸 수 없음");
  expect(local.list).not.toHaveBeenCalled();
  expect(Array.from(localPane().querySelectorAll(".s3-local-start button")).map(button => button.textContent))
    .toEqual(["로컬 폴더 열기"]);
});

it("로컬 전송 거부 원인과 S3 존재 확인 오류를 전송 행에 표시한다", async () => {
  const select = vi.fn<LocalFileActions["select"]>()
    .mockResolvedValueOnce({ kind: "rejected", reason: { kind: "invalidPath" } })
    .mockResolvedValueOnce({ kind: "failed", error: { stage: "headObject", failure: { kind: "service", status: 403, code: "AccessDenied" }, bytes: 0 } });
  const actions = s3Actions();
  await render(actions, localActions({ select }));
  await openBucket();
  await click("로컬 폴더 열기", localPane());
  await act(async () => fileRow(host, objectKey).click());
  await click("로컬 폴더로 다운로드");
  await act(async () => fileRow(host, "올릴 파일.txt").click());
  await click("S3로 업로드", localPane());
  const rows = Array.from(host.querySelectorAll(".transfer-table tbody tr"));
  expect(rows.map(row => row.getAttribute("data-state"))).toEqual(["failed", "rejected"]);
  expect(rows[0].querySelector(".transfer-result")?.textContent).toBe("객체 확인: 권한 거부 (403 AccessDenied) · 0 바이트");
  expect(rows[1].querySelector(".transfer-state")?.textContent).toBe("거부");
  expect(rows[1].querySelector(".transfer-result")?.textContent).toBe("잘못된 경로");
  expect(actions.upload).not.toHaveBeenCalled();
  expect(actions.download).not.toHaveBeenCalled();
});

it("루트가 옮겨지면 목록과 전송 모두 루트를 해제한다", async () => {
  const list = vi.fn<LocalFileActions["list"]>().mockResolvedValueOnce({ ok: true, value: rootListing })
    .mockResolvedValueOnce({ ok: false, error: { kind: "rootChanged" } })
    .mockResolvedValue({ ok: true, value: rootListing });
  const select = vi.fn<LocalFileActions["select"]>().mockResolvedValue({ kind: "rejected", reason: { kind: "rootChanged" } });
  const actions = s3Actions();
  await render(actions, localActions({ list, select }));
  await click("로컬 폴더 열기", localPane());
  await click("로컬 새로고침", localPane());
  expect(localPane().querySelector('[role="alert"]')?.textContent).toBe("로컬 폴더: 로컬 폴더 이동됨");
  expect(localPane().querySelector('[role="grid"]')).toBeNull();

  await click("로컬 폴더 열기", localPane());
  await openBucket();
  await act(async () => fileRow(host, objectKey).click());
  await click("로컬 폴더로 다운로드");
  expect(host.querySelector(".transfer-table tbody tr .transfer-result")?.textContent).toBe("로컬 폴더 이동됨");
  expect(localPane().querySelector('[role="grid"]')).toBeNull();
  expect((control(host, "로컬 폴더로 다운로드") as HTMLButtonElement).disabled).toBe(true);
});

it("전송 중에는 루트를 바꾸지 않고 로컬 목록을 읽는 동안 업로드하지 않는다", async () => {
  const pending = Promise.withResolvers<Awaited<ReturnType<LocalFileActions["select"]>>>();
  const refresh = Promise.withResolvers<Awaited<ReturnType<LocalFileActions["list"]>>>();
  const list = vi.fn<LocalFileActions["list"]>().mockResolvedValueOnce({ ok: true, value: rootListing }).mockReturnValueOnce(refresh.promise);
  const local = localActions({ list, select: vi.fn<LocalFileActions["select"]>(() => pending.promise) });
  await render(s3Actions(), local);
  await openBucket();
  await click("로컬 폴더 열기", localPane());
  await act(async () => fileRow(host, "올릴 파일.txt").click());
  await click("로컬 새로고침", localPane());
  expect((control(localPane(), "S3로 업로드") as HTMLButtonElement).disabled).toBe(true);
  await act(async () => refresh.resolve({ ok: true, value: rootListing }));
  expect((control(localPane(), "S3로 업로드") as HTMLButtonElement).disabled).toBe(false);
  await click("S3로 업로드", localPane());
  expect((control(localPane(), "로컬 폴더 열기") as HTMLButtonElement).disabled).toBe(true);
  await act(async () => pending.resolve({ kind: "cancelled" }));
  expect((control(localPane(), "로컬 폴더 열기") as HTMLButtonElement).disabled).toBe(false);
  expect(local.openRoot).toHaveBeenCalledTimes(1);
});

it("클라우드 전용 파일은 오프라인 아이콘으로 보이고 올리거나 끌 수 없다", async () => {
  const offline: LocalListing = {
    truncated: false,
    entries: [...rootListing.entries, { name: "클라우드 파일.txt", kind: "file", bytes: 5, modified: null, offline: true }],
  };
  const local = localActions({ list: vi.fn<LocalFileActions["list"]>().mockResolvedValue({ ok: true, value: offline }) });
  await render(s3Actions(), local);
  await openBucket();
  await click("로컬 폴더 열기", localPane());
  const row = fileRow(host, "클라우드 파일.txt");
  expect(row.querySelector('.file-icon[data-type="offline"]')).not.toBeNull();
  expect(row.getAttribute("aria-description")).toBe("클라우드 전용");
  expect(fileRow(host, "바깥 링크").getAttribute("aria-description")).toBe("링크");
  expect(fileRow(host, "올릴 파일.txt").hasAttribute("aria-description")).toBe(false);
  expect(fileRow(host, "올릴 파일.txt").querySelector('.file-icon[data-type="offline"]')).toBeNull();
  await act(async () => row.click());
  expect((control(localPane(), "S3로 업로드") as HTMLButtonElement).disabled).toBe(true);
  expect((await drag(row, "dragstart", transfer())).defaultPrevented).toBe(true);
  expect(local.select).not.toHaveBeenCalled();
});

it("클라우드 전용 거부와 조건부 업로드 충돌을 전송 행에 표시한다", async () => {
  const select = vi.fn<LocalFileActions["select"]>()
    .mockResolvedValueOnce({ kind: "rejected", reason: { kind: "cloudOnly" } })
    .mockResolvedValueOnce({ kind: "approved", approvalId: "local-approval" });
  const upload = vi.fn<S3Actions["upload"]>().mockResolvedValue({
    ok: false, error: { stage: "putObject", failure: { kind: "preconditionFailed" }, bytes: 0 },
  });
  await render(s3Actions({ upload }), localActions({ select }));
  await openBucket();
  await click("로컬 폴더 열기", localPane());
  await act(async () => fileRow(host, objectKey).click());
  await click("로컬 폴더로 다운로드");
  await act(async () => fileRow(host, "올릴 파일.txt").click());
  await click("S3로 업로드", localPane());
  const rows = Array.from(host.querySelectorAll(".transfer-table tbody tr"));
  expect(rows.map(row => row.getAttribute("data-state"))).toEqual(["failed", "rejected"]);
  expect(rows[0].querySelector(".transfer-result")?.textContent).toBe("객체 업로드: 다른 곳에서 객체가 바뀜 · 0 바이트");
  expect(rows[1].querySelector(".transfer-result")?.textContent).toBe("클라우드 전용 파일");
});

it("같은 키가 없을 때만 보낸 업로드가 거부되면 같은 키의 객체 있음으로 표시한다", async () => {
  const upload = vi.fn<S3Actions["upload"]>().mockResolvedValue({
    ok: false, error: { stage: "putObject", failure: { kind: "alreadyExists" }, bytes: 0 },
  });
  await render(s3Actions({ upload }), localActions());
  await openBucket();
  await click("로컬 폴더 열기", localPane());
  await act(async () => fileRow(host, "올릴 파일.txt").click());
  await click("S3로 업로드", localPane());
  expect(host.querySelector(".transfer-table tbody tr .transfer-result")?.textContent).toBe("객체 업로드: 같은 키의 객체 있음 · 0 바이트");
});
