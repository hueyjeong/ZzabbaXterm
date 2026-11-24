import { describe, expect, it, vi } from "vitest";
import type { ConnectionRequest, ConnectionTestDraft, ConnectionTestRun, SavedTarget, TargetDraft } from "./profiles";
import { createSessionEditor, shownError, shownFields, testSupported, validate, type EditorActions, type EditorRequest } from "./sessionEditorState";

const local: SavedTarget = {
  id: "work-shell", groupId: "work", displayName: "작업 셸", defaultMethod: "local",
  local: { shell: "powerShell", startDirectory: "C:\\work" },
};
const node = { profile: "company-sso", region: "ap-northeast-2", instanceId: "i-0123456789abcdef0" };
const fingerprint = `SHA256:${"A".repeat(43)}`;

function fakeActions() {
  return {
    test: vi.fn(async (attempt: ConnectionTestDraft) => ({ draftId: attempt.draftId, draftVersion: attempt.draftVersion, runId: "run-1" })),
    cancelTest: vi.fn(async (_attempt: ConnectionTestDraft) => { }),
    save: vi.fn(async (_request: ConnectionRequest) => { }),
    saveTarget: vi.fn(async (_draft: TargetDraft) => ({ revision: 1 })),
    onClose: vi.fn(),
  } satisfies EditorActions;
}

function open(request: EditorRequest, actions = fakeActions(), storedAuth = false) {
  return { actions, editor: createSessionEditor(request, actions, { draftId: "draft", shell: "powerShell", storedAuth: () => storedAuth }) };
}

function fillSsh(editor: ReturnType<typeof open>["editor"]) {
  editor.setName("개발 서버");
  editor.selectMethod("ssh");
  editor.editSsh({ host: "ssh.actual.test", port: "2222", account: "deploy" });
  editor.editSsh({ auth: { kind: "manualPassword" } });
}

