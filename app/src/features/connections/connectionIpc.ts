import { invoke } from "@tauri-apps/api/core";
import type { ConnectionRequest, ConnectionTestDraft, ConnectionTestRun, LocalConnectionActions, SavedTarget, TargetDraft } from "./profiles";

export type { SavedTarget, TargetDraft } from "./profiles";

type SavedEntry<T> = Readonly<{ revision: number; profile: T }>;
// parentId가 없으면 최상위 그룹, groupId가 없으면 그룹 없는 최상위 세션이다.
/** 서버 키를 다시 받은 결과다. 핀이 없어 등록했으면 registered, 같으면 unchanged, 교체했으면 replaced다. */
export type SshHostTrust = Readonly<{ fingerprint: string; status: "registered" | "unchanged" | "replaced" }>;

export type SavedGroup = Readonly<{ id: string; displayName: string; parentId?: string }>;
export type SavedConnections = Readonly<{
  version: 2;
  groups: readonly SavedEntry<SavedGroup>[];
  targets: readonly SavedEntry<SavedTarget>[];
}>;

export type GroupDraft = Readonly<{ version: 2; revision: number | null; group: SavedGroup }>;
// 하위 포함 그룹 삭제에서 클라이언트가 본 하위 그룹과 세션이다. 저장본과 정확히 같아야 지운다.
export type ExpectedEntry = Readonly<{ kind: "group" | "target"; id: string; revision: number }>;
export type ConnectionEditError =
  | Readonly<{ kind: "invalid"; field: string }>
  | Readonly<{ kind: "conflict" }>
  | Readonly<{ kind: "referenced"; targets: readonly string[] }>
  | Readonly<{ kind: "failed"; message: string }>;

type WaitReply = ConnectionTestRun & Readonly<{ sessionId: string | null }>;
type Execution = {
  started: Promise<ConnectionTestRun>;
  cancelled: boolean;
  cancellation?: Promise<void>;
};

export function createLocalConnectionIpc(): LocalConnectionActions & {
  load(): Promise<SavedConnections>;
  registerSavedSshHost(targetId: string): Promise<null>;
  trustSavedSshHost(targetId: string): Promise<SshHostTrust>;
  capabilities(): Promise<{ storedAuth: boolean }>;
  saveGroup(draft: GroupDraft): Promise<{ revision: number }>;
  deleteGroup(id: string, revision: number, expected: readonly ExpectedEntry[]): Promise<null>;
  deleteTarget(id: string, revision: number): Promise<null>;
  saveTarget(draft: TargetDraft): Promise<{ revision: number }>;
} {
  const attempts = new WeakMap<ConnectionTestDraft, Execution>();
  const tested = new WeakMap<ConnectionTestRun, { attempt: ConnectionTestDraft; execution: Execution }>();

  return {
    load: () => invoke<SavedConnections>("connections_load", {}),
    registerSavedSshHost: (targetId) => invoke<null>("register_saved_ssh_host", { targetId }),
    trustSavedSshHost: (targetId) => invoke<SshHostTrust>("trust_saved_ssh_host", { targetId }),
    capabilities: () => invoke<{ storedAuth: boolean }>("connection_capabilities", {}),
    // 아래 편집 명령은 실패하면 ConnectionEditError로 거부한다.
    saveGroup: (draft) => invoke<{ revision: number }>("connection_group_save", { draft }),
    deleteGroup: (id, revision, expected) => invoke<null>("connection_group_delete", { id, revision, expected }),
    deleteTarget: (id, revision) => invoke<null>("connection_target_delete", { id, revision }),
    saveTarget: (draft) => invoke<{ revision: number }>("connection_target_save", { draft }),
    async test(attempt, interactive) {
      // 시작은 초안 검증 실패를 ConnectionEditError의 invalid로, 그 밖의 실패를 문자열로 거부한다.
      const started = invoke<ConnectionTestRun>("connection_test_start", attempt);
      const execution: Execution = { started, cancelled: false };
      attempts.set(attempt, execution);
      const { draftId, draftVersion, runId } = await started;
      if (execution.cancelled) throw new Error("연결 테스트가 취소되었습니다");
      if (draftId !== attempt.draftId || draftVersion !== attempt.draftVersion || typeof runId !== "string" || !runId) {
        throw new Error("시험 시작 응답이 현재 초안과 일치하지 않습니다");
      }
      const run = { draftId, draftVersion, runId };
      if (attempt.draft.target.defaultMethod === "ssh") {
        if (!interactive) throw new Error("SSH 시험 터미널이 필요합니다");
        interactive(run);
      }
      const reply = await invoke<WaitReply>("connection_test_wait", { run });
      if (execution.cancelled || attempts.get(attempt) !== execution) throw new Error("연결 테스트가 취소되었습니다");
      const validSession = attempt.draft.target.defaultMethod === "s3" ? reply.sessionId === null
        : typeof reply.sessionId === "string" && !!reply.sessionId;
      if (reply.runId !== runId || reply.draftId !== draftId || reply.draftVersion !== draftVersion || !validSession) {
        throw new Error("시험 결과가 요청한 실행과 일치하지 않습니다");
      }
      tested.set(run, { attempt, execution });
      return run;
    },
    async cancelTest(attempt) {
      const execution = attempts.get(attempt);
      if (!execution) return;
      execution.cancelled = true;
      execution.cancellation ??= execution.started.then(async ({ draftId, draftVersion, runId }) => {
        const exact = attempt.draft.target.defaultMethod !== "local";
        if (typeof runId !== "string" || !runId || (exact && (draftId !== attempt.draftId || draftVersion !== attempt.draftVersion))) {
          if (exact) throw new Error("종료 확인 불가");
          return;
        }
        const result = await invoke<unknown>("connection_test_cancel", { run: { draftId, draftVersion, runId } });
        if (exact && result !== null) throw new Error("종료 확인 불가");
      }, () => { /* 시작이 거절되면 회수할 실행이 없다. */ });
      try {
        await execution.cancellation;
        attempts.delete(attempt);
      } catch (error) {
        delete execution.cancellation;
        throw error;
      }
    },
    async save(request: ConnectionRequest) {
      const success = tested.get(request.run);
      // 시험하지 않은 초안은 saveTarget으로 저장한다. 티켓 저장은 같은 초안의 성공한 시험만 받는다.
      if (!success || success.execution.cancelled || success.attempt.draft !== request.draft ||
        attempts.get(success.attempt) !== success.execution) {
        throw new Error("현재 초안의 연결 테스트 성공이 필요합니다");
      }
      // 실패하면 ConnectionEditError로 거부한다.
      await invoke<null>("connection_save", { request });
      tested.delete(request.run);
      attempts.delete(success.attempt);
    },
  };
}
