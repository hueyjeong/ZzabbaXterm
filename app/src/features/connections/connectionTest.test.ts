import { describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { createLocalConnectionIpc } from "./connectionIpc";
import type { ConnectionClose, ConnectionTestRun, SavedTarget } from "./profiles";
import { createSessionEditor } from "./sessionEditorState";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
const ipc = vi.mocked(invoke);

const local: SavedTarget = {
  id: "work-shell",
  groupId: "work",
  displayName: "작업 셸",
  defaultMethod: "local",
  local: { shell: "powerShell", startDirectory: "C:\\work" },
};
const draft = { version: 2, targetRevision: 3, target: local } as const;
const s3: SavedTarget = {
  id: "bucket", groupId: "work", displayName: "버킷", defaultMethod: "s3",
  s3: { profile: "company", region: "ap-northeast-2", bucket: "company-dev", prefix: "" },
};
const node = { profile: "company-sso", region: "ap-northeast-2", instanceId: "i-0123456789abcdef0" };

function nativePeer() {
  ipc.mockReset();
  return { ...createLocalConnectionIpc(), onClose: vi.fn<(result: ConnectionClose) => void>() };
}

function open(peer: ReturnType<typeof nativePeer>, target = local, revision = 3) {
  return createSessionEditor({ target, revision }, peer, { draftId: "work-draft", shell: "powerShell" });
}

const firstRun = { draftId: "work-draft", draftVersion: 0, runId: "rust-1" };
const secondRun = { draftId: "work-draft", draftVersion: 1, runId: "rust-2" };
const calls = (name: string) => ipc.mock.calls.filter(([command]) => command === name);

describe("연결 시험 실행과 취소", () => {
  it("같은 편집 창에서 늦은 이전 wait는 새 초안을 티켓 저장 가능하게 만들지 않는다", async () => {
    const peer = nativePeer();
    const firstReply = Promise.withResolvers<ConnectionTestRun & { sessionId: string }>();
    const secondReply = Promise.withResolvers<ConnectionTestRun & { sessionId: string }>();
    ipc.mockImplementation(async (command, args) => {
      if (command === "connection_test_start") return args && "draftVersion" in args && args.draftVersion === 0 ? firstRun : secondRun;
      if (command === "connection_test_wait") return args && "run" in args && (args.run as ConnectionTestRun).runId === "rust-1"
        ? firstReply.promise : secondReply.promise;
      return null;
    });
    const editor = open(peer);
    const oldTest = editor.test();
    await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith("connection_test_wait", { run: firstRun }));
    editor.editLocal("C:\\selected");
    const newTest = editor.test();
    firstReply.resolve({ ...firstRun, sessionId: "old-pty" });
    await oldTest;
    await editor.save();
    expect(calls("connection_save")).toEqual([]);
    expect(calls("connection_target_save")).toEqual([]);

    secondReply.resolve({ ...secondRun, sessionId: "new-pty" });
    await newTest;
    await editor.save();
    const edited = { ...draft, target: { ...local, local: { shell: "powerShell", startDirectory: "C:\\selected" } } };
    expect(calls("connection_test_cancel")).toEqual([["connection_test_cancel", { run: firstRun }]]);
    expect(ipc).toHaveBeenCalledWith("connection_test_start", { draftId: "work-draft", draftVersion: 1, draft: edited });
    expect(calls("connection_save")).toEqual([["connection_save", { request: { run: secondRun, draft: edited } }]]);
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "saved", draftId: "work-draft", targetId: "work-shell" });
  });

  it("시작 응답 전 빠르게 취소하면 도착한 Rust 실행만 회수하고 wait를 보내지 않는다", async () => {
    const peer = nativePeer();
    const start = Promise.withResolvers<ConnectionTestRun>();
    ipc.mockReturnValueOnce(start.promise).mockResolvedValue(null);
    const editor = open(peer);
    const testing = editor.test();
    const closing = editor.close();
    expect(ipc.mock.calls).toEqual([["connection_test_start", { draftId: "work-draft", draftVersion: 0, draft }]]);
    start.resolve(firstRun);
    await Promise.all([testing, closing]);
    await editor.save();
    expect(ipc.mock.calls.slice(1)).toEqual([["connection_test_cancel", { run: firstRun }]]);
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "cancelled", draftId: "work-draft" });
  });

  it("시험 시간 초과를 표시하고 취소 응답을 기다리지 않고 닫을 수 있다", async () => {
    vi.useFakeTimers();
    try {
      const peer = nativePeer();
      const cancel = Promise.withResolvers<null>();
      const error = "연결 시험 시간이 초과되었습니다";
      ipc.mockResolvedValueOnce(firstRun).mockRejectedValueOnce(error).mockReturnValueOnce(cancel.promise);
      const editor = open(peer);
      await editor.test();
      expect(editor.getSnapshot().status).toEqual({ kind: "testFailed", error });
      const closing = editor.close();
      await vi.advanceTimersByTimeAsync(2_000);
      await closing;
      cancel.resolve(null);
      expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "cancelled", draftId: "work-draft" });
      expect(ipc.mock.calls.slice(2)).toEqual([["connection_test_cancel", { run: firstRun }]]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("수정 시 성공 실행을 폐기하고 새 경로의 실제 실패와 입력을 보존한다", async () => {
    const peer = nativePeer();
    ipc.mockResolvedValueOnce(firstRun).mockResolvedValueOnce({ ...firstRun, sessionId: "first-pty" });
    const editor = open(peer);
    await editor.test();
    editor.editLocal("C:\\missing");
    await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith("connection_test_cancel", { run: firstRun }));
    const error = "지정된 경로를 찾을 수 없습니다 (os error 3)";
    ipc.mockResolvedValueOnce(secondRun).mockRejectedValueOnce(error).mockResolvedValue(null);
    await editor.test();

    expect(editor.getSnapshot()).toMatchObject({
      status: { kind: "testFailed", error },
      draft: { local: { startDirectory: "C:\\missing" } },
    });
    expect(calls("connection_test_cancel")).toEqual([
      ["connection_test_cancel", { run: firstRun }],
      ["connection_test_cancel", { run: secondRun }],
    ]);
    // 실패한 시험은 티켓을 만들지 않아 저장은 입력 검증 경로로만 간다.
    await editor.save();
    expect(calls("connection_save")).toEqual([]);
    expect(calls("connection_target_save")).toEqual([["connection_target_save", {
      draft: { ...draft, target: { ...local, local: { shell: "powerShell", startDirectory: "C:\\missing" } } },
    }]]);
  });

  it("Rust가 시험 시작을 거절하면 실제 오류를 남기고 존재하지 않는 실행의 대기나 회수를 보내지 않는다", async () => {
    const peer = nativePeer();
    const error = "시작 디렉터리는 절대 경로여야 합니다";
    ipc.mockRejectedValueOnce(error);
    const editor = open(peer);
    await editor.test();
    expect(editor.getSnapshot()).toMatchObject({ draft: local, status: { kind: "testFailed", error } });
    await editor.close();
    expect(ipc.mock.calls).toEqual([["connection_test_start", { draftId: "work-draft", draftVersion: 0, draft }]]);
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "cancelled", draftId: "work-draft" });
  });

  it("저장 오류를 보존하고 사용자가 취소하면 성공했던 실행을 회수한다", async () => {
    const peer = nativePeer();
    const error = "설정 파일 교체 실패: 접근이 거부되었습니다";
    ipc.mockResolvedValueOnce(firstRun).mockResolvedValueOnce({ ...firstRun, sessionId: "first-pty" })
      .mockRejectedValueOnce(error).mockResolvedValue(null);
    const editor = open(peer);
    await editor.test();
    await editor.save();
    expect(editor.getSnapshot()).toMatchObject({ draft: local, status: { kind: "saveFailed", error } });
    expect(peer.onClose).not.toHaveBeenCalled();
    await editor.close();
    expect(ipc.mock.calls.slice(2)).toEqual([
      ["connection_save", { request: { run: firstRun, draft } }],
      ["connection_test_cancel", { run: firstRun }],
    ]);
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "cancelled", draftId: "work-draft" });
  });

  it("다른 실행의 wait 응답과 검증하지 않은 티켓 저장은 IPC 저장으로 이어지지 않는다", async () => {
    const peer = nativePeer();
    ipc.mockResolvedValueOnce(firstRun).mockResolvedValueOnce({ ...firstRun, runId: "other-run", sessionId: "other-pty" })
      .mockResolvedValue(null);
    const editor = open(peer);
    await editor.test();
    expect(editor.getSnapshot().status).toEqual({ kind: "testFailed", error: "시험 결과가 요청한 실행과 일치하지 않습니다" });
    await expect(peer.save({ run: firstRun, draft })).rejects.toThrow("현재 초안의 연결 테스트 성공이 필요합니다");
    expect(ipc.mock.calls.slice(2)).toEqual([["connection_test_cancel", { run: firstRun }]]);
  });
});

