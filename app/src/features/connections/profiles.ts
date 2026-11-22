import type { SsmTarget } from "../ssm/ssm";

export type LocalShell = "powerShell" | "bash";

// version은 Rust 저장 스키마 버전이고 draftVersion은 이번 편집의 버전이다.
type ProfileDraft<T> = Readonly<{
  version: 1;
  groupRevision: number | null;
  targetRevision: number | null;
  group: Readonly<{ id: string; displayName: string }>;
  target: T;
}>;

type TargetIdentity = Readonly<{ id: string; groupId: string; displayName: string }>;
type LocalMethod = Readonly<{
  defaultMethod: "local";
  local: Readonly<{ shell: LocalShell; startDirectory: string }>;
  ssm?: never;
}>;
type SsmMethod<T = SsmTarget> = Readonly<{ defaultMethod: "ssm"; ssm: T; local?: never }>;
export type LocalProfileDraft = ProfileDraft<TargetIdentity & LocalMethod>;
export type ConnectionProfileDraft = ProfileDraft<TargetIdentity & (LocalMethod | SsmMethod)>;
type WizardDraft = ProfileDraft<TargetIdentity & (LocalMethod | SsmMethod<SsmTarget | null>)>;

type DraftIdentity = Readonly<{ draftId: string; draftVersion: number }>;

export type ConnectionTestDraft = DraftIdentity & Readonly<{ draft: ConnectionProfileDraft }>;

export type ConnectionTestRun = DraftIdentity & Readonly<{
  runId: string;
}>;

export type ConnectionRequest = Readonly<{
  run: ConnectionTestRun;
  draft: ConnectionProfileDraft;
}>;

export type ConnectionClose =
  | Readonly<{ kind: "cancelled"; draftId: string }>
  | Readonly<{ kind: "saved"; draftId: string; targetId: string }>;

export interface LocalConnectionActions {
  test(attempt: ConnectionTestDraft): Promise<ConnectionTestRun>;
  cancelTest(attempt: ConnectionTestDraft): Promise<void>;
  // Rust의 저장 성공만 이행하고 충돌과 저장 오류는 실제 메시지로 거절한다.
  save(request: ConnectionRequest): Promise<void>;
  onClose(result: ConnectionClose): void;
}

type Step = "target" | "method" | "auth" | "verify";
type Status =
  | { kind: "idle" | "closing" | "closed" }
  | { kind: "testing"; attempt: ConnectionTestDraft }
  | { kind: "passed" | "saving"; request: ConnectionRequest; attempt: ConnectionTestDraft }
  | { kind: "testFailed" | "closeFailed"; error: string }
  | { kind: "saveFailed"; request: ConnectionRequest; attempt: ConnectionTestDraft; error: string };
type RemoteCleanup = Readonly<{ kind: "cancelling" | "unconfirmed"; target: SsmTarget }>;
type State = Readonly<{ step: Step; draft: WizardDraft; status: Status; remoteCleanup?: RemoteCleanup }>;

const steps = ["target", "method", "auth", "verify"] as const;
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);
const CANCEL_RESPONSE_TIMEOUT = 2_000;

export interface LocalConnectionWizardState {
  getSnapshot(): State;
  subscribe(listener: () => void): () => void;
  go(delta: -1 | 1, selection?: SsmTarget | null): void;
  edit(field: "groupName" | "targetName" | "startDirectory", value: string): void;
  selectMethod(method: "local" | "ssm"): void;
  invalidateSsmSelection(): void;
  selectGroup(group: LocalProfileDraft["group"], revision: number | null): void;
  test(actions: LocalConnectionActions): Promise<void>;
  save(actions: LocalConnectionActions): Promise<void>;
  close(actions: LocalConnectionActions): Promise<void>;
  retryCleanup(): void;
  abandon(): void;
}

