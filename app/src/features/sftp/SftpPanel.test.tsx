// @vitest-environment jsdom
import { invoke } from "@tauri-apps/api/core";
import { act, createRef, StrictMode, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SftpPanel, type SftpPanelActions } from "./SftpPanel";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
const ipc = vi.mocked(invoke);
const folder = { entryId: "dir-opaque", name: "설정", isDirectory: true };
const file = { entryId: "file-opaque", name: "한글.txt", isDirectory: false };
let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
  ipc.mockReset();
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.unstubAllGlobals();
});

function button(name: string) {
  const found = Array.from(host.querySelectorAll("button")).find(item => item.textContent === name);
  if (!found) throw new Error(`버튼 없음: ${name}`);
  return found;
}

it("SSH 세션에 한 번 붙고 불투명 ID로 이동하며 편집 메뉴에서만 파일을 연다", async () => {
  ipc.mockResolvedValueOnce({ entries: [folder], parentEntryId: null });
  const onEditFile = vi.fn<ComponentProps<typeof SftpPanel>["onEditFile"]>();
  await act(async () => root.render(<StrictMode><SftpPanel sshSessionId="ssh-1" onEditFile={onEditFile} /></StrictMode>));
  expect(ipc.mock.calls).toEqual([["editor_sftp_attach", { sessionId: "ssh-1" }]]);
  expect(host.textContent).toContain("지원 안 함");
  expect(host.textContent).not.toContain(folder.entryId);

  ipc.mockResolvedValueOnce({ entries: [file], parentEntryId: null });
  await act(async () => button(folder.name).click());
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_list", { sessionId: "ssh-1", entryId: folder.entryId });
  await act(async () => button(file.name).click());
  expect(onEditFile).not.toHaveBeenCalled();
  expect(button(file.name).getAttribute("aria-pressed")).toBe("true");

  ipc.mockResolvedValueOnce({ handle: "approved", name: file.name, text: "본문" });
  await act(async () => {
    button(file.name).dispatchEvent(new KeyboardEvent("keydown", { key: "F10", shiftKey: true, bubbles: true, cancelable: true }));
  });
  await act(async () => button("편집").click());
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_open", { sessionId: "ssh-1", entryId: file.entryId });
  expect(onEditFile).toHaveBeenCalledExactlyOnceWith({ handle: "approved", label: file.name, text: "본문" }, expect.any(Object), expect.any(Function));

  ipc.mockResolvedValueOnce({ entries: [folder], parentEntryId: null });
  await act(async () => button("상위 폴더").click());
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_list", { sessionId: "ssh-1", entryId: null });
  expect(onEditFile).toHaveBeenCalledTimes(1);

  const [approved, transport, close] = onEditFile.mock.calls[0];
  ipc.mockResolvedValueOnce({ status: "conflict", remote: { kind: "missing" }, cleanup: { kind: "removed" } });
  await expect(transport.save(approved.handle, "초안")).resolves.toMatchObject({ kind: "conflict", current: null });
  ipc.mockResolvedValueOnce({ status: "conflict", remote: { kind: "notUtf8" }, cleanup: { kind: "removed" } });
  await expect(transport.save(approved.handle, "초안")).resolves.toMatchObject({ kind: "conflict", current: undefined });
  ipc.mockRejectedValueOnce("IPC 응답 소실");
  await expect(transport.save(approved.handle, "초안")).resolves.toEqual({ kind: "unknown", message: "IPC 응답 소실" });
  await close();
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_close", { sessionId: "ssh-1", handle: "approved" });
});

it("SSH ID 교체와 종료 확인 뒤에 도착한 목록 응답을 무시한다", async () => {
  const late = Promise.withResolvers<unknown>();
  ipc.mockResolvedValueOnce({ entries: [folder], parentEntryId: null });
  const closeRef = createRef<SftpPanelActions>();
  const onEditFile = vi.fn();
  const render = (sshSessionId: string) => act(async () => root.render(<SftpPanel {...{ sshSessionId, closeRef, onEditFile }} />));
  await render("ssh-1");
  ipc.mockReturnValueOnce(late.promise);
  await act(async () => button(folder.name).click());
  ipc.mockImplementation(async (command) => command === "editor_sftp_attach" ? { entries: [], parentEntryId: null } : undefined);
  await render("ssh-2");
  await act(async () => { await closeRef.current?.confirmClose(); });
  await act(async () => late.resolve({ entries: [file], parentEntryId: null }));
  expect(host.textContent).not.toContain(file.name);
  expect(ipc.mock.calls.filter(([command]) => command === "editor_sftp_detach")).toEqual([
    ["editor_sftp_detach", { sessionId: "ssh-1" }], ["editor_sftp_detach", { sessionId: "ssh-2" }],
  ]);
});

it("연결 시작 응답을 받은 뒤에만 종료를 확인한다", async () => {
  const opening = Promise.withResolvers<unknown>();
  ipc.mockReturnValueOnce(opening.promise);
  const closeRef = createRef<SftpPanelActions>();
  await act(async () => root.render(<SftpPanel sshSessionId="ssh-1" closeRef={closeRef} onEditFile={vi.fn()} />));
  let close: Promise<void> | undefined;
  await act(async () => { close = closeRef.current?.confirmClose(); });
  expect(ipc.mock.calls).toEqual([["editor_sftp_attach", { sessionId: "ssh-1" }]]);
  await act(async () => {
    opening.resolve({ entries: [file], parentEntryId: null });
    await close;
  });
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_detach", { sessionId: "ssh-1" });
  expect(host.textContent).not.toContain(file.name);
});

it("진행 중인 폴더 응답을 종료 시작 때 무효화하고 종료 오류를 상위에 전달한다", async () => {
  const listing = Promise.withResolvers<unknown>();
  ipc.mockResolvedValueOnce({ entries: [folder], parentEntryId: null });
  const closeRef = createRef<SftpPanelActions>();
  await act(async () => root.render(<SftpPanel sshSessionId="ssh-1" closeRef={closeRef} onEditFile={vi.fn()} />));
  ipc.mockReturnValueOnce(listing.promise);
  await act(async () => button(folder.name).click());
  ipc.mockRejectedValueOnce({ kind: "timedOut" });
  await act(async () => {
    const actions = closeRef.current;
    if (!actions) throw new Error("SFTP 종료 참조 없음");
    await expect(actions.confirmClose()).rejects.toThrow("SFTP 응답 시간 초과");
  });
  await act(async () => listing.resolve({ entries: [file], parentEntryId: null }));
  expect(host.textContent).not.toContain(file.name);
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_detach", { sessionId: "ssh-1" });
});