describe("세션 편집 창 상태", () => {
  it("검증만 통과하면 저장이 켜진다", () => {
    const { editor } = open({ groupId: "work" });
    expect(editor.getSnapshot().draft).toMatchObject({ groupId: "work", defaultMethod: "local", local: { shell: "powerShell", startDirectory: "" } });
    expect(editor.canSave()).toBe(false);
    editor.setName("작업 셸");
    expect(editor.canSave()).toBe(true);
    editor.selectMethod("ssh");
    expect(editor.canSave()).toBe(false);
    editor.editSsh({ host: "ssh.actual.test", account: "deploy", auth: { kind: "agent", publicKeyPath: "" } });
    expect(editor.canSave()).toBe(false);
    editor.editSsh({ auth: { kind: "agent", publicKeyPath: "C:\\keys\\id.pub" } });
    expect(editor.canSave()).toBe(true);
  });

  it("시험 없이 저장하면 saveTarget을 호출한다", async () => {
    const { editor, actions } = open({ groupId: "work" });
    editor.setName("작업 셸");
    editor.editLocal("C:\\work");
    await editor.save();
    expect(actions.save).not.toHaveBeenCalled();
    expect(actions.saveTarget).toHaveBeenCalledExactlyOnceWith({
      version: 2, targetRevision: null,
      target: { id: expect.any(String), groupId: "work", displayName: "작업 셸", defaultMethod: "local", local: { shell: "powerShell", startDirectory: "C:\\work" } },
    });
    expect(actions.onClose).toHaveBeenCalledExactlyOnceWith({ kind: "saved", draftId: "draft", targetId: editor.getSnapshot().draft.id });
  });

  it("복제는 같은 설정과 새 ID, \"이름 (복사본)\"으로 새 세션을 저장한다", async () => {
    const ssh: SavedTarget = {
      id: "dev", groupId: "work", displayName: "개발 서버", defaultMethod: "ssh",
      ssh: { host: "ssh.actual.test", port: 2222, account: "deploy", hostKeyFingerprint: fingerprint, transport: { kind: "direct" },
        auth: { kind: "keyFile", privateKeyPath: "C:\\keys\\id" } },
    };
    const { editor, actions } = open({ copy: ssh });
    expect(editor.targetRevision).toBeNull();
    expect(editor.getSnapshot().draft).toMatchObject({ groupId: "work", displayName: "개발 서버 (복사본)", ssh: { port: "2222" } });
    expect(editor.getSnapshot().draft.id).not.toBe("dev");
    expect(editor.canSave()).toBe(true);
    await editor.save();
    expect(actions.saveTarget).toHaveBeenCalledExactlyOnceWith({
      version: 2, targetRevision: null, target: { ...ssh, id: editor.getSnapshot().draft.id, displayName: "개발 서버 (복사본)" },
    });
  });

  it("시험에 성공한 뒤 저장하면 티켓 저장을 호출한다", async () => {
    const { editor, actions } = open({ target: local, revision: 3 });
    await editor.test();
    expect(editor.getSnapshot().status.kind).toBe("passed");
    await editor.save();
    const draft = { version: 2, targetRevision: 3, target: local };
    expect(actions.test).toHaveBeenCalledWith({ draftId: "draft", draftVersion: 0, draft }, expect.any(Function));
    expect(actions.save).toHaveBeenCalledExactlyOnceWith({ run: { draftId: "draft", draftVersion: 0, runId: "run-1" }, draft });
    expect(actions.saveTarget).not.toHaveBeenCalled();
    expect(actions.cancelTest).not.toHaveBeenCalled();
  });

  it("시험 뒤 필드를 바꾸면 티켓이 무효가 되고 실행이 취소된다", async () => {
    const { editor, actions } = open({ target: local, revision: 3 });
    await editor.test();
    const attempt = actions.test.mock.calls[0][0];
    editor.editLocal("C:\\other");
    expect(editor.getSnapshot().status).toEqual({ kind: "idle" });
    await vi.waitFor(() => expect(actions.cancelTest).toHaveBeenCalledExactlyOnceWith(attempt));
    await editor.save();
    expect(actions.save).not.toHaveBeenCalled();
    expect(actions.saveTarget).toHaveBeenCalledExactlyOnceWith({
      version: 2, targetRevision: 3, target: { ...local, local: { shell: "powerShell", startDirectory: "C:\\other" } },
    });
  });

  it("RDP와 VNC는 시험하지 않는다", async () => {
    for (const method of ["rdp", "vnc"] as const) {
      const { editor, actions } = open({});
      editor.setName("원격 화면");
      editor.selectMethod(method);
      editor.editDesktop({ host: "10.0.1.25" });
      expect(editor.canSave()).toBe(true);
      expect(editor.canTest()).toBe(false);
      await editor.test();
      expect(actions.test).not.toHaveBeenCalled();
    }
  });

  it("Rust의 invalid 오류를 해당 필드에 표시하고 입력을 바꾸면 지운다", async () => {
    const actions = fakeActions();
    actions.saveTarget.mockRejectedValueOnce({ kind: "invalid", field: "ssh.port" });
    const { editor } = open({}, actions);
    fillSsh(editor);
    await editor.save();
    const state = editor.getSnapshot();
    const errors = validate(state.draft, false);
    expect(state.status).toMatchObject({ kind: "saveFailed", field: "ssh.port" });
    expect(shownError(state, errors, "ssh.port")).toBe("포트 형식 오류");
    expect(shownError(state, errors, "ssh.host")).toBeUndefined();
    editor.editSsh({ port: "22" }, "ssh.port");
    expect(shownError(editor.getSnapshot(), errors, "ssh.port")).toBeUndefined();
  });

  it("저장 충돌 뒤에도 입력이 남는다", async () => {
    const actions = fakeActions();
    actions.saveTarget.mockRejectedValueOnce({ kind: "conflict" });
    const { editor } = open({ target: local, revision: 3 }, actions);
    editor.setName("바꾼 이름");
    await editor.save();
    expect(editor.getSnapshot()).toMatchObject({
      draft: { displayName: "바꾼 이름" },
      status: { kind: "saveFailed", error: "다른 작업이 세션을 변경했습니다" },
    });
    expect(actions.onClose).not.toHaveBeenCalled();
    actions.saveTarget.mockRejectedValueOnce({ kind: "failed", message: "설정 파일 교체 실패" });
    await editor.save();
    expect(editor.getSnapshot().status).toMatchObject({ kind: "saveFailed", error: "설정 파일 교체 실패" });
  });

  it("빈 필수 입력은 건드린 뒤에만, 형식 오류는 바로 표시한다", () => {
    const { editor } = open({});
    editor.selectMethod("ssh");
    editor.editSsh({ hostKeyFingerprint: "SHA256:short", port: "70000" });
    const shown = (field: string) => shownError(editor.getSnapshot(), validate(editor.getSnapshot().draft, false), field);
    expect(shown("ssh.host")).toBeUndefined();
    expect(shown("displayName")).toBeUndefined();
    expect(shown("ssh.hostKeyFingerprint")).toBe("지문 형식 오류");
    expect(shown("ssh.port")).toBe("포트 형식 오류");
    editor.touch("ssh.host");
    expect(shown("ssh.host")).toBe("필수 입력");
    editor.editSsh({ host: "ssh.actual.test", hostKeyFingerprint: "" }, "ssh.host");
    expect(shown("ssh.host")).toBeUndefined();
    expect(shown("ssh.hostKeyFingerprint")).toBeUndefined();
  });

  it("지문 없이 SSH 초안을 완성해 시험하고 지문 형식이 틀리면 저장과 시험을 막는다", async () => {
    const { editor, actions } = open({});
    fillSsh(editor);
    editor.editSsh({ hostKeyFingerprint: "SHA256:short" });
    expect(editor.canSave()).toBe(false);
    expect(editor.canTest()).toBe(false);
    editor.editSsh({ hostKeyFingerprint: "" });
    await editor.test();
    expect(actions.test.mock.calls[0][0].draft.target).toEqual({
      id: expect.any(String), displayName: "개발 서버", defaultMethod: "ssh",
      ssh: { host: "ssh.actual.test", port: 2222, account: "deploy", transport: { kind: "direct" }, auth: { kind: "manualPassword" } },
    });
  });

  it("저장 암호 SSH는 저장 인증이 있을 때만 saveTarget으로 저장한다", async () => {
    const itemId = "21eb36c3-234d-439d-9e1f-1d3cc5ba8125";
    for (const storedAuth of [false, true]) {
      const { editor, actions } = open({}, fakeActions(), storedAuth);
      fillSsh(editor);
      editor.editSsh({ hostKeyFingerprint: fingerprint, auth: { kind: "storedPassword", itemId } });
      const errors = validate(editor.getSnapshot().draft, storedAuth);
      expect(errors["ssh.auth"]?.text).toBe(storedAuth ? undefined : "저장 인증 사용 불가");
      await editor.save();
      expect(actions.saveTarget.mock.calls).toEqual(storedAuth ? [[{
        version: 2, targetRevision: null, target: expect.objectContaining({
          ssh: { host: "ssh.actual.test", port: 2222, account: "deploy", hostKeyFingerprint: fingerprint,
            transport: { kind: "direct" }, auth: { kind: "storedPassword", itemId } },
        }),
      }]] : []);
    }
  });

  it("S3는 프로필, 버킷 리전, 버킷이 필요하며 빈 접두어를 허용한다", async () => {
    const { editor, actions } = open({});
    editor.setName("버킷");
    editor.selectMethod("s3");
    expect(editor.canSave()).toBe(false);
    editor.editS3("profile", "company");
    editor.editS3("region", "ap-northeast-2");
    editor.editS3("bucket", "company-dev");
    expect(editor.canSave()).toBe(true);
    await editor.save();
    expect(actions.saveTarget.mock.calls[0][0].target).toMatchObject({
      defaultMethod: "s3", s3: { profile: "company", region: "ap-northeast-2", bucket: "company-dev", prefix: "" },
    });
  });

  it("방식을 바꿨다가 돌아오면 그 방식의 입력을 되살린다", () => {
    const { editor } = open({ target: local, revision: 3 });
    editor.selectMethod("rdp");
    editor.editDesktop({ host: "win.example.test" });
    editor.selectMethod("local");
    expect(editor.getSnapshot().draft).toEqual(local);
    editor.setGroup(undefined);
    editor.selectMethod("rdp");
    expect(editor.getSnapshot().draft).toEqual({
      id: local.id, displayName: local.displayName, defaultMethod: "rdp",
      rdp: { host: "win.example.test", port: "3389", tlsOnly: false, route: { kind: "direct" } },
    });
  });
});