export function createLocalConnectionWizard(draftId: string, initialDraft: ConnectionProfileDraft): LocalConnectionWizardState {
  let state: State = { step: "target", draft: initialDraft, status: { kind: "idle" } };
  let local = initialDraft.target.local ?? {
    shell: /Windows/.test(navigator.userAgent) ? "powerShell" : "bash",
    startDirectory: "",
  } satisfies LocalMethod["local"];
  let draftVersion = 0;
  const listeners = new Set<() => void>();
  const pending = new Map<ConnectionTestDraft, LocalConnectionActions["cancelTest"]>();
  const remoteCancellations = new Map<ConnectionTestDraft, Promise<void>>();

  function update(change: Partial<State>) {
    state = { ...state, ...change };
    for (const listener of listeners) listener();
  }

  function locked() {
    const { kind } = state.status;
    return kind === "saving" || kind === "closing" || kind === "closed" || kind === "closeFailed";
  }

  function go(delta: -1 | 1, selection?: SsmTarget | null) {
    if (locked()) return;
    const { draft } = state;
    if (delta === 1 && state.step === "auth" && draft.target.defaultMethod === "ssm") {
      if (!selection) return;
      const { ssm } = draft.target;
      if (!ssm || ssm.profile !== selection.profile || ssm.region !== selection.region || ssm.instanceId !== selection.instanceId) {
        const { profile, region, instanceId } = selection;
        updateDraft({ ...draft, target: { ...draft.target, ssm: { profile, region, instanceId } } });
      }
    }
    const step = steps[steps.indexOf(state.step) + delta];
    if (step) update({ step });
  }

  function edit(field: "groupName" | "targetName" | "startDirectory", value: string) {
    if (locked() || state.step !== (field === "startDirectory" ? "method" : "target")) return;
    const { draft } = state;
    switch (field) {
      case "groupName":
        if (draft.group.displayName === value) return;
        updateDraft({ ...draft, group: { ...draft.group, displayName: value } });
        break;
      case "targetName":
        if (draft.target.displayName === value) return;
        updateDraft({ ...draft, target: { ...draft.target, displayName: value } });
        break;
      case "startDirectory":
        if (draft.target.defaultMethod !== "local" || local.startDirectory === value) return;
        local = { ...local, startDirectory: value };
        updateDraft({ ...draft, target: { ...draft.target, local } });
        break;
    }
  }

  function selectMethod(defaultMethod: "local" | "ssm") {
    const { draft } = state;
    if (locked() || state.step !== "method" || draft.target.defaultMethod === defaultMethod) return;
    const { id, groupId, displayName } = draft.target;
    const method: LocalMethod | SsmMethod<null> = defaultMethod === "local"
      ? { defaultMethod, local }
      : { defaultMethod, ssm: null };
    updateDraft({ ...draft, target: { id, groupId, displayName, ...method } });
  }

  function invalidateSsmSelection() {
    const { draft } = state;
    if (locked() || state.step !== "auth" || draft.target.defaultMethod !== "ssm" || !draft.target.ssm) return;
    updateDraft({ ...draft, target: { ...draft.target, ssm: null } });
  }

  function selectGroup(group: LocalProfileDraft["group"], groupRevision: number | null) {
    if (locked() || state.step !== "target") return;
    const { draft } = state;
    updateDraft({ ...draft, group, groupRevision, target: { ...draft.target, groupId: group.id } });
  }

  function updateDraft(draft: WizardDraft) {
    draftVersion += 1;
    update({ draft, status: { kind: "idle" } });
    void cancelPending();
  }

  async function test(actions: LocalConnectionActions) {
    if (locked() || state.step !== "verify" || state.status.kind === "testing" || state.remoteCleanup) return;
    const { draft } = state;
    const { target } = draft;
    const complete = target.defaultMethod === "local" ? target : target.ssm && { ...target, ssm: target.ssm };
    if (!complete) return;
    const attempt: ConnectionTestDraft = { draftId, draftVersion, draft: { ...draft, target: complete } };
    void cancelPending();
    const testing = { kind: "testing", attempt } as const;
    pending.set(attempt, active => actions.cancelTest(active));
    update({ status: testing });
    try {
      const run = await actions.test(attempt);
      if (!isTesting(attempt)) return;
      if (run.draftId !== draftId || run.draftVersion !== attempt.draftVersion) {
        throw new Error("시험 응답의 실행 ID 또는 초안 버전이 일치하지 않습니다");
      }
      update({ status: { kind: "passed", request: { run, draft: attempt.draft }, attempt } });
    } catch (error) {
      if (isTesting(attempt)) update({ status: { kind: "testFailed", error: errorText(error) } });
      else if (attempt.draft.target.defaultMethod === "ssm") return;
      void cancelPending(attempt);
    }
  }

  function isTesting(attempt: ConnectionTestDraft) {
    return state.status.kind === "testing" && state.status.attempt === attempt;
  }

  async function save(actions: LocalConnectionActions) {
    const { status } = state;
    if (state.step !== "verify" || (status.kind !== "passed" && status.kind !== "saveFailed")) return;
    const { request, attempt } = status;
    const saving = { kind: "saving", request, attempt } as const;
    update({ status: saving });
    try {
      await actions.save(request);
    } catch (error) {
      if (state.status === saving) update({ status: { kind: "saveFailed", request, attempt, error: errorText(error) } });
      return;
    }
    pending.delete(attempt);
    if (state.status !== saving) return;
    update({ status: { kind: "closed" } });
    actions.onClose({ kind: "saved", draftId, targetId: request.draft.target.id });
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

  async function close(actions: LocalConnectionActions) {
    if (locked() && state.status.kind !== "closeFailed") return;
    const closing = { kind: "closing" } as const;
    update({ status: closing });
    const hasRemote = [...pending.keys()].some(attempt => attempt.draft.target.defaultMethod === "ssm");
    const cancellation = cancelPending();
    let results: Awaited<typeof cancellation> | null;
    if (hasRemote) {
      results = await cancellation;
    } else {
      const timeout = Promise.withResolvers<null>();
      const timer = setTimeout(() => timeout.resolve(null), CANCEL_RESPONSE_TIMEOUT);
      results = await Promise.race([cancellation, timeout.promise]).finally(() => clearTimeout(timer));
    }
    if (state.status !== closing) return;
    const failures = results?.flatMap(result => result.status === "rejected" ? [errorText(result.reason)] : []) ?? [];
    if (failures.length) {
      update({ status: { kind: "closeFailed", error: hasRemote ? "종료 확인 불가" : failures.join("\n") } });
      return;
    }
    update({ status: { kind: "closed" } });
    actions.onClose({ kind: "cancelled", draftId });
  }

  return {
    getSnapshot: () => state,
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
    go,
    edit,
    selectMethod,
    invalidateSsmSelection,
    selectGroup,
    test,
    save,
    close,
    retryCleanup() {
      if (state.remoteCleanup?.kind === "unconfirmed" && !locked()) void cancelPending();
    },
    abandon() {
      if (state.status.kind === "closing") {
        state = { ...state, status: { kind: "closed" } };
      } else if (pending.size || state.status.kind === "saving") {
        state = { ...state, status: { kind: "closed" } };
        void cancelPending();
      }
    },
  };
}
