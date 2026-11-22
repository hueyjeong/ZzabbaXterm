// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ApprovedEditorFile } from "../editor/contract";
import { S3StoragePanel } from "./S3StoragePanel";
import type { S3Actions, StorageEditorOpen } from "./s3";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

const session = { sessionId: "first", profile: "dev", region: "ap-northeast-2", endpoint: "https://s3.ap-northeast-2.amazonaws.com" };
const key = "  실험//../한글 +%2F.txt  ";
const file = { handle: "edit-handle", label: key, text: "원문" } as ApprovedEditorFile;
const onEditFile = vi.fn<(opened: StorageEditorOpen) => void>();
const buckets = vi.fn<S3Actions["listBuckets"]>();
const download = vi.fn<S3Actions["download"]>();
function unused(): never { throw new Error("예상하지 않은 작업"); }
const actions: S3Actions = {
  listBuckets: buckets,
  listObjects: async () => ({ ok: true, value: { nextToken: null, rows: [
    { kind: "object", key: key.trim(), bytes: 2, rowId: "different-row" },
    { kind: "object", key, bytes: 1, rowId: "approved-row" },
    { kind: "object", key: "이전 목록 행", bytes: null },
  ] } }),
  selectLocalFile: async () => ({ kind: "approved", approvalId: "download" }),
  upload: unused,
  download,
};
const countHistory = async () => ({ ok: true, value: 0 } as const);
let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
  invoke.mockReset();
  onEditFile.mockClear();
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.unstubAllGlobals();
});

function render(editEnabled = true, currentSession = session) {
  return act(async () => root.render(<S3StoragePanel session={currentSession} actions={actions}
    countHistory={countHistory} onEditFile={onEditFile} editEnabled={editEnabled} />));
}

function button(text: string) {
  const node = Array.from(host.querySelectorAll("button")).find(node => node.textContent === text);
  if (!node) throw new Error(`버튼 없음: ${text}`);
  return node;
}

async function list() {
  await render();
  const input = host.querySelector("input");
  if (!input) throw new Error("버킷 입력란 없음");
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(input, "bucket");
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => button("버킷 열기").click());
}

function menu(name: string, keyboard = false) {
  return act(async () => {
    button(name).dispatchEvent(keyboard
      ? new KeyboardEvent("keydown", { key: "F10", shiftKey: true, bubbles: true, cancelable: true })
      : new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2 }));
  });
}

it("우클릭은 메뉴만 열고 편집을 고르면 정확한 원문 키의 승인 행을 연다", async () => {
  invoke.mockResolvedValueOnce(file).mockResolvedValueOnce(true);
  await list();
  await act(async () => button(key.trim()).click());
  await menu("이전 목록 행", true);
  expect(host.querySelector('[role="menu"]')?.textContent).toBe("선택");
  await menu(key);
  expect(invoke).not.toHaveBeenCalled();
  expect(onEditFile).not.toHaveBeenCalled();
  await act(async () => button("편집").click());
  expect(invoke).toHaveBeenCalledExactlyOnceWith("storage_edit_open", { request: { sessionId: "first", rowId: "approved-row" } });
  expect(onEditFile).toHaveBeenCalledExactlyOnceWith({ file, transport: expect.objectContaining({ save: expect.any(Function) }), close: expect.any(Function) });
  await onEditFile.mock.calls[0][0].close();
  expect(invoke).toHaveBeenLastCalledWith("storage_edit_close", { request: { sessionId: "first", handle: file.handle } });
});

it.each(["이동", "탭 전환", "세션 교체", "로그인 만료", "닫기"] as const)("%s 이후 편집 응답은 요청한 핸들만 닫는다", async reason => {
  const pending = Promise.withResolvers<ApprovedEditorFile>();
  invoke.mockReturnValueOnce(pending.promise).mockResolvedValueOnce(true);
  await list();
  await menu(key, true);
  await act(async () => button("편집").click());
  if (reason === "이동") {
    buckets.mockResolvedValueOnce({ ok: true, value: { rows: [], nextToken: null } });
    await act(async () => button("버킷 목록").click());
  } else if (reason === "로그인 만료") {
    download.mockResolvedValueOnce({ ok: false, error: { stage: "getObject", bytes: 0, failure: { kind: "ssoLoginRequired" } } });
    await act(async () => button(key).click());
    await act(async () => button("다운로드").click());
  } else if (reason === "탭 전환") {
    await render(false);
    await render();
  } else if (reason === "세션 교체") {
    await render(true, { ...session, sessionId: "second" });
  } else {
    await act(async () => root.render(null));
  }
  await act(async () => pending.resolve(file));
  expect(onEditFile).not.toHaveBeenCalled();
  expect(invoke.mock.calls).toEqual([
    ["storage_edit_open", { request: { sessionId: "first", rowId: "approved-row" } }],
    ["storage_edit_close", { request: { sessionId: "first", handle: file.handle } }],
  ]);
});

it("열기 실패를 객체 목록과 함께 표시한다", async () => {
  invoke.mockResolvedValueOnce({ kind: "denied", message: "편집 승인 만료" });
  await list();
  await menu(key);
  await act(async () => button("편집").click());
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("파일 열기 실패: 편집 승인 만료");
  expect(button(key)).toBeTruthy();
  expect(onEditFile).not.toHaveBeenCalled();
});
