// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { S3StoragePanel } from "./S3StoragePanel";
import type { Downloaded, LocalFileApproval, S3Actions, S3Result, S3Session } from "./s3";
import { control, dblclick, fileRow, keyDown } from "../files/testing";

const first: S3Session = { sessionId: "first", profile: "dev-sso", region: "ap-northeast-2", endpoint: "https://s3.ap-northeast-2.amazonaws.com" };
const second: S3Session = { sessionId: "second", profile: "other-sso", region: "eu-west-1", endpoint: "https://s3.eu-west-1.amazonaws.com" };
const key = "실험//한글 + %.txt";
const token = "opaque+/=%";
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

function render(actions: S3Actions, session = first) {
  return act(async () => root.render(<S3StoragePanel session={session} actions={actions} />));
}

function button(text: string) {
  return control(host, text) as HTMLButtonElement;
}

function click(text: string) {
  return act(async () => button(text).click());
}

function select(title: string) {
  return act(async () => fileRow(host, title).click());
}

function queue() {
  return Array.from(host.querySelectorAll(".transfer-table tbody tr")).map(row => row.textContent ?? "");
}

async function upload(key: string) {
  if (!host.querySelector('[role="dialog"][aria-label="업로드"]')) await click("업로드");
  await fill("업로드 전체 키", key);
  await click("파일 선택");
}

