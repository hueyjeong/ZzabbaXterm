import { describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { createLocalConnectionIpc } from "./connectionIpc";
import { createLocalConnectionWizard, type ConnectionClose, type ConnectionTestRun, type LocalConnectionWizardState, type LocalProfileDraft } from "./profiles";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
const ipc = vi.mocked(invoke);

const draft: LocalProfileDraft = {
  version: 1,
  groupRevision: null,
  targetRevision: null,
  group: { id: "work", displayName: "회사 개발" },
  target: {
    id: "work-shell",
    groupId: "work",
    displayName: "작업 셸",
    defaultMethod: "local",
    local: { shell: "powerShell", startDirectory: "C:\\work" },
  },
};

function nativePeer() {
  ipc.mockReset();
  return { ...createLocalConnectionIpc(), onClose: vi.fn<(result: ConnectionClose) => void>() };
}

const firstRun = { draftId: "work-draft", draftVersion: 0, runId: "rust-1" };
const secondRun = { draftId: "work-draft", draftVersion: 1, runId: "rust-2" };

function finishSteps(wizard: LocalConnectionWizardState) {
  wizard.go(1);
  wizard.go(1);
  wizard.go(1);
}

describe("C01 로컬 연결 IPC", () => {
  it("같은 마법사에서 늦은 이전 wait는 새 초안을 저장 가능하게 만들지 않는다", async () => {
    const peer = nativePeer();
    const firstReply = Promise.withResolvers<ConnectionTestRun & { sessionId: string }>();
    const secondReply = Promise.withResolvers<ConnectionTestRun & { sessionId: string }>();
    ipc.mockImplementation(async (command, args) => {
      if (command === "connection_test_start") return args && "draftVersion" in args && args.draftVersion === 0 ? firstRun : secondRun;
      if (command === "connection_test_wait") return args && "run" in args && (args.run as ConnectionTestRun).runId === "rust-1"
        ? firstReply.promise : secondReply.promise;
      return null;
    });
    const wizard = createLocalConnectionWizard("work-draft", draft);
    finishSteps(wizard);
    const oldTest = wizard.test(peer);
    await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith("connection_test_wait", { run: firstRun }));
    wizard.go(-1);
    wizard.go(-1);
    wizard.edit("startDirectory", "C:\\selected");
    wizard.go(1);
    wizard.go(1);
    const newTest = wizard.test(peer);
    firstReply.resolve({ ...firstRun, sessionId: "old-pty" });
    await oldTest;
    await wizard.save(peer);
    expect(ipc).not.toHaveBeenCalledWith("connection_save", expect.anything());

    secondReply.resolve({ ...secondRun, sessionId: "new-pty" });
    await newTest;
    await wizard.save(peer);
    const edited = { ...draft, target: { ...draft.target, local: { shell: "powerShell", startDirectory: "C:\\selected" } } };
    expect(ipc.mock.calls.filter(([command]) => command === "connection_test_cancel")).toEqual([
      ["connection_test_cancel", { run: firstRun }],
    ]);
    expect(ipc).toHaveBeenCalledWith("connection_test_start", { draftId: "work-draft", draftVersion: 1, draft: edited });
    expect(ipc.mock.calls.filter(([command]) => command === "connection_save")).toEqual([
      ["connection_save", { request: { run: secondRun, draft: edited } }],
    ]);
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "saved", draftId: "work-draft", targetId: "work-shell" });
  });

  it("시작 응답 전 빠르게 취소하면 도착한 Rust 실행만 회수하고 wait를 보내지 않는다", async () => {
    const peer = nativePeer();
    const start = Promise.withResolvers<ConnectionTestRun>();
    ipc.mockReturnValueOnce(start.promise).mockResolvedValue(null);
    const wizard = createLocalConnectionWizard("work-draft", draft);
    finishSteps(wizard);
    const testing = wizard.test(peer);
    const closing = wizard.close(peer);
    expect(ipc.mock.calls).toEqual([["connection_test_start", { draftId: "work-draft", draftVersion: 0, draft }]]);
    start.resolve(firstRun);
    await Promise.all([testing, closing]);
    await wizard.save(peer);
    expect(ipc.mock.calls.slice(1)).toEqual([["connection_test_cancel", { run: firstRun }]]);
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "cancelled", draftId: "work-draft" });
  });

  it("수정 시 성공 실행을 폐기하고 새 경로의 실제 실패와 입력을 보존한다", async () => {
    const peer = nativePeer();
    ipc.mockResolvedValueOnce(firstRun).mockResolvedValueOnce({ ...firstRun, sessionId: "first-pty" });
    const wizard = createLocalConnectionWizard("work-draft", draft);
    finishSteps(wizard);
    await wizard.test(peer);
    wizard.go(-1);
    wizard.go(-1);
    wizard.edit("startDirectory", "C:\\missing");
    await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith("connection_test_cancel", { run: firstRun }));
    wizard.go(1);
    wizard.go(1);
    const error = "지정된 경로를 찾을 수 없습니다 (os error 3)";
    ipc.mockResolvedValueOnce(secondRun).mockRejectedValueOnce(error).mockResolvedValue(null);
    await wizard.test(peer);
    await wizard.save(peer);

    expect(wizard.getSnapshot()).toMatchObject({
      step: "verify",
      status: { kind: "testFailed", error },
      draft: { target: { local: { startDirectory: "C:\\missing" } } },
    });
    expect(ipc.mock.calls.filter(([command]) => command === "connection_test_cancel")).toEqual([
      ["connection_test_cancel", { run: firstRun }],
      ["connection_test_cancel", { run: secondRun }],
    ]);
    expect(ipc).not.toHaveBeenCalledWith("connection_save", expect.anything());
  });

  it("Rust가 시험 시작을 거절하면 실제 오류를 남기고 존재하지 않는 실행의 대기나 회수를 보내지 않는다", async () => {
    const peer = nativePeer();
    const error = "시작 디렉터리는 절대 경로여야 합니다";
    ipc.mockRejectedValueOnce(error);
    const wizard = createLocalConnectionWizard("work-draft", draft);
    finishSteps(wizard);
    await wizard.test(peer);
    expect(wizard.getSnapshot()).toMatchObject({ draft, status: { kind: "testFailed", error } });
    await wizard.save(peer);
    await wizard.close(peer);
    expect(ipc.mock.calls).toEqual([["connection_test_start", { draftId: "work-draft", draftVersion: 0, draft }]]);
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "cancelled", draftId: "work-draft" });
  });

  it("저장 오류를 보존하고 사용자가 취소하면 성공했던 실행을 회수한다", async () => {
    const peer = nativePeer();
    const error = "설정 파일 교체 실패: 접근이 거부되었습니다";
    ipc.mockResolvedValueOnce(firstRun).mockResolvedValueOnce({ ...firstRun, sessionId: "first-pty" })
      .mockRejectedValueOnce(error).mockResolvedValue(null);
    const wizard = createLocalConnectionWizard("work-draft", draft);
    finishSteps(wizard);
    await wizard.test(peer);
    await wizard.save(peer);
    expect(wizard.getSnapshot()).toMatchObject({ draft, status: { kind: "saveFailed", error } });
    expect(peer.onClose).not.toHaveBeenCalled();
    await wizard.close(peer);
    expect(ipc.mock.calls.slice(2)).toEqual([
      ["connection_save", { request: { run: firstRun, draft } }],
      ["connection_test_cancel", { run: firstRun }],
    ]);
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "cancelled", draftId: "work-draft" });
  });

  it("다른 실행의 wait 응답과 검증하지 않은 저장은 IPC 저장으로 이어지지 않는다", async () => {
    const peer = nativePeer();
    ipc.mockResolvedValueOnce(firstRun).mockResolvedValueOnce({ ...firstRun, runId: "other-run", sessionId: "other-pty" })
      .mockResolvedValue(null);
    const wizard = createLocalConnectionWizard("work-draft", draft);
    await wizard.test(peer);
    expect(ipc).not.toHaveBeenCalled();
    finishSteps(wizard);
    await wizard.test(peer);
    await wizard.save(peer);
    await expect(peer.save({ run: firstRun, draft })).rejects.toThrow("현재 초안의 연결 테스트 성공이 필요합니다");
    expect(ipc.mock.calls.slice(2)).toEqual([["connection_test_cancel", { run: firstRun }]]);
  });

  it("저장본 조회에서 그룹과 대상 개정을 그대로 받는다", async () => {
    const peer = nativePeer();
    const saved = { version: 1, groups: [{ profile: draft.group, revision: 2 }], targets: [{ profile: draft.target, revision: 3 }] };
    ipc.mockResolvedValueOnce(saved);
    await expect(peer.load()).resolves.toEqual(saved);
    expect(ipc).toHaveBeenCalledExactlyOnceWith("connections_load", {});
  });
});