describe("세션 편집 창 RDP·VNC", () => {
  it("SSH 터널 경유 RDP는 SSH 세션을 골라야 저장되고 비밀 없는 경로만 저장한다", async () => {
    const { editor, actions } = open({ groupId: "work" });
    editor.setName("윈도");
    editor.selectMethod("rdp");
    expect(editor.getSnapshot().draft).toMatchObject({ rdp: { host: "", port: "3389", tlsOnly: false, route: { kind: "direct" } } });
    editor.editDesktop({ host: "xrdp.example.test", tlsOnly: true, route: { kind: "ssh", targetId: "" } });
    expect(validate(editor.getSnapshot().draft, false)["desktop.route"]?.text).toBe("필수 입력");
    expect(editor.canSave()).toBe(false);
    editor.editDesktop({ route: { kind: "ssh", targetId: "ssh-server" } });
    await editor.save();
    expect(actions.saveTarget.mock.calls[0][0].target).toEqual({
      id: expect.any(String), groupId: "work", displayName: "윈도", defaultMethod: "rdp",
      rdp: { host: "xrdp.example.test", port: 3389, tlsOnly: true, route: { kind: "ssh", targetId: "ssh-server" } },
    });
  });

  it("SSM 경로는 노드 선택을 경로에 넣고 VNC 터널 경로는 호스트를 받지 않는다", async () => {
    const { editor, actions } = open({});
    editor.setName("VNC");
    editor.selectMethod("vnc");
    editor.editDesktop({ host: "vnc.internal.example", port: "5901", route: { kind: "ssm" } });
    editor.selectSsm(node);
    expect(validate(editor.getSnapshot().draft, false)["desktop.host"]?.text).toBe("호스트 형식 오류");
    editor.editDesktop({ host: "" });
    expect(editor.canSave()).toBe(true);
    editor.invalidateSsmSelection();
    expect(editor.canSave()).toBe(false);
    editor.selectSsm(node);
    await editor.save();
    expect(actions.saveTarget.mock.calls[0][0].target).toMatchObject({ vnc: { host: "", port: 5901, route: { kind: "ssm", ...node } } });
  });

  it("RDP는 모든 경로에서 IP 주소와 DNS 이름을 받고 숫자 꼬리 이름은 거부한다", () => {
    const { editor } = open({ target: { id: "rdp", displayName: "윈도", defaultMethod: "rdp",
      rdp: { host: "", port: 3390, tlsOnly: false, route: { kind: "ssm", ...node } } }, revision: 2 });
    expect(editor.getSnapshot().draft).toMatchObject({ rdp: { host: "", port: "3390", route: { kind: "ssm", ...node } } });
    expect(editor.canSave()).toBe(false);
    for (const [host, route, valid] of [
      ["10.0.1.25", { kind: "direct" }, true],
      ["win.example.test", { kind: "direct" }, true],
      ["127.0.0.1", { kind: "ssh", targetId: "ssh-server" }, true],
      ["fd00::25", { kind: "ssh", targetId: "ssh-server" }, true],
      ["2001:db8::25", { kind: "direct" }, true],
      ["2001:DB8::25", { kind: "direct" }, false],
      ["2001:0db8::25", { kind: "direct" }, false],
      ["[2001:db8::25]", { kind: "direct" }, false],
      ["fe80::1%eth0", { kind: "direct" }, false],
      ["0.0.0.0", { kind: "direct" }, false],
      ["0.1.2.3", { kind: "direct" }, false],
      ["::ffff:10.0.0.1", { kind: "direct" }, false],
      ["::ffff:a00:1", { kind: "ssh", targetId: "ssh-server" }, false],
      ["::ffff:0.0.0.0", { kind: "ssm", ...node }, false],
      ["::", { kind: "ssm", ...node }, false],
      ["010.0.1.25", { kind: "direct" }, false],
      ["host,other", { kind: "ssm", ...node }, false],
      ["10.0.1.25", { kind: "ssm", ...node }, true],
      ["2852039166", { kind: "ssm", ...node }, false],
      ["127.1", { kind: "direct" }, false],
      ["rdp.internal.example", { kind: "ssm", ...node }, true],
    ] as const) {
      editor.editDesktop({ host, route });
      expect(editor.canSave(), `${host} ${route.kind}`).toBe(valid);
      expect(validate(editor.getSnapshot().draft, false)["desktop.host"]?.text, host).toBe(valid ? undefined : "호스트 형식 오류");
    }
  });

  it("VNC 직접 연결은 IP 주소 또는 DNS 이름만 받는다", () => {
    const { editor } = open({});
    editor.setName("VNC");
    editor.selectMethod("vnc");
    for (const [host, valid] of [["10.0.1.25", true], ["vnc.example.test", true], ["127.1", false], ["", false]] as const) {
      editor.editDesktop({ host });
      expect(editor.canSave(), host).toBe(valid);
    }
  });
});

