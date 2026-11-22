export type LocalShell = "powerShell" | "bash";

// version은 Rust 저장 스키마 버전이고 draftVersion은 이번 편집의 버전이다.
export type LocalProfileDraft = Readonly<{
  version: 1;
  groupRevision: number | null;
  targetRevision: number | null;
  group: Readonly<{ id: string; displayName: string }>;
  target: Readonly<{
    id: string;
    groupId: string;
    displayName: string;
    defaultMethod: "local";
    local: Readonly<{ shell: LocalShell; startDirectory: string }>;
  }>;
}>;

type DraftIdentity = Readonly<{ draftId: string; draftVersion: number }>;

export type ConnectionTestDraft = DraftIdentity & Readonly<{ draft: LocalProfileDraft }>;

export type ConnectionTestRun = DraftIdentity & Readonly<{
  runId: string;
}>;

export type ConnectionRequest = Readonly<{
  run: ConnectionTestRun;
  draft: LocalProfileDraft;
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
type State = Readonly<{ step: Step; draft: LocalProfileDraft; status: Status }>;

const steps = ["target", "method", "auth", "verify"] as const;
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);
const CANCEL_RESPONSE_TIMEOUT = 2_000;

export interface LocalConnectionWizardState {
  getSnapshot(): State;
  subscribe(listener: () => void): () => void;
  go(delta: -1 | 1): void;
  edit(field: "groupName" | "targetName" | "startDirectory", value: string): void;
  selectGroup(group: LocalProfileDraft["group"], revision: number | null): void;
  test(actions: LocalConnectionActions): Promise<void>;
  save(actions: LocalConnectionActions): Promise<void>;
  close(actions: LocalConnectionActions): Promise<void>;
  abandon(): void;
}

export function createLocalConnectionWizard(draftId: string, draft: LocalProfileDraft): LocalConnectionWizardState {
  let state: State = { step: "target", draft, status: { kind: "idle" } };
  let draftVersion = 0;
  const listeners = new Set<() => void>();
  const pending = new Map<ConnectionTestDraft, LocalConnectionActions["cancelTest"]>();

  function update(change: Partial<State>) {
    state = { ...state, ...change };
    for (const listener of listeners) listener();
  }

  function locked() {
    const { kind } = state.status;
    return kind === "saving" || kind === "closing" || kind === "closed" || kind === "closeFailed";
  }

  function go(delta: -1 | 1) {
    if (locked()) return;
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
        if (draft.target.local.startDirectory === value) return;
        updateDraft({ ...draft, target: { ...draft.target, local: { ...draft.target.local, startDirectory: value } } });
        break;
    }
  }

  function selectGroup(group: LocalProfileDraft["group"], groupRevision: number | null) {
    if (locked() || state.step !== "target") return;
    const { draft } = state;
    updateDraft({ ...draft, group, groupRevision, target: { ...draft.target, groupId: group.id } });
  }

  function updateDraft(draft: LocalProfileDraft) {
    draftVersion += 1;
    update({ draft, status: { kind: "idle" } });
    void cancelPending();
  }

  async function test(actions: LocalConnectionActions) {
    if (locked() || state.step !== "verify" || state.status.kind === "testing") return;
    void cancelPending();
    const attempt = { draftId, draftVersion, draft: state.draft };
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
      await cancel(attempt);
      pending.delete(attempt);
    }));
  }

  async function close(actions: LocalConnectionActions) {
    if (locked() && state.status.kind !== "closeFailed") return;
    const closing = { kind: "closing" } as const;
    update({ status: closing });
    const timeout = Promise.withResolvers<null>();
    const timer = setTimeout(() => timeout.resolve(null), CANCEL_RESPONSE_TIMEOUT);
    const results = await Promise.race([cancelPending(), timeout.promise]).finally(() => clearTimeout(timer));
    if (state.status !== closing) return;
    const failures = results?.flatMap(result => result.status === "rejected" ? [errorText(result.reason)] : []) ?? [];
    if (failures.length) {
      update({ status: { kind: "closeFailed", error: failures.join("\n") } });
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
    selectGroup,
    test,
    save,
    close,
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
