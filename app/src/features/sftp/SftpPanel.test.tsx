// @vitest-environment jsdom
import { invoke } from "@tauri-apps/api/core";
import { act, createRef, StrictMode, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { control, dblclick, fileRow, keyDown, menuItem } from "../files/testing";
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


it("SSH 세션에 한 번 붙고 불투명 ID로 이동하며 편집 메뉴에서만 파일을 연다", async () => {
  ipc.mockResolvedValueOnce({ entries: [folder], parentEntryId: null });
  const onEditFile = vi.fn<ComponentProps<typeof SftpPanel>["onEditFile"]>();
  await act(async () => root.render(<StrictMode><SftpPanel sshSessionId="ssh-1" onEditFile={onEditFile} /></StrictMode>));
  expect(ipc.mock.calls).toEqual([["editor_sftp_attach", { sessionId: "ssh-1" }]]);
  expect(fileRow(host, folder.name).dataset.kind).toBe("folder");
  expect(host.innerHTML).not.toContain(folder.entryId);
  expect(control(host, "상위 폴더").hasAttribute("disabled")).toBe(true);

  ipc.mockResolvedValueOnce({ entries: [file], parentEntryId: null });
  await dblclick(fileRow(host, folder.name));
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_list", { sessionId: "ssh-1", entryId: folder.entryId });
  expect(host.querySelector('[aria-label="SFTP 경로"] [aria-current="location"]')?.textContent).toBe(folder.name);
  await act(async () => fileRow(host, file.name).click());
  expect(onEditFile).not.toHaveBeenCalled();
  expect(fileRow(host, file.name).getAttribute("aria-selected")).toBe("true");

  ipc.mockResolvedValueOnce({ handle: "approved", name: file.name, text: "본문" });
  await keyDown(fileRow(host, file.name), "F10", { shiftKey: true });
  await act(async () => menuItem(host, "편집").click());
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_open", { sessionId: "ssh-1", entryId: file.entryId });
  expect(onEditFile).toHaveBeenCalledExactlyOnceWith({ handle: "approved", label: file.name, text: "본문" }, expect.any(Object), expect.any(Function));

  ipc.mockResolvedValueOnce({ entries: [folder], parentEntryId: null });
  await act(async () => control(host, "상위 폴더").click());
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_list", { sessionId: "ssh-1", entryId: null });
  expect(onEditFile).toHaveBeenCalledTimes(1);

  const [approved, transport, close] = onEditFile.mock.calls[0];
  ipc.mockResolvedValueOnce({ status: "conflict", remote: { kind: "missing" }, cleanup: { kind: "removed" } });
  await expect(transport.save(approved.handle, "초안")).resolves.toMatchObject({ kind: "conflict", current: null });
  ipc.mockResolvedValueOnce({ status: "conflict", remote: { kind: "notUtf8" }, cleanup: { kind: "removed" } });
  await expect(transport.save(approved.handle, "초안")).resolves.toMatchObject({ kind: "conflict", current: undefined });
  ipc.mockRejectedValueOnce("IPC 응답 소실");
  await expect(transport.save(approved.handle, "초안")).resolves.toEqual({ kind: "unknown", message: "IPC 응답 소실" });
  ipc.mockResolvedValueOnce({ status: "unknown", error: { kind: "remoteFailure" }, recoveryPath: "/home/test/.zxt-edit-0123456789abcdef0123456789abcdef.tmp/original" });
  await expect(transport.save(approved.handle, "보존할 초안")).resolves.toEqual({
    kind: "unknown", message: "원격 작업 실패 · 복구 파일: /home/test/.zxt-edit-0123456789abcdef0123456789abcdef.tmp/original"
  });
  ipc.mockResolvedValueOnce({ status: "conflict", remote: { kind: "text", text: "외부 변경" } });
  await expect(transport.save(approved.handle, "보존할 초안")).resolves.toMatchObject({ kind: "conflict", current: "외부 변경" });
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
  await dblclick(fileRow(host, folder.name));
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
  await dblclick(fileRow(host, folder.name));
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

it("두 번 눌러 파일을 열고 경로 조각·새로고침·Backspace·시작 폴더로 이동한다", async () => {
  const sub = { entryId: "sub-opaque", name: "하위", isDirectory: true };
  ipc.mockResolvedValueOnce({ entries: [folder], parentEntryId: null });
  const onEditFile = vi.fn<ComponentProps<typeof SftpPanel>["onEditFile"]>();
  await act(async () => root.render(<SftpPanel sshSessionId="ssh-1" onEditFile={onEditFile} />));
  const crumbs = () => Array.from(host.querySelectorAll('[aria-label="SFTP 경로"] button')).map(node => node.getAttribute("aria-label") ?? node.textContent);

  ipc.mockResolvedValueOnce({ entries: [sub, file], parentEntryId: null });
  await dblclick(fileRow(host, folder.name));
  ipc.mockResolvedValueOnce({ entries: [file], parentEntryId: folder.entryId });
  await dblclick(fileRow(host, sub.name));
  expect(crumbs()).toEqual(["시작 폴더", folder.name, sub.name]);

  ipc.mockResolvedValueOnce({ entries: [sub, file], parentEntryId: null });
  await act(async () => control(host, folder.name, '[aria-label="SFTP 경로"] button').click());
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_list", { sessionId: "ssh-1", entryId: folder.entryId });
  expect(crumbs()).toEqual(["시작 폴더", folder.name]);

  ipc.mockResolvedValueOnce({ entries: [sub, file], parentEntryId: null });
  await act(async () => control(host, "새로고침").click());
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_list", { sessionId: "ssh-1", entryId: folder.entryId });

  ipc.mockResolvedValueOnce({ handle: "approved", name: file.name, text: "본문" });
  await dblclick(fileRow(host, file.name));
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_open", { sessionId: "ssh-1", entryId: file.entryId });
  expect(onEditFile).toHaveBeenCalledTimes(1);

  ipc.mockResolvedValueOnce({ entries: [folder], parentEntryId: null });
  await keyDown(fileRow(host, file.name), "Backspace");
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_list", { sessionId: "ssh-1", entryId: null });
  expect(crumbs()).toEqual(["시작 폴더"]);
  expect(control(host, "시작 폴더").hasAttribute("disabled")).toBe(true);

  ipc.mockResolvedValueOnce({ entries: [file], parentEntryId: null });
  await dblclick(fileRow(host, folder.name));
  ipc.mockResolvedValueOnce({ entries: [folder], parentEntryId: null });
  await act(async () => control(host, "시작 폴더").click());
  expect(ipc).toHaveBeenLastCalledWith("editor_sftp_list", { sessionId: "ssh-1", entryId: null });
});