function fill(label: string, value: string) {
  const node = Array.from(host.querySelectorAll("label")).find(node => node.textContent === label)?.querySelector("input");
  if (!node) throw new Error(`입력란 없음: ${label}`);
  return act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(node, value);
    node.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

function unused(): never { throw new Error("예상하지 않은 작업"); }

it("승인 대기 중 탭과 객체를 바꿔도 최초 전체 키로 다운로드하고 실제 결과만 표시한다", async () => {
  const picker = Promise.withResolvers<LocalFileApproval>();
  const result = Promise.withResolvers<S3Result<Downloaded>>();
  const listObjects = vi.fn<S3Actions["listObjects"]>().mockResolvedValue({
    ok: true, value: {
      rows: [{ kind: "object", key, bytes: 9 }, { kind: "object", key: "다른 키", bytes: 1 }], nextToken: null,
    }
  });
  const selectLocalFile = vi.fn<S3Actions["selectLocalFile"]>(() => picker.promise);
  const download = vi.fn<S3Actions["download"]>(() => result.promise);
  const actions: S3Actions = { listBuckets: unused, listObjects, selectLocalFile, upload: unused, download };
  await render(actions);
  await fill("버킷", "direct-bucket");
  await fill("접두어", "실험//");
  await click("버킷 열기");
  await select(key);
  await click("다운로드");
  await select("다른 키");
  await render(actions, second);
  await fill("버킷", "other-bucket");
  await click("버킷 열기");
  await act(async () => picker.resolve({ kind: "approved", approvalId: "download-approval" }));
  const target = { sessionId: first.sessionId, bucket: "direct-bucket", region: first.region, key };
  expect(selectLocalFile).toHaveBeenCalledExactlyOnceWith({ ...target, direction: "download" });
  expect(download).toHaveBeenCalledExactlyOnceWith({ ...target, approvalId: "download-approval" });
  expect(queue()).toEqual([]);
  await act(async () => result.resolve({ ok: true, value: { bytes: 7, integrity: "lengthOnly" } }));
  expect(queue()).toEqual([]);
  await render(actions);
  expect(fileRow(host, "다른 키").getAttribute("aria-selected")).toBe("true");
  expect(queue()).toHaveLength(1);
  expect(queue()[0]).toContain("완료");
  expect(queue()[0]).toContain("7 바이트 · 길이만 확인");
  expect(host.querySelector(".transfer-key code")?.textContent).toBe(key);
});

it("원래 이름으로 복구를 확인할 수 없으면 미완료와 보존 파일 경로를 표시한다", async () => {
  const recoveryPath = "C:\\시험  폴더\\.zxt-보존\\원본.txt";
  await render({
    listBuckets: unused,
    listObjects: async () => ({ ok: true, value: { rows: [{ kind: "object", key, bytes: 7 }], nextToken: null } }),
    selectLocalFile: async () => ({ kind: "approved", approvalId: "download-approval" }),
    upload: unused,
    download: async () => ({
      ok: false, error: {
        stage: "downloadCommit", failure: { kind: "recoveryUnconfirmed", recoveryPath }, bytes: 7,
      },
    }),
  });
  await fill("버킷", "direct-bucket");
  await click("버킷 열기");
  await select(key);
  await click("다운로드");
  expect(queue()).toHaveLength(1);
  expect(queue()[0]).toContain("실패");
  expect(queue()[0]).toContain("다운로드 파일 확정: 복구 확인 실패 · 7 바이트");
  expect(host.querySelector(".transfer-result code")?.textContent).toBe(recoveryPath);
  expect(queue()[0]).not.toContain("완료");
});

it("버킷 권한이 없어도 지정 버킷을 열고, 다음 페이지 실패 시 같은 토큰과 선택을 유지한다", async () => {
  const error = { stage: "listObjects", failure: { kind: "service", status: 403, code: "AccessDenied" }, bytes: 0 } as const;
  const listBuckets = vi.fn<S3Actions["listBuckets"]>().mockResolvedValue({ ok: false, error: { ...error, stage: "listBuckets" } });
  const listObjects = vi.fn<S3Actions["listObjects"]>()
    .mockResolvedValueOnce({ ok: true, value: { rows: [{ kind: "prefix", prefix: "실험//" }], nextToken: null } })
    .mockResolvedValueOnce({ ok: true, value: { rows: [{ kind: "object", key, bytes: 10 }], nextToken: token } })
    .mockResolvedValueOnce({ ok: false, error })
    .mockResolvedValueOnce({ ok: true, value: { rows: [{ kind: "object", key: "다음 + %.txt", bytes: null }], nextToken: null } });
  await render({ listBuckets, listObjects, selectLocalFile: unused, upload: unused, download: unused });
  await click("버킷 목록");
  expect(host.textContent).toContain("권한 거부 (403 AccessDenied)");
  await fill("버킷", "direct-bucket");
  await click("버킷 열기");
  await dblclick(fileRow(host, "실험//"));
  await select(key);
  await click("더 불러오기");
  expect(fileRow(host, key).getAttribute("aria-selected")).toBe("true");
  expect(host.textContent).toContain(first.endpoint);
  await click("더 불러오기");
  const next = { sessionId: first.sessionId, bucket: "direct-bucket", prefix: "실험//", region: first.region, token };
  expect(listObjects.mock.calls.slice(2)).toEqual([[next], [next]]);
  expect(fileRow(host, key).getAttribute("aria-selected")).toBe("true");
  expect(fileRow(host, "다음 + %.txt")).toBeTruthy();
  expect(Array.from(host.querySelectorAll('[role="row"][aria-selected]')).map(row => row.getAttribute("title"))).toEqual(["다음 + %.txt", key]);
  expect(() => button("더 불러오기")).toThrow();
  expect(host.querySelector(".s3-status")?.textContent).toContain("2개 항목");
});

it("버킷 목록의 연속 토큰을 그대로 넘기고 업로드 파일 승인이 거부되면 목록과 선택을 유지한다", async () => {
  const listBuckets = vi.fn<S3Actions["listBuckets"]>()
    .mockResolvedValueOnce({ ok: true, value: { rows: [{ name: "first-bucket", region: "us-west-2" }], nextToken: token } })
    .mockResolvedValueOnce({ ok: true, value: { rows: [{ name: "second-bucket", region: first.region }], nextToken: null } });
  const listObjects = vi.fn<S3Actions["listObjects"]>().mockResolvedValue({
    ok: true, value: {
      rows: [{ kind: "object", key, bytes: 12 }], nextToken: null,
    }
  });
  const selectLocalFile = vi.fn<S3Actions["selectLocalFile"]>()
    .mockResolvedValueOnce({ kind: "denied" })
    .mockResolvedValueOnce({ kind: "approved", approvalId: "upload-approval" })
    .mockResolvedValueOnce({ kind: "approved", approvalId: "download-approval" });
  const uploadAction = vi.fn<S3Actions["upload"]>().mockResolvedValue({ ok: true, value: 16 });
  const download = vi.fn<S3Actions["download"]>().mockResolvedValue({
    ok: false, error: {
      stage: "downloadBody", bytes: 7, failure: { kind: "length", expected: 12 },
    }
  });
  await render({ listBuckets, listObjects, selectLocalFile, upload: uploadAction, download });
  await click("버킷 목록");
  await click("버킷 더 불러오기");
  expect(listBuckets).toHaveBeenLastCalledWith({ sessionId: first.sessionId, token });
  expect(button("second-bucket")).toBeTruthy();
  expect(() => button("버킷 더 불러오기")).toThrow();
  await click("first-bucket");
  await select(key);
  await upload(key);
  expect(queue()[0]).toContain("승인 거부");
  expect(uploadAction).not.toHaveBeenCalled();
  await upload(key);
  expect(uploadAction).toHaveBeenCalledExactlyOnceWith({ sessionId: first.sessionId, bucket: "first-bucket", region: "us-west-2", key, approvalId: "upload-approval" });
  expect(queue()[0]).toContain("완료");
  expect(queue()[0]).toContain("16 바이트");
  await click("다운로드");
  expect(queue()[0]).toContain("실패");
  expect(queue()[0]).toContain("다운로드 본문: 길이 불일치 (예상 12 바이트) · 7 바이트");
  // 전송 큐는 최신 항목부터 세션의 모든 전송을 보관한다.
  expect(queue().map(row => row.match(/(승인 거부|완료|실패)/)?.[0])).toEqual(["실패", "완료", "승인 거부"]);
  expect(Array.from(host.querySelectorAll(".transfer-direction svg")).map(icon => icon.getAttribute("aria-label")))
    .toEqual(["다운로드", "업로드", "업로드"]);
  expect(fileRow(host, key).getAttribute("aria-selected")).toBe("true");
});

it("SSO 재로그인, AWS 자격 증명 만료, 권한 거부를 구분한다", async () => {
  const listBuckets = vi.fn<S3Actions["listBuckets"]>();
  await render({ listBuckets, listObjects: unused, selectLocalFile: unused, upload: unused, download: unused });
  for (const [failure, label] of [
    [{ kind: "ssoLoginRequired" }, "같은 SSO 프로필로 다시 로그인 필요"],
    [{ kind: "service", status: 403, code: "ExpiredToken" }, "AWS 임시 자격 증명 만료"],
    [{ kind: "service", status: 403, code: "AccessDenied" }, "권한 거부"],
  ] as const) {
    listBuckets.mockResolvedValueOnce({ ok: false, error: { stage: "listBuckets", failure, bytes: 0 } });
    await click("버킷 목록");
    expect(host.querySelector('[role="alert"]')?.textContent).toContain(label);
  }
});

it("실패한 버킷 이동 뒤에는 이전 목록에서 전송할 수 없고 리전을 명시적으로 확인한다", async () => {
  const listObjects = vi.fn<S3Actions["listObjects"]>()
    .mockResolvedValueOnce({ ok: true, value: { rows: [{ kind: "object", key, bytes: 9 }], nextToken: token } })
    .mockResolvedValueOnce({ ok: false, error: { stage: "headBucket", failure: { kind: "bucketRegionMismatch", region: "us-west-2" }, bytes: 0 } })
    .mockResolvedValueOnce({ ok: true, value: { rows: [], nextToken: null } });
  const selectLocalFile = vi.fn<S3Actions["selectLocalFile"]>().mockResolvedValue({ kind: "cancelled" });
  await render({ listBuckets: unused, listObjects, selectLocalFile, upload: unused, download: unused });
  await fill("버킷", "first-bucket");
  await click("버킷 열기");
  await select(key);
  await click("업로드");
  await fill("업로드 전체 키", key);
  await click("주소 편집");
  await fill("버킷", "new-bucket");
  expect(button("업로드").disabled).toBe(true);
  expect(button("파일 선택").disabled).toBe(true);
  await click("버킷 열기");
  expect(button("다운로드").disabled).toBe(true);
  expect(button("더 불러오기").disabled).toBe(true);
  await click("파일 선택");
  await click("다운로드");
  await click("us-west-2 선택");
  expect(listObjects).toHaveBeenCalledTimes(2);
  expect(selectLocalFile).not.toHaveBeenCalled();
  await click("버킷 열기");
  await click("파일 선택");
  expect(selectLocalFile).toHaveBeenCalledExactlyOnceWith({ sessionId: first.sessionId, bucket: "new-bucket", region: "us-west-2", key, direction: "upload" });
});

it("한 줄 입력에서 LF와 CR 붙여넣기를 거부하고 승인에는 기존 원문 키를 전달한다", async () => {
  const literal = "  실험//../한글 +%2F.txt  ";
  const listObjects = vi.fn<S3Actions["listObjects"]>().mockResolvedValue({ ok: true, value: { rows: [], nextToken: null } });
  const selectLocalFile = vi.fn<S3Actions["selectLocalFile"]>().mockResolvedValue({ kind: "approved", approvalId: "literal" });
  const upload = vi.fn<S3Actions["upload"]>().mockResolvedValue({ ok: true, value: 12 });
  await render({ listBuckets: unused, listObjects, selectLocalFile, upload, download: unused });
  await fill("버킷", "literal-bucket");
  await click("버킷 열기");
  await click("업로드");
  await fill("업로드 전체 키", literal);
  const field = [...host.querySelectorAll("input")].find(input => input.value === literal)!;
  for (const value of ["다른\n키", "다른\r키", "다른\r\n키"]) {
    const event = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(event, "clipboardData", { value: { getData: (type: string) => type === "text/plain" ? value : "" } });
    await act(async () => { field.dispatchEvent(event); });
    expect(event.defaultPrevented).toBe(true);
    expect(field.value).toBe(literal);
    expect(host.querySelector('[role="alert"]')?.textContent).toContain("줄바꿈");
  }
  expect(upload).not.toHaveBeenCalled();
  await click("파일 선택");
  const target = { sessionId: first.sessionId, bucket: "literal-bucket", region: first.region, key: literal };
  expect(selectLocalFile).toHaveBeenCalledExactlyOnceWith({ ...target, direction: "upload" });
  expect(upload).toHaveBeenCalledExactlyOnceWith({ ...target, approvalId: "literal" });
});

it("뒤로·앞으로·상위 폴더·경로 조각·트리·키보드로 이동하고 본 접두어를 트리에 남긴다", async () => {
  const listObjects = vi.fn<S3Actions["listObjects"]>(async ({ prefix }) => ({
    ok: true, value: {
      nextToken: null, rows: prefix === "" ? [{ kind: "prefix", prefix: "a/" }, { kind: "prefix", prefix: "z/" }, { kind: "object", key: "root.txt", bytes: 1 }]
        : prefix === "a/" ? [{ kind: "prefix", prefix: "a/b/" }, { kind: "object", key: "a/x.txt", bytes: 2 }]
          : prefix === "a/b/" ? [{ kind: "object", key: "a/b/y.txt", bytes: 3 }] : [],
    },
  }));
  await render({ listBuckets: unused, listObjects, selectLocalFile: unused, upload: unused, download: unused });
  await fill("버킷", "nav-bucket");
  await click("버킷 열기");
  const prefixes = () => listObjects.mock.calls.map(([request]) => request.prefix);
  const here = () => host.querySelector<HTMLElement>('[aria-label="위치"] [aria-current="location"]')?.title;
  const tree = () => Array.from(host.querySelectorAll('[role="treeitem"]')).map(node =>
    `${node.getAttribute("aria-level")}:${node.getAttribute("aria-label")}${node.getAttribute("aria-selected") === "true" ? "*" : ""}`);
  expect(button("뒤로").disabled).toBe(true);
  expect(button("상위 폴더").disabled).toBe(true);
  expect(fileRow(host, "a/").querySelector(".file-name")?.textContent).toBe("a");

  await dblclick(fileRow(host, "a/"));
  expect(here()).toBe("a/");
  expect(fileRow(host, "a/x.txt").querySelector(".file-name")?.textContent).toBe("x.txt");
  expect(tree()).toEqual(["1:nav-bucket", "2:a*", "3:b", "2:z"]);

  const folder = fileRow(host, "a/b/");
  await act(async () => folder.focus());
  await keyDown(folder, "Enter");
  expect(here()).toBe("a/b/");
  await click("뒤로");
  expect(here()).toBe("a/");
  await click("앞으로");
  expect(here()).toBe("a/b/");
  expect(button("앞으로").disabled).toBe(true);
  await click("상위 폴더");
  expect(here()).toBe("a/");
  await act(async () => control(host, "z").click());
  expect(here()).toBe("z/");
  expect(host.querySelector('[role="status"]')?.textContent).toBe("빈 폴더");
  await keyDown(host.querySelector<HTMLElement>(".s3-explorer")!, "ArrowLeft", { altKey: true });
  expect(here()).toBe("a/");
  await act(async () => control(host, "nav-bucket", '[aria-label="위치"] button').click());
  expect(here()).toBe("nav-bucket");
  expect(button("상위 폴더").disabled).toBe(true);
  const root = fileRow(host, "root.txt");
  await keyDown(root, "Backspace");
  expect(prefixes()).toEqual(["", "a/", "a/b/", "a/", "a/b/", "a/", "z/", "a/", ""]);
});
