import { invoke } from "@tauri-apps/api/core";
import { beforeEach, expect, it, vi } from "vitest";
import { createLocalConnectionIpc } from "./connectionIpc";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

beforeEach(() => { vi.mocked(invoke).mockReset(); });

it("그룹과 세션 편집 명령에 초안, 개정, 하위 항목을 그대로 넘긴다", async () => {
  vi.mocked(invoke).mockResolvedValue({ revision: 2 });
  const ipc = createLocalConnectionIpc();
  const group = { version: 2, revision: 1, group: { id: "child", displayName: "하위", parentId: "work" } } as const;
  await expect(ipc.saveGroup(group)).resolves.toEqual({ revision: 2 });
  const target = {
    version: 2, targetRevision: null,
    target: { id: "top", displayName: "최상위 셸", defaultMethod: "local", local: { shell: "powerShell", startDirectory: "C:\\work" } },
  } as const;
  await ipc.saveTarget(target);
  const expected = [{ kind: "group", id: "child", revision: 1 }, { kind: "target", id: "shell", revision: 3 }] as const;
  await ipc.deleteGroup("work", 4, expected);
  await ipc.deleteTarget("top", 1);
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["connection_group_save", { draft: group }],
    ["connection_target_save", { draft: target }],
    ["connection_group_delete", { id: "work", revision: 4, expected }],
    ["connection_target_delete", { id: "top", revision: 1 }],
  ]);
});

it("편집 명령의 구조화된 오류를 그대로 거부한다", async () => {
  const referenced = { kind: "referenced", targets: ["rdp"] };
  vi.mocked(invoke).mockRejectedValue(referenced);
  await expect(createLocalConnectionIpc().deleteTarget("ssh", 1)).rejects.toBe(referenced);
});

it("호스트 키 확인 명령에 세션 ID만 넘기고 결과를 그대로 돌려준다", async () => {
  const reply = { fingerprint: "SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ", status: "replaced" } as const;
  vi.mocked(invoke).mockResolvedValue(reply);
  await expect(createLocalConnectionIpc().trustSavedSshHost("server")).resolves.toEqual(reply);
  expect(vi.mocked(invoke).mock.calls).toEqual([["trust_saved_ssh_host", { targetId: "server" }]]);
});

it("저장본 조회에서 그룹과 세션 개정을 그대로 받는다", async () => {
  const saved = {
    version: 2, groups: [{ profile: { id: "work", displayName: "회사 개발" }, revision: 2 }],
    targets: [{ profile: { id: "shell", groupId: "work", displayName: "작업 셸", defaultMethod: "local", local: { shell: "powerShell", startDirectory: "" } }, revision: 3 }],
  };
  vi.mocked(invoke).mockResolvedValueOnce(saved);
  await expect(createLocalConnectionIpc().load()).resolves.toEqual(saved);
  expect(vi.mocked(invoke)).toHaveBeenCalledExactlyOnceWith("connections_load", {});
});

it("티켓 저장은 같은 초안의 성공한 시험만 받는다", async () => {
  const ipc = createLocalConnectionIpc();
  const draft = { version: 2, targetRevision: null, target: { id: "s3", displayName: "버킷", defaultMethod: "s3",
    s3: { profile: "company", region: "ap-northeast-2", bucket: "dev", prefix: "" } } } as const;
  const run = { draftId: "d", draftVersion: 0, runId: "r" };
  vi.mocked(invoke).mockResolvedValueOnce(run).mockResolvedValueOnce({ ...run, sessionId: null }).mockResolvedValue(null);
  await expect(ipc.save({ run, draft })).rejects.toThrow("현재 초안의 연결 테스트 성공이 필요합니다");
  const tested = await ipc.test({ draftId: "d", draftVersion: 0, draft });
  await expect(ipc.save({ run: tested, draft: { ...draft } })).rejects.toThrow("현재 초안의 연결 테스트 성공이 필요합니다");
  await ipc.save({ run: tested, draft });
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["connection_test_start", { draftId: "d", draftVersion: 0, draft }],
    ["connection_test_wait", { run }],
    ["connection_save", { request: { run, draft } }],
  ]);
});

it("시험 시작과 티켓 저장의 구조화된 오류를 그대로 거부한다", async () => {
  const ipc = createLocalConnectionIpc();
  const draft = { version: 2, targetRevision: null, target: { id: "s3", displayName: "버킷", defaultMethod: "s3",
    s3: { profile: "company", region: "ap-northeast-2", bucket: "dev", prefix: "" } } } as const;
  const invalid = { kind: "invalid", field: "s3.bucket" };
  vi.mocked(invoke).mockRejectedValueOnce(invalid);
  await expect(ipc.test({ draftId: "d", draftVersion: 0, draft })).rejects.toBe(invalid);
  const run = { draftId: "d", draftVersion: 1, runId: "r" };
  const conflict = { kind: "conflict" };
  vi.mocked(invoke).mockResolvedValueOnce(run).mockResolvedValueOnce({ ...run, sessionId: null }).mockRejectedValueOnce(conflict);
  const tested = await ipc.test({ draftId: "d", draftVersion: 1, draft });
  await expect(ipc.save({ run: tested, draft })).rejects.toBe(conflict);
});