describe("S3 연결 시험", () => {
  it("S3 시험 응답의 PTY 세션 ID로는 티켓 저장을 승인할 수 없다", async () => {
    const peer = nativePeer();
    ipc.mockResolvedValueOnce(firstRun).mockResolvedValueOnce({ ...firstRun, sessionId: "unexpected-pty" }).mockResolvedValue(null);
    const editor = open(peer, s3);
    await editor.test();
    expect(editor.getSnapshot().status).toEqual({ kind: "testFailed", error: "시험 결과가 요청한 실행과 일치하지 않습니다" });
    expect(calls("connection_save")).toEqual([]);
  });

  it("기존 S3 세션의 늦은 시험 결과는 편집 뒤 티켓 저장 권한을 주지 않는다", async () => {
    const peer = nativePeer();
    const previousWait = Promise.withResolvers<ConnectionTestRun & { sessionId: null }>();
    ipc.mockImplementation(async (command, args) => {
      if (command === "connection_test_start") return args && "draftVersion" in args && args.draftVersion === 0 ? firstRun : secondRun;
      if (command === "connection_test_wait") return args && "run" in args && (args.run as ConnectionTestRun).runId === firstRun.runId
        ? previousWait.promise : { ...secondRun, sessionId: null };
      if (command === "connection_test_cancel" || command === "connection_save") return null;
      throw new Error(`예상하지 못한 IPC: ${command}`);
    });
    const editor = open(peer, s3, 4);
    const previousTest = editor.test();
    await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith("connection_test_wait", { run: firstRun }));
    const prefix = " 한글// ";
    editor.editS3("prefix", prefix);
    previousWait.resolve({ ...firstRun, sessionId: null });
    await previousTest;
    expect(editor.getSnapshot().status).toEqual({ kind: "idle" });

    await editor.test();
    await editor.save();
    expect(calls("connection_save")).toEqual([["connection_save", { request: {
      run: secondRun, draft: { version: 2, targetRevision: 4, target: { ...s3, s3: { ...s3.s3, prefix } } },
    } }]]);
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "saved", draftId: "work-draft", targetId: "bucket" });
  });
});

