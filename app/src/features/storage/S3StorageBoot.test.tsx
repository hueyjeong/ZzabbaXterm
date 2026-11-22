// @vitest-environment jsdom
import { act, createRef, StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import type { S3Location, StorageEditorOpen } from "./s3";
import { StorageTab } from "./StorageTab";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

it("고정된 저장 ID를 한 번 열고 Rust가 반환한 위치를 숨김 전환 뒤에도 유지한다", async () => {
  const location: S3Location = { bucket: "approved-bucket", prefix: " 한글// ", region: "eu-west-1" };
  const targetId = "registered-s3";
  invoke.mockImplementation(async (command: string, { request }: { request: { id?: string } }) => {
    switch (command) {
      case "storage_open":
        return { ok: true, value: { sessionId: request.id, profile: "company", region: location.region, endpoint: "https://s3.eu-west-1.amazonaws.com", location } };
      case "storage_list_objects": return { ok: true, value: { rows: [], nextToken: null } };
      case "storage_count_previous_files": return { ok: true, value: 0 };
      case "storage_close": return true;
      default: throw new Error(command);
    }
  });
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const closeRef = createRef<{ confirmClose(): Promise<void> }>();
  const render = (active: boolean) => act(async () => root.render(<StrictMode>
    <div hidden={!active}><StorageTab targetId={targetId} active={active} closing={false} closeRef={closeRef} onRetest={vi.fn()} /></div>
  </StrictMode>));
  try {
    await render(true);
    const sessionId = invoke.mock.calls[0]?.[1].request.id;
    expect(sessionId).toEqual(expect.any(String));
    expect(invoke.mock.calls.filter(([command]) => command === "storage_open")).toEqual([
      ["storage_open", { request: { targetId, id: sessionId } }],
    ]);
    expect(invoke).toHaveBeenCalledWith("storage_list_objects", { request: { sessionId, ...location, token: null } });
    expect(host.querySelector<HTMLInputElement>("label:nth-child(2) input")?.value).toBe(location.prefix);
    await render(false);
    await render(true);
    expect(invoke.mock.calls.filter(([command]) => command === "storage_open" || command === "storage_list_objects")).toHaveLength(2);
    expect(invoke.mock.calls.some(([command]) => command === "storage_close")).toBe(false);
    await act(async () => closeRef.current?.confirmClose());
    expect(invoke).toHaveBeenCalledWith("storage_close", { request: { sessionId } });
  } finally {
    await act(async () => root.unmount());
    host.remove();
    vi.unstubAllGlobals();
  }
});

it.each(["활성 세션", "개설 중", "닫기 중"] as const)("%s 재로그인의 대상과 후속 요청을 유지한다", async mode => {
  const location = { bucket: "approved-bucket", prefix: " 한글// ", region: "eu-west-1" };
  const targetId = "registered-s3";
  const expired = { ok: false, error: { stage: "identity", failure: { kind: "ssoLoginRequired" }, bytes: 0 } };
  const success = { ok: true, value: null };
  const login = Promise.withResolvers<typeof success>();
  let opens = 0;
  let lists = 0;
  let logins = 0;
  invoke.mockClear();
  invoke.mockImplementation(async (command: string, { request }: { request: { id: string } }) => {
    switch (command) {
      case "storage_open":
        if (++opens === 1 && mode === "개설 중") return expired;
        return { ok: true, value: { sessionId: request.id, profile: "company", region: location.region, endpoint: "https://s3.eu-west-1.amazonaws.com", location } };
      case "storage_list_objects":
        ++lists;
        if ((mode === "활성 세션" && lists === 2) || (mode === "닫기 중" && lists === 1)) return expired;
        return { ok: true, value: { rows: lists === 1 && mode === "활성 세션"
          ? [{ kind: "object", key: "기존 편집.txt", rowId: "old-row", bytes: 1 }]
          : [{ kind: "object", key: "새 목록.txt", rowId: "new-row", bytes: 1 }], nextToken: null } };
      case "storage_edit_open": return { handle: "editor-handle", label: "기존 편집.txt", text: "원문" };
      case "storage_edit_save": return { kind: "saved" };
      case "storage_relogin":
        if (mode === "닫기 중") return login.promise;
        return ++logins === 1 ? { ok: false, error: { stage: "profile", failure: { kind: "loginFailed" }, bytes: 0 } } : success;
      case "storage_count_previous_files": return { ok: true, value: 0 };
      case "storage_close": return true;
      default: throw new Error(command);
    }
  });
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const closeRef = createRef<{ confirmClose(): Promise<void> }>();
  const onEditFile = vi.fn<(opened: StorageEditorOpen) => void>();
  const button = (label: string) => [...host.querySelectorAll("button")].find(node => node.textContent === label);
  async function press(label: string) {
    const node = button(label);
    if (!node) throw new Error(`버튼 없음: ${label}`);
    await act(async () => node.click());
  }
  try {
    await act(async () => root.render(<StorageTab targetId={targetId} closing={false} closeRef={closeRef} onRetest={vi.fn()} onEditFile={onEditFile} />));
    const sessionId = invoke.mock.calls[0]?.[1].request.id;
    if (mode === "활성 세션") {
      await act(async () => button("기존 편집.txt")?.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2 })));
      await press("편집");
      await press("버킷 열기");
    }
    await press("SSO 재로그인");
    if (mode === "닫기 중") {
      await act(async () => closeRef.current?.confirmClose());
      await act(async () => login.resolve(success));
      expect(lists).toBe(1);
      expect(invoke).toHaveBeenCalledWith("storage_close", { request: { sessionId } });
      return;
    }
    expect(host.textContent).toContain("SSO 재로그인 실패");
    expect(opens).toBe(1);
    expect(lists).toBe(mode === "개설 중" ? 0 : 2);
    await press("SSO 재로그인");
    expect(invoke.mock.calls.filter(([command]) => command === "storage_relogin")).toEqual([
      ["storage_relogin", { request: { id: sessionId } }], ["storage_relogin", { request: { id: sessionId } }],
    ]);
    expect(button("SSO 재로그인")).toBeUndefined();
    expect(opens).toBe(1);
    if (mode === "개설 중") {
      await press("스토리지 연결");
      const reopen = invoke.mock.calls.filter(([command]) => command === "storage_open")[1]?.[1].request;
      expect(reopen).toEqual({ targetId, id: expect.any(String) });
      expect(reopen.id).not.toBe(sessionId);
    } else {
      expect(invoke.mock.calls.filter(([command]) => command === "storage_list_objects")).toEqual([
        ["storage_list_objects", { request: { sessionId, ...location, token: null } }],
        ["storage_list_objects", { request: { sessionId, ...location, token: null } }],
        ["storage_list_objects", { request: { sessionId, ...location, token: null } }],
      ]);
      expect(invoke).toHaveBeenCalledWith("storage_count_previous_files", { request: { sessionId, ...location } });
      const opened = onEditFile.mock.calls[0]?.[0];
      expect(opened?.file.label).toBe("기존 편집.txt");
      expect(await opened?.transport.save(opened.file.handle, "편집 내용")).toEqual({ kind: "saved" });
      expect(invoke).toHaveBeenCalledWith("storage_edit_save", { request: { sessionId, handle: "editor-handle", text: "편집 내용" } });
      await act(async () => button("새 목록.txt")?.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2 })));
      await press("편집");
      expect(invoke).toHaveBeenCalledWith("storage_edit_open", { request: { sessionId, rowId: "new-row" } });
    }
    expect(host.textContent).toContain("새 목록.txt");
    expect(host.querySelector('[role="alert"]')).toBeNull();
    expect(invoke.mock.calls.some(([command]) => command === "storage_close")).toBe(false);
  } finally {
    await act(async () => root.unmount());
    host.remove();
    vi.unstubAllGlobals();
  }
});
