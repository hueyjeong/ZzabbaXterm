import type { SsmTarget } from "../ssm/ssm";
import type { ConnectionRequest, ConnectionTestDraft, ConnectionTestRun, LocalConnectionActions } from "./profiles";

export type TestActions = Pick<LocalConnectionActions, "test" | "cancelTest">;
export type TestStatus =
  | { kind: "idle" | "closing" | "closed" }
  | { kind: "testing"; attempt: ConnectionTestDraft; sshRun?: ConnectionTestRun }
  | { kind: "passed"; request: ConnectionRequest; attempt: ConnectionTestDraft }
  // 시험 티켓으로 저장할 때만 request와 attempt가 있다.
  | { kind: "saving"; request?: ConnectionRequest; attempt?: ConnectionTestDraft }
  | { kind: "testFailed" | "closeFailed"; error: string }
  | { kind: "saveFailed"; error: string; field?: string; request?: ConnectionRequest; attempt?: ConnectionTestDraft };
export type RemoteCleanup = Readonly<{ kind: "cancelling" | "unconfirmed"; target: SsmTarget }>;
export type TestState = Readonly<{ status: TestStatus; remoteCleanup?: RemoteCleanup }>;
// silent는 화면을 떠난 뒤 알림 없이 상태만 바꾼다.
export type TestStore = Readonly<{ get(): TestState; set(change: Partial<TestState>, silent?: boolean): void }>;

export const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);
export const CANCEL_RESPONSE_TIMEOUT = 2_000;

export type ConnectionTest = ReturnType<typeof createConnectionTest>;

/** 연결 시험 실행, 취소, SSM 원격 정리, 닫기를 맡는다. 상태는 편집 창 상태 기계의 status와 remoteCleanup에 쓴다. */
export function createConnectionTest(store: TestStore, actions: TestActions) {
  const pending = new Map<ConnectionTestDraft, TestActions["cancelTest"]>();
  const remoteCancellations = new Map<ConnectionTestDraft, Promise<void>>();
  const update = store.set;

  function locked() {
    const { kind } = store.get().status;
    return kind === "saving" || kind === "closing" || kind === "closed" || kind === "closeFailed";
  }

  function isTesting(attempt: ConnectionTestDraft) {
    const { status } = store.get();
    return status.kind === "testing" && status.attempt === attempt;
  }

  /** 완성된 초안을 시험한다. 시험 중이거나 원격 정리가 남아 있으면 시작하지 않는다. */
  async function test(attempt: ConnectionTestDraft) {
    const state = store.get();
    if (locked() || state.status.kind === "testing" || state.remoteCleanup) return;
    void cancelPending();
    const testing = { kind: "testing", attempt } as const;
    pending.set(attempt, active => actions.cancelTest(active));
    update({ status: testing });
    try {
      const run = await actions.test(attempt, sshRun => {
        if (isTesting(attempt)) update({ status: { kind: "testing", attempt, sshRun } });
      });
      if (!isTesting(attempt)) return;
      if (run.draftId !== attempt.draftId || run.draftVersion !== attempt.draftVersion) {
        throw new Error("시험 응답의 실행 ID 또는 초안 버전이 일치하지 않습니다");
      }
      update({ status: { kind: "passed", request: { run, draft: attempt.draft }, attempt } });
    } catch (error) {
      if (isTesting(attempt)) update({ status: { kind: "testFailed", error: errorText(error) } });
      else if (attempt.draft.target.defaultMethod === "ssm") return;
      void cancelPending(attempt);
    }
  }

  async function cancelSshTest(attempt: ConnectionTestDraft) {
    if (!isTesting(attempt)) return;
    update({ status: { kind: "idle" } });
    const [result] = await cancelPending(attempt);
    if (result?.status === "rejected" && store.get().status.kind === "idle") {
      update({ status: { kind: "testFailed", error: errorText(result.reason) } });
    }
  }

  function cancelPending(only?: ConnectionTestDraft) {
    return Promise.allSettled(Array.from(pending, async ([attempt, cancel]) => {
      if (only && only !== attempt) return;
      if (attempt.draft.target.defaultMethod === "ssm") {
        let cancellation = remoteCancellations.get(attempt);
        if (!cancellation) {
          const target = attempt.draft.target.ssm;
          update({ remoteCleanup: { kind: "cancelling", target } });
          cancellation = (async () => {
            try {
              await cancel(attempt);
              pending.delete(attempt);
              update({ remoteCleanup: undefined });
            } catch (error) {
              update({ remoteCleanup: { kind: "unconfirmed", target } });
              throw error;
            }
          })().finally(() => remoteCancellations.delete(attempt));
          remoteCancellations.set(attempt, cancellation);
        }
        await cancellation;
      } else {
        await cancel(attempt);
      }
      pending.delete(attempt);
    }));
  }

  /** 열린 시험을 모두 회수한 뒤 닫는다. 회수에 실패하면 closeFailed로 남는다. */
  async function close(onClosed: () => void) {
    if (locked() && store.get().status.kind !== "closeFailed") return;
    const closing = { kind: "closing" } as const;
    update({ status: closing });
    const hasRemote = [...pending.keys()].some(attempt => attempt.draft.target.defaultMethod === "ssm");
    const hasSsh = [...pending.keys()].some(attempt => attempt.draft.target.defaultMethod === "ssh");
    const cancellation = cancelPending();
    let results: Awaited<typeof cancellation> | null;
    if (hasRemote || hasSsh) {
      results = await cancellation;
    } else {
      const timeout = Promise.withResolvers<null>();
      const timer = setTimeout(() => timeout.resolve(null), CANCEL_RESPONSE_TIMEOUT);
      results = await Promise.race([cancellation, timeout.promise]).finally(() => clearTimeout(timer));
    }
    if (store.get().status !== closing) return;
    const failures = results?.flatMap(result => result.status === "rejected" ? [errorText(result.reason)] : []) ?? [];
    if (failures.length) {
      update({ status: { kind: "closeFailed", error: hasRemote ? "종료 확인 불가" : failures.join("\n") } });
      return;
    }
    update({ status: { kind: "closed" } });
    onClosed();
  }

  return {
    locked,
    test,
    cancelSshTest,
    cancelPending,
    close,
    /** 티켓 저장이 끝난 실행은 Rust가 회수하므로 취소 대상에서 뺀다. */
    settle(attempt: ConnectionTestDraft) { pending.delete(attempt); },
    retryCleanup() {
      if (store.get().remoteCleanup?.kind === "unconfirmed" && !locked()) void cancelPending();
    },
    abandon() {
      const state = store.get();
      if (state.status.kind === "closing") {
        update({ status: { kind: "closed" } }, true);
      } else if (pending.size || state.status.kind === "saving") {
        update({ status: { kind: "closed" } }, true);
        void cancelPending();
      }
    },
  };
}