describe("SSM 시험 원격 정리", () => {
  it("SSM 시험 취소를 확인하지 못하면 노드를 표시하고 종료 다시 확인으로 회수한다", async () => {
    const peer = nativePeer();
    const ssm: SavedTarget = { id: "node", displayName: "노드", defaultMethod: "ssm", ssm: node };
    let cancelReply: unknown = "busy";
    ipc.mockImplementation(async (command) => {
      if (command === "connection_test_start") return firstRun;
      if (command === "connection_test_wait") return new Promise(() => { });
      if (command === "connection_test_cancel") return cancelReply;
      throw new Error(`예상하지 못한 IPC: ${command}`);
    });
    const editor = open(peer, ssm, 1);
    void editor.test();
    await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith("connection_test_wait", { run: firstRun }));
    editor.selectSsm({ ...node, instanceId: "i-0fedcba9876543210" });
    expect(editor.getSnapshot().remoteCleanup).toEqual({ kind: "cancelling", target: node });
    await vi.waitFor(() => expect(editor.getSnapshot().remoteCleanup).toEqual({ kind: "unconfirmed", target: node }));
    // 원격 정리가 남아 있으면 새 시험을 시작하지 않는다.
    expect(editor.canTest()).toBe(false);
    cancelReply = null;
    editor.retryCleanup();
    await vi.waitFor(() => expect(editor.getSnapshot().remoteCleanup).toBeUndefined());
    expect(calls("connection_test_cancel")).toEqual([
      ["connection_test_cancel", { run: firstRun }],
      ["connection_test_cancel", { run: firstRun }],
    ]);
    expect(editor.canTest()).toBe(true);
    await editor.close();
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "cancelled", draftId: "work-draft" });
  });

  it("닫을 때 SSM 시험 종료를 확인하지 못하면 창을 닫지 않는다", async () => {
    const peer = nativePeer();
    const ssm: SavedTarget = { id: "node", displayName: "노드", defaultMethod: "ssm", ssm: node };
    ipc.mockImplementation(async (command) => {
      if (command === "connection_test_start") return firstRun;
      if (command === "connection_test_wait") return new Promise(() => { });
      if (command === "connection_test_cancel") return "busy";
      throw new Error(`예상하지 못한 IPC: ${command}`);
    });
    const editor = open(peer, ssm, 1);
    void editor.test();
    await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith("connection_test_wait", { run: firstRun }));
    await editor.close();
    expect(editor.getSnapshot()).toMatchObject({
      status: { kind: "closeFailed", error: "종료 확인 불가" },
      remoteCleanup: { kind: "unconfirmed", target: node },
    });
    expect(peer.onClose).not.toHaveBeenCalled();
  });
});
