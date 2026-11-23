import { describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { createLocalConnectionIpc } from "./connectionIpc";
import { createLocalConnectionWizard, type ConnectionClose, type ConnectionTestRun, type InitialWizardDraft, type LocalConnectionWizardState, type LocalProfileDraft } from "./profiles";

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

  it("시험 시간 초과를 표시하고 취소 응답을 기다리지 않고 닫을 수 있다", async () => {
    vi.useFakeTimers();
    try {
      const peer = nativePeer();
      const cancel = Promise.withResolvers<null>();
      const error = "연결 시험 시간이 초과되었습니다";
      ipc.mockResolvedValueOnce(firstRun).mockRejectedValueOnce(error).mockReturnValueOnce(cancel.promise);
      const wizard = createLocalConnectionWizard("work-draft", draft);
      finishSteps(wizard);
      await wizard.test(peer);
      expect(wizard.getSnapshot().status).toEqual({ kind: "testFailed", error });
      const closing = wizard.close(peer);
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

describe("F01 SSH 마법사", () => {
  it("모달 취소 뒤 도착한 네이티브 확인을 커밋하지 않는다", async () => {
    const result = Promise.withResolvers<unknown>();
    const peer = nativePeer();
    ipc.mockReturnValueOnce(result.promise).mockResolvedValueOnce(null);
    const controller = new AbortController();
    const attempt = { draftId: "draft", draftVersion: 2, attemptId: "1" };
    const request = { targetId: "server", host: "dev.example.invalid", port: 2222, hostKeyFingerprint: `SHA256:${"A".repeat(43)}` };
    const verification = peer.verifySshHost!(request, draft, attempt, controller.signal);
    controller.abort();
    result.resolve({ ...request, fingerprint: request.hostKeyFingerprint, ticket: "a".repeat(32) });
    await expect(verification).rejects.toMatchObject({ name: "AbortError" });
    expect(ipc.mock.calls.map(([command]) => command)).toEqual(["verify_ssh_host", "cancel_ssh_host"]);
    expect(ipc).toHaveBeenLastCalledWith("cancel_ssh_host", { attempt });
  });

  it("이전 호스트 키 확인과 늦은 SSM 선택으로 직접 SSH 시험을 승인할 수 없다", async () => {
    const first = Promise.withResolvers<void>();
    const verifySshHost = vi.fn().mockReturnValueOnce(first.promise).mockResolvedValueOnce(undefined);
    const peer = { ...nativePeer(), verifySshHost };
    const wizard = createLocalConnectionWizard("work-draft", draft);
    const ssm = { profile: "company", region: "ap-northeast-2", instanceId: "i-0123456789abcdef0" };
    const host = {
      host: "ssh.actual.test", port: "2222", account: "deploy",
      hostKeyFingerprint: `SHA256:${"A".repeat(43)}`,
    };
    wizard.go(1);
    wizard.selectMethod("ssh");
    wizard.editSsh({ ...host, transport: { kind: "ssm" } });
    wizard.go(1);
    wizard.editSsh({ auth: { kind: "storedPassword", itemId: "vault-item" } });
    wizard.go(1, null);
    expect(wizard.getSnapshot().step).toBe("auth");
    wizard.go(1, ssm);
    await wizard.test(peer);
    expect(ipc).not.toHaveBeenCalled();
    const oldApproval = wizard.verifySshHost(peer);
    wizard.go(-1);
    wizard.go(-1);
    wizard.editSsh({ host: "second.actual.test", transport: { kind: "direct" } });
    wizard.go(1);
    wizard.editSsh({ auth: { kind: "manualPassword" } });
    wizard.go(1, ssm);
    first.resolve();
    await oldApproval;
    await wizard.test(peer);
    await wizard.save(peer);
    expect(ipc).not.toHaveBeenCalled();
    await wizard.verifySshHost(peer);
    expect(verifySshHost.mock.calls).toEqual([
      [{ targetId: draft.target.id, host: host.host, port: 2222, hostKeyFingerprint: host.hostKeyFingerprint }, expect.any(Object),
      { draftId: "work-draft", draftVersion: expect.any(Number), attemptId: "1" }, expect.objectContaining({ aborted: true })],
      [{ targetId: draft.target.id, host: "second.actual.test", port: 2222, hostKeyFingerprint: host.hostKeyFingerprint }, expect.any(Object),
      { draftId: "work-draft", draftVersion: expect.any(Number), attemptId: "2" }, expect.objectContaining({ aborted: false })],
    ]);
    ipc.mockRejectedValueOnce("SSH 연결 시험을 실행할 수 없습니다");
    await wizard.test(peer);
    await wizard.save(peer);
    expect(ipc.mock.calls).toEqual([["connection_test_start", {
      draftId: "work-draft", draftVersion: expect.any(Number), draft: {
        ...draft, target: {
          id: draft.target.id, groupId: draft.target.groupId, displayName: draft.target.displayName,
          defaultMethod: "ssh", ssh: {
            host: "second.actual.test", port: 2222, account: "deploy", hostKeyFingerprint: host.hostKeyFingerprint,
            transport: { kind: "direct" }, auth: { kind: "manualPassword" },
          },
        },
      },
    }]]);
    expect(wizard.getSnapshot().status).toEqual({ kind: "testFailed", error: "SSH 연결 시험을 실행할 수 없습니다" });
  });

  it("확인한 저장 암호 대상은 연결 시험 없이 참조만 저장한다", async () => {
    const verifySshHost = vi.fn().mockResolvedValue(undefined);
    const peer = { ...nativePeer(), verifySshHost };
    const host = {
      host: "ssh.actual.test", port: "2222", account: "deploy",
      hostKeyFingerprint: `SHA256:${"A".repeat(43)}`,
    };
    const itemId = "21eb36c3-234d-439d-9e1f-1d3cc5ba8125";
    const wizard = createLocalConnectionWizard("stored-draft", draft);
    wizard.go(1);
    wizard.selectMethod("ssh");
    wizard.editSsh(host);
    wizard.go(1);
    wizard.editSsh({ auth: { kind: "storedPassword", itemId } });
    wizard.go(1, null);
    expect(wizard.getSnapshot().step).toBe("verify");
    await wizard.save(peer);
    expect(ipc).not.toHaveBeenCalled();
    await wizard.verifySshHost(peer);
    expect(wizard.getSnapshot().sshPin).toEqual({ kind: "verified" });
    await wizard.save(peer);
    expect(ipc).toHaveBeenCalledTimes(1);
    expect(ipc).toHaveBeenCalledWith("connection_save", { request: {
      run: { draftId: "stored-draft", draftVersion: expect.any(Number), runId: "stored-draft" },
      draft: {
        ...draft, group: { id: "work", displayName: "회사 개발" }, target: {
          id: draft.target.id, groupId: draft.target.groupId, displayName: draft.target.displayName,
          defaultMethod: "ssh", ssh: {
            host: host.host, port: 2222, account: host.account, hostKeyFingerprint: host.hostKeyFingerprint,
            transport: { kind: "direct" }, auth: { kind: "storedPassword", itemId },
          },
        },
      },
    } });
    expect(wizard.getSnapshot().status).toEqual({ kind: "closed" });
  });
});

describe("저장 S3 연결", () => {
  const s3 = { profile: "company", region: "ap-northeast-2", bucket: "company-dev", prefix: "" };

  it("신규 S3 연결에는 프로필, 버킷 리전과 버킷이 필요하며 빈 접두어는 허용한다", () => {
    const wizard = createLocalConnectionWizard("work-draft", draft);
    wizard.go(1);
    wizard.selectMethod("s3");
    wizard.go(1);
    wizard.go(1);
    expect(wizard.getSnapshot().step).toBe("auth");
    wizard.editS3("profile", s3.profile);
    wizard.editS3("region", s3.region);
    wizard.editS3("bucket", s3.bucket);
    wizard.go(1);
    const { step, draft: { target } } = wizard.getSnapshot();
    expect(step).toBe("verify");
    expect(target).toEqual({ id: draft.target.id, groupId: draft.target.groupId, displayName: draft.target.displayName, defaultMethod: "s3", s3 });
  });

  it("S3 시험 응답의 PTY 세션 ID로는 저장을 승인할 수 없다", async () => {
    const peer = nativePeer();
    const wizard = createLocalConnectionWizard("work-draft", {
      ...draft,
      target: { id: draft.target.id, groupId: draft.target.groupId, displayName: draft.target.displayName, defaultMethod: "s3", s3 },
    });
    ipc.mockResolvedValueOnce(firstRun).mockResolvedValueOnce({ ...firstRun, sessionId: "unexpected-pty" }).mockResolvedValueOnce(null);
    finishSteps(wizard);
    await wizard.test(peer);
    expect(wizard.getSnapshot().status).toEqual({ kind: "testFailed", error: "시험 결과가 요청한 실행과 일치하지 않습니다" });
    await wizard.save(peer);
    expect(ipc).not.toHaveBeenCalledWith("connection_save", expect.anything());
  });

  it("기존 S3 연결의 늦은 시험 결과는 편집 후 저장 권한을 주지 않는다", async () => {
    const peer = nativePeer();
    const initial: InitialWizardDraft = {
      ...draft, groupRevision: 2, targetRevision: 3,
      target: { id: draft.target.id, groupId: draft.target.groupId, displayName: draft.target.displayName, defaultMethod: "s3", s3 },
    };
    const previousWait = Promise.withResolvers<ConnectionTestRun & { sessionId: null }>();
    ipc.mockImplementation(async (command, args) => {
      if (command === "connection_test_start") return args && "draftVersion" in args && args.draftVersion === 0 ? firstRun : secondRun;
      if (command === "connection_test_wait") return args && "run" in args && (args.run as ConnectionTestRun).runId === firstRun.runId
        ? previousWait.promise : { ...secondRun, sessionId: null };
      if (command === "connection_test_cancel" || command === "connection_save") return null;
      throw new Error(`예상하지 못한 IPC: ${command}`);
    });

    const wizard = createLocalConnectionWizard("work-draft", initial);
    wizard.go(1);
    wizard.selectMethod("local");
    expect(wizard.getSnapshot().draft.target).toEqual(initial.target);
    wizard.go(1);
    wizard.go(1);
    const previousTest = wizard.test(peer);
    await vi.waitFor(() => expect(ipc).toHaveBeenCalledWith("connection_test_wait", { run: firstRun }));
    wizard.go(-1);
    const prefix = " 한글// ";
    wizard.editS3("prefix", prefix);
    wizard.go(1);
    previousWait.resolve({ ...firstRun, sessionId: null });
    await previousTest;
    await wizard.save(peer);
    expect(ipc).not.toHaveBeenCalledWith("connection_save", expect.anything());

    await wizard.test(peer);
    await wizard.save(peer);
    expect(ipc).toHaveBeenCalledWith("connection_save", { request: {
      run: secondRun, draft: { ...initial, target: { ...initial.target, s3: { ...s3, prefix } } },
    } });
    expect(peer.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "saved", draftId: "work-draft", targetId: draft.target.id });
  });
});
