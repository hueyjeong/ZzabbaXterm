import { invoke } from "@tauri-apps/api/core";
import type { ConnectionProfileDraft, ConnectionRequest, ConnectionTestDraft, ConnectionTestRun, LocalConnectionActions } from "./profiles";

type SavedEntry<T> = Readonly<{ revision: number; profile: T }>;
export type SavedConnections = Readonly<{
  version: 1;
  groups: readonly SavedEntry<ConnectionProfileDraft["group"]>[];
  targets: readonly SavedEntry<ConnectionProfileDraft["target"]>[];
}>;

type WaitReply = ConnectionTestRun & Readonly<{ sessionId: string | null }>;
type Execution = {
  started: Promise<ConnectionTestRun>;
  cancelled: boolean;
  cancellation?: Promise<void>;
};

export function createLocalConnectionIpc(): Omit<LocalConnectionActions, "onClose"> & {
  load(): Promise<SavedConnections>;
  registerSavedSshHost(targetId: string): Promise<null>;
} {
  const attempts = new WeakMap<ConnectionTestDraft, Execution>();
  const tested = new WeakMap<ConnectionTestRun, { attempt: ConnectionTestDraft; execution: Execution }>();

  return {
    load: () => invoke<SavedConnections>("connections_load", {}),
    registerSavedSshHost: (targetId) => invoke<null>("register_saved_ssh_host", { targetId }),
    async verifySshHost({ targetId, host, port, hostKeyFingerprint }, draft, attempt, signal) {
      type Reply = { targetId: string; host: string; port: number; fingerprint: string };
      const request = { targetId, host, port, expectedFingerprint: hostKeyFingerprint };
      const cancel = () => { void invoke("cancel_ssh_host", { attempt }).catch(() => { }); };
      const check = (result: Reply) => {
        if (result?.targetId !== targetId || result.port !== port || result.fingerprint !== hostKeyFingerprint || result.host !== host) {
          throw new Error("SSH 호스트 키 확인 응답이 현재 초안과 일치하지 않습니다");
        }
      };
      signal.addEventListener("abort", cancel, { once: true });
      try {
        signal.throwIfAborted();
        const result = await invoke<Reply & { ticket: string }>("verify_ssh_host", { request, draft, attempt });
        signal.throwIfAborted();
        check(result);
        if (!/^[a-f0-9]{32}$/.test(result.ticket)) throw new Error("SSH 호스트 키 확인 예약이 올바르지 않습니다");
        check(await invoke<Reply>("commit_ssh_host", { attempt, ticket: result.ticket }));
      } catch (error) {
        if (!signal.aborted && error !== "busy") cancel();
        switch (error) {
          case "cancelled": case "stale_document": throw new Error("SSH 호스트 키 등록이 취소되었습니다");
          case "busy": throw new Error("열려 있는 SSH 호스트 키 창을 먼저 닫으세요");
          case "fingerprint_mismatch": throw new Error("선택한 SSH 호스트 키의 지문이 다릅니다");
          case "existing_pin_conflict": throw new Error("다른 SSH 호스트 키가 이미 등록되어 있습니다");
          case "store_unavailable": throw new Error("SSH 신뢰 저장소를 사용할 수 없습니다");
          default: throw error;
        }
      } finally {
        signal.removeEventListener("abort", cancel);
      }
    },
    async test(attempt, interactive) {
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