describe("편집 창 실행 응답", () => {
  it("시험 응답의 초안 버전이 다르면 실패로 남기고 티켓을 만들지 않는다", async () => {
    const actions = fakeActions();
    actions.test.mockResolvedValueOnce({ draftId: "draft", draftVersion: 9, runId: "run-1" } satisfies ConnectionTestRun);
    const { editor } = open({ target: local, revision: 3 }, actions);
    await editor.test();
    expect(editor.getSnapshot().status).toEqual({ kind: "testFailed", error: "시험 응답의 실행 ID 또는 초안 버전이 일치하지 않습니다" });
  });
});

describe("편집 창 시험과 오류 표시", () => {
  it("저장 암호 SSH는 시험하지 않는다", async () => {
    const { editor, actions } = open({}, fakeActions(), true);
    fillSsh(editor);
    expect(editor.canTest()).toBe(true);
    editor.editSsh({ auth: { kind: "storedPassword", itemId: "21eb36c3-234d-439d-9e1f-1d3cc5ba8125" } });
    expect(testSupported(editor.getSnapshot().draft)).toBe(false);
    expect(editor.canSave()).toBe(true);
    expect(editor.canTest()).toBe(false);
    await editor.test();
    expect(actions.test).not.toHaveBeenCalled();
  });

  it("시험 시작의 invalid 오류를 해당 필드에 두고 문자열 오류는 그대로 둔다", async () => {
    const actions = fakeActions();
    actions.test.mockRejectedValueOnce({ kind: "invalid", field: "ssh.host" }).mockRejectedValueOnce("호스트에 연결할 수 없습니다");
    const { editor } = open({}, actions);
    fillSsh(editor);
    await editor.test();
    const state = editor.getSnapshot();
    expect(state.status).toEqual({ kind: "testFailed", error: "호스트 형식 오류", field: "ssh.host" });
    expect(shownError(state, validate(state.draft, false), "ssh.host")).toBe("호스트 형식 오류");
    await editor.test();
    expect(editor.getSnapshot().status).toEqual({ kind: "testFailed", error: "호스트에 연결할 수 없습니다" });
  });

  it("티켓 저장의 편집 오류를 표시하고 티켓을 유지한다", async () => {
    const actions = fakeActions();
    actions.save.mockRejectedValueOnce({ kind: "invalid", field: "local.startDirectory" })
      .mockRejectedValueOnce({ kind: "conflict" }).mockRejectedValueOnce({ kind: "failed", message: "설정 파일 교체 실패" });
    const { editor } = open({ target: local, revision: 3 }, actions);
    await editor.test();
    await editor.save();
    expect(editor.getSnapshot().status).toMatchObject({ kind: "saveFailed", error: "경로 형식 오류", field: "local.startDirectory" });
    await editor.save();
    expect(editor.getSnapshot().status).toMatchObject({ kind: "saveFailed", error: "다른 작업이 세션을 변경했습니다" });
    await editor.save();
    expect(editor.getSnapshot().status).toMatchObject({ kind: "saveFailed", error: "설정 파일 교체 실패" });
    expect(actions.save).toHaveBeenCalledTimes(3);
    expect(actions.saveTarget).not.toHaveBeenCalled();
  });

  it("저장 충돌 뒤에는 저장본의 새 개정으로 다시 저장하고 입력을 유지한다", async () => {
    const actions = { ...fakeActions(), latestRevision: vi.fn(async (_id: string): Promise<number | undefined> => 8) };
    actions.saveTarget.mockRejectedValueOnce({ kind: "conflict" });
    const { editor } = open({ target: local, revision: 3 }, actions);
    editor.setName("바꾼 셸");
    await editor.save();
    expect(editor.getSnapshot().status).toMatchObject({ kind: "saveFailed", error: "다른 작업이 세션을 변경했습니다" });
    expect(actions.latestRevision).toHaveBeenCalledExactlyOnceWith("work-shell");
    expect(editor.getSnapshot().draft.displayName).toBe("바꾼 셸");
    expect(editor.targetRevision).toBe(8);
    await editor.save();
    expect(actions.saveTarget.mock.calls.map(([draft]) => draft.targetRevision)).toEqual([3, 8]);
    expect(actions.onClose).toHaveBeenCalledTimes(1);
  });

  it("티켓 저장이 충돌하면 이전 개정에 묶인 티켓을 버리고 실행을 회수한 뒤 새 개정으로 저장한다", async () => {
    const actions = { ...fakeActions(), latestRevision: vi.fn(async (_id: string): Promise<number | undefined> => 5) };
    actions.save.mockRejectedValueOnce({ kind: "conflict" });
    const { editor } = open({ target: local, revision: 3 }, actions);
    await editor.test();
    await editor.save();
    expect(editor.getSnapshot().status).toEqual({ kind: "saveFailed", error: "다른 작업이 세션을 변경했습니다" });
    expect(actions.cancelTest).toHaveBeenCalledTimes(1);
    await editor.save();
    expect(actions.save).toHaveBeenCalledTimes(1);
    expect(actions.saveTarget).toHaveBeenCalledExactlyOnceWith({ version: 2, targetRevision: 5, target: local });
  });

  it("로컬 시작 디렉터리는 비우면 통과하고 입력하면 절대 경로여야 한다", () => {
    const { editor } = open({});
    editor.setName("셸");
    expect(validate(editor.getSnapshot().draft, false)).toEqual({});
    expect(editor.canSave()).toBe(true);
    for (const path of ["C:\\work", "c:/work", "/home/dev"]) {
      editor.editLocal(path);
      expect(validate(editor.getSnapshot().draft, false), path).toEqual({});
    }
    for (const path of ["work", ".\\work", "C:work"]) {
      editor.editLocal(path);
      expect(validate(editor.getSnapshot().draft, false)["local.startDirectory"], path).toEqual({ text: "절대 경로 필요" });
      expect(editor.canSave()).toBe(false);
    }
  });

  it("화면에 표시하는 필드는 방식, 경로, 인증에 따른다", () => {
    const { editor } = open({});
    expect(shownFields(editor.getSnapshot().draft)).toEqual(["displayName", "groupId", "local.startDirectory"]);
    editor.selectMethod("ssh");
    editor.editSsh({ transport: { kind: "ssm" }, auth: { kind: "storedPassword", itemId: "" } });
    expect(shownFields(editor.getSnapshot().draft)).toEqual(["displayName", "groupId", "ssh.host", "ssh.port", "ssh.account",
      "ssh.hostKeyFingerprint", "ssh.auth", "ssm.profile", "ssm.region", "ssm.instanceId", "ssh.auth.itemId"]);
    editor.selectMethod("vnc");
    editor.editDesktop({ route: { kind: "ssh", targetId: "" } });
    expect(shownFields(editor.getSnapshot().draft)).toEqual(["displayName", "groupId", "desktop.port", "desktop.route"]);
  });
});
