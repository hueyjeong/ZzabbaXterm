import { invoke } from "@tauri-apps/api/core";
import type { ConnectionRequest, ConnectionTestDraft, ConnectionTestRun, LocalConnectionActions, LocalProfileDraft } from "./profiles";

type SavedEntry<T> = Readonly<{ revision: number; profile: T }>;
export type SavedConnections = Readonly<{
  version: 1;
  groups: readonly SavedEntry<LocalProfileDraft["group"]>[];
  targets: readonly SavedEntry<LocalProfileDraft["target"]>[];
}>;

type WaitReply = ConnectionTestRun & Readonly<{ sessionId: string }>;
type Execution = {
  started: Promise<ConnectionTestRun>;
  cancelled: boolean;
  cancellation?: Promise<void>;
};

export function createLocalConnectionIpc(): Omit<LocalConnectionActions, "onClose"> & {
  load(): Promise<SavedConnections>;
} {
  const attempts = new WeakMap<ConnectionTestDraft, Execution>();
  const tested = new WeakMap<ConnectionTestRun, { attempt: ConnectionTestDraft; execution: Execution }>();

  return {
    load: () => invoke<SavedConnections>("connections_load", {}),
    async test(attempt) {
      const started = invoke<ConnectionTestRun>("connection_test_start", attempt);
      const execution: Execution = { started, cancelled: false };
      attempts.set(attempt, execution);
      const { draftId, draftVersion, runId } = await started;
      if (execution.cancelled) throw new Error("연결 테스트가 취소되었습니다");
      if (draftId !== attempt.draftId || draftVersion !== attempt.draftVersion || typeof runId !== "string" || !runId) {
        throw new Error("시험 시작 응답이 현재 초안과 일치하지 않습니다");
      }
      const run = { draftId, draftVersion, runId };
      const reply = await invoke<WaitReply>("connection_test_wait", { run });
      if (execution.cancelled || attempts.get(attempt) !== execution) throw new Error("연결 테스트가 취소되었습니다");
      if (reply.runId !== runId || reply.draftId !== draftId || reply.draftVersion !== draftVersion ||
        typeof reply.sessionId !== "string" || !reply.sessionId) {
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
        if (typeof runId === "string" && runId) {
          await invoke<null>("connection_test_cancel", { run: { draftId, draftVersion, runId } });
        }
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
      if (!success || success.execution.cancelled || success.attempt.draft !== request.draft ||
        attempts.get(success.attempt) !== success.execution) {
        throw new Error("현재 초안의 연결 테스트 성공이 필요합니다");
      }
      await invoke<null>("connection_save", { request });
      tested.delete(request.run);
      attempts.delete(success.attempt);
    },
  };
}
