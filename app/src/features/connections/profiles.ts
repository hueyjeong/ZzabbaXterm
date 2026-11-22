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
  ssh?: never;
  s3?: never;
}>;
type SsmMethod<T = SsmTarget> = Readonly<{ defaultMethod: "ssm"; ssm: T; local?: never; ssh?: never; s3?: never }>;
type SshMethod<T = SshSettings> = Readonly<{ defaultMethod: "ssh"; ssh: T; local?: never; ssm?: never; s3?: never }>;
type S3Method = Readonly<{ defaultMethod: "s3"; s3: S3Settings; local?: never; ssm?: never; ssh?: never }>;
type S3Settings = Readonly<{ profile: string; region: string; bucket: string; prefix: string }>;

export type SshTransport = Readonly<{ kind: "direct" }> | (Readonly<{ kind: "ssm" }> & SsmTarget);
export type SshAuth =
  | Readonly<{ kind: "agent"; publicKeyPath: string }>
  | Readonly<{ kind: "keyFile"; privateKeyPath: string }>
  | Readonly<{ kind: "manualPassword" }>
  | Readonly<{ kind: "storedPassword"; itemId: string }>;
export type SshSettings = Readonly<{
  host: string;
  port: number;
  account: string;
  hostKeyFingerprint: string;
  transport: SshTransport;
  auth: SshAuth;
}>;
export type SshInput = Omit<SshSettings, "port" | "transport" | "auth"> & Readonly<{
  port: string;
  auth: SshAuth | null;
  transport: SshTransport | Readonly<{ kind: "ssm"; profile?: never; region?: never; instanceId?: never }>;
}>;
export type LocalProfileDraft = ProfileDraft<TargetIdentity & LocalMethod>;
export type InitialWizardDraft = ProfileDraft<TargetIdentity & (LocalMethod | SsmMethod | S3Method)>;
export type ConnectionProfileDraft = ProfileDraft<TargetIdentity & (LocalMethod | SsmMethod | SshMethod | S3Method)>;
type WizardDraft = ProfileDraft<TargetIdentity & (LocalMethod | SsmMethod<SsmTarget | null> | SshMethod<SshInput> | S3Method)>;
export type SshHostApproval = Readonly<{
  host: string;
  port: number;
  hostKeyFingerprint: string;
  targetId: string;
}>;

type DraftIdentity = Readonly<{ draftId: string; draftVersion: number }>;
export type SshPinAttempt = DraftIdentity & Readonly<{ attemptId: string }>;

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
  verifySshHost?(approval: SshHostApproval, draft: ConnectionProfileDraft, attempt: SshPinAttempt, signal: AbortSignal): Promise<void>;
  test(attempt: ConnectionTestDraft, interactive?: (run: ConnectionTestRun) => void): Promise<ConnectionTestRun>;
  cancelTest(attempt: ConnectionTestDraft): Promise<void>;
  // Rust의 저장 성공만 이행하고 충돌과 저장 오류는 실제 메시지로 거절한다.
  save(request: ConnectionRequest): Promise<void>;
  onClose(result: ConnectionClose): void;
}

type Step = "target" | "method" | "auth" | "verify";
type Status =
  | { kind: "idle" | "closing" | "closed" }
  | { kind: "testing"; attempt: ConnectionTestDraft; sshRun?: ConnectionTestRun }
  | { kind: "passed" | "saving"; request: ConnectionRequest; attempt: ConnectionTestDraft }
  | { kind: "testFailed" | "closeFailed"; error: string }
  | { kind: "saveFailed"; request: ConnectionRequest; attempt: ConnectionTestDraft; error: string };
type SshPin = { kind: "idle" | "checking" | "verified" } | { kind: "failed"; error: string };
type RemoteCleanup = Readonly<{ kind: "cancelling" | "unconfirmed"; target: SsmTarget }>;
type State = Readonly<{ step: Step; draft: WizardDraft; status: Status; sshPin: SshPin; remoteCleanup?: RemoteCleanup }>;

function completeSsh(input: SshInput): SshSettings | null {
  const { host, account, hostKeyFingerprint, transport, auth } = input;
  const port = Number(input.port);
  if (!auth || !host.trim() || !account.trim() || !/^SHA256:[A-Za-z0-9+/]{42}[AEIMQUYcgkosw048]$/.test(hostKeyFingerprint)
    || !/^\d+$/.test(input.port) || port < 1 || port > 65535 || (transport.kind === "ssm" && !transport.instanceId)) return null;
  if ((auth.kind === "agent" && !auth.publicKeyPath.trim()) || (auth.kind === "keyFile" && !auth.privateKeyPath.trim())
    || (auth.kind === "storedPassword" && !auth.itemId.trim())) return null;
  return { host, port, account, hostKeyFingerprint, transport, auth };
}

function completeS3({ profile, region, bucket }: S3Settings) {
  return !!(profile.trim() && region.trim() && bucket.trim());
}

const steps = ["target", "method", "auth", "verify"] as const;
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);
const CANCEL_RESPONSE_TIMEOUT = 2_000;

export interface LocalConnectionWizardState {
  getSnapshot(): State;
  subscribe(listener: () => void): () => void;
  go(delta: -1 | 1, selection?: SsmTarget | null): void;
  edit(field: "groupName" | "targetName" | "startDirectory", value: string): void;
  selectMethod(method: WizardDraft["target"]["defaultMethod"]): void;
  canReviewSsh(selection: SsmTarget | null): boolean;
  editSsh(change: Partial<SshInput>): void;
  canReviewS3(): boolean;
  editS3(field: keyof S3Settings, value: string): void;
  verifySshHost(actions: LocalConnectionActions): Promise<void>;
  invalidateSsmSelection(): void;
  selectGroup(group: LocalProfileDraft["group"], revision: number | null): void;
  test(actions: LocalConnectionActions): Promise<void>;
  cancelSshTest(attempt: ConnectionTestDraft): Promise<void>;
  save(actions: LocalConnectionActions): Promise<void>;
  close(actions: LocalConnectionActions): Promise<void>;
  retryCleanup(): void;
  abandon(): void;
}

export function createLocalConnectionWizard(draftId: string, initialDraft: InitialWizardDraft): LocalConnectionWizardState {
  let state: State = { step: "target", draft: initialDraft, status: { kind: "idle" }, sshPin: { kind: "idle" } };
  let local: LocalMethod["local"] = initialDraft.target.local ?? {
    shell: /Windows/.test(navigator.userAgent) ? "powerShell" : "bash",
    startDirectory: "",
  };
  let draftVersion = 0;
  let pinCheck: AbortController | undefined;
  let nextPinAttempt = 0;
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

  function sshWithSelection(ssh: SshInput, selection?: SsmTarget | null): SshInput {
    return ssh.transport.kind === "ssm" && selection ? {
      ...ssh, transport: { kind: "ssm", profile: selection.profile, region: selection.region, instanceId: selection.instanceId },
    } : ssh;
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
    if (delta === 1 && state.step === "auth" && draft.target.defaultMethod === "ssh") {
      const { ssh } = draft.target;
      if (ssh.transport.kind === "ssm" && !selection) return;
      const next = sshWithSelection(ssh, selection);
      if (!completeSsh(next)) return;
      const previous = ssh.transport;
      if (previous.kind === "ssm" && selection && (previous.profile !== selection.profile || previous.region !== selection.region || previous.instanceId !== selection.instanceId)) {
        updateDraft({ ...draft, target: { ...draft.target, ssh: next } });
      }
    }
    if (delta === 1 && state.step === "auth" && draft.target.defaultMethod === "s3" && !completeS3(draft.target.s3)) return;
    const step = steps[steps.indexOf(state.step) + delta];
    if (step) {
      pinCheck?.abort();
      if (state.step === "verify" && (draft.target.defaultMethod === "ssh" || draft.target.defaultMethod === "s3")) {
        update({ status: { kind: "idle" } });
        void cancelPending();
      }
      update({ step, sshPin: state.sshPin.kind === "checking" ? { kind: "idle" } : state.sshPin });
    }
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

  function selectMethod(defaultMethod: WizardDraft["target"]["defaultMethod"]) {
    const { draft } = state;
    if (locked() || state.step !== "method" || draft.target.defaultMethod === defaultMethod) return;
    if (initialDraft.targetRevision !== null && initialDraft.target.defaultMethod === "s3") return;
    const { id, groupId, displayName } = draft.target;
    const method: LocalMethod | SsmMethod<null> | SshMethod<SshInput> | S3Method = defaultMethod === "local" ? { defaultMethod, local }
      : defaultMethod === "ssm" ? { defaultMethod, ssm: null }
        : defaultMethod === "s3" ? { defaultMethod, s3: { profile: "", region: "", bucket: "", prefix: "" } }
          : {
            defaultMethod, ssh: {
              host: "", port: "22", account: "", hostKeyFingerprint: "",
              transport: { kind: "direct" }, auth: null,
            }
          };
    updateDraft({ ...draft, target: { id, groupId, displayName, ...method } });
  }

  function editSsh(change: Partial<SshInput>) {
    const { draft, step } = state;
    if (locked() || draft.target.defaultMethod !== "ssh" || (step !== "method" && step !== "auth")) return;
    updateDraft({ ...draft, target: { ...draft.target, ssh: { ...draft.target.ssh, ...change } } });
  }

  function editS3(field: keyof S3Settings, value: string) {
    const { draft, step } = state;
    const { target } = draft;
    if (locked() || step !== "auth" || target.defaultMethod !== "s3" || target.s3[field] === value) return;
    updateDraft({ ...draft, target: { ...target, s3: { ...target.s3, [field]: value } } });
  }

  async function verifySshHost(actions: LocalConnectionActions) {
    const { draft, step, sshPin, status } = state;
    if (locked() || step !== "verify" || sshPin.kind === "checking" || sshPin.kind === "verified" ||
      status.kind === "testing" || draft.target.defaultMethod !== "ssh" || !actions.verifySshHost) return;
    const ssh = completeSsh(draft.target.ssh);
    if (!ssh) return;
    const checking = { kind: "checking" } as const;
    const controller = new AbortController();
    const attempt = { draftId, draftVersion, attemptId: String(++nextPinAttempt) };
    pinCheck = controller;
    update({ sshPin: checking });
    try {
      await actions.verifySshHost({ targetId: draft.target.id, host: ssh.host, port: ssh.port, hostKeyFingerprint: ssh.hostKeyFingerprint },
        { ...draft, target: { ...draft.target, ssh } }, attempt, controller.signal);
      if (state.sshPin === checking && state.status.kind !== "closing" && state.status.kind !== "closed") update({ sshPin: { kind: "verified" } });
    } catch (error) {
      if (state.sshPin === checking && state.status.kind !== "closing" && state.status.kind !== "closed") update({ sshPin: { kind: "failed", error: errorText(error) } });
    } finally {
      if (pinCheck === controller) pinCheck = undefined;
    }
  }

  function invalidateSsmSelection() {
    const { draft } = state;
    if (locked() || state.step !== "auth") return;
    const { target } = draft;
    if (target.defaultMethod === "ssm" && target.ssm) updateDraft({ ...draft, target: { ...target, ssm: null } });
    if (target.defaultMethod === "ssh" && target.ssh.transport.kind === "ssm" && target.ssh.transport.instanceId) {
      updateDraft({ ...draft, target: { ...target, ssh: { ...target.ssh, transport: { kind: "ssm" } } } });
    }
  }

  function selectGroup(group: LocalProfileDraft["group"], groupRevision: number | null) {
    if (locked() || state.step !== "target") return;
    const { draft } = state;
    updateDraft({ ...draft, group, groupRevision, target: { ...draft.target, groupId: group.id } });
  }

  function updateDraft(draft: WizardDraft) {
    pinCheck?.abort();
    draftVersion += 1;
    update({ draft, status: { kind: "idle" }, sshPin: { kind: "idle" } });
    void cancelPending();
  }

  async function cancelSshTest(attempt: ConnectionTestDraft) {
    if (!isTesting(attempt)) return;
    update({ status: { kind: "idle" } });
    const [result] = await cancelPending(attempt);
    if (result?.status === "rejected" && state.status.kind === "idle") {
      update({ status: { kind: "testFailed", error: errorText(result.reason) } });
    }
  }

  async function test(actions: LocalConnectionActions) {
    if (locked() || state.step !== "verify" || state.status.kind === "testing" || state.remoteCleanup) return;
    const { draft } = state;
    const { target } = draft;
    const ssh = target.defaultMethod === "ssh" && state.sshPin.kind === "verified" ? completeSsh(target.ssh) : null;
    const complete = target.defaultMethod === "local" ? target
      : target.defaultMethod === "ssm" ? target.ssm && { ...target, ssm: target.ssm }
        : target.defaultMethod === "s3" ? completeS3(target.s3) && target
          : ssh && { ...target, ssh };
    if (!complete) return;
    const attempt: ConnectionTestDraft = { draftId, draftVersion, draft: { ...draft, target: complete } };
    void cancelPending();
    const testing = { kind: "testing", attempt } as const;
    pending.set(attempt, active => actions.cancelTest(active));
    update({ status: testing });
    try {
      const run = await actions.test(attempt, sshRun => {
        if (isTesting(attempt)) update({ status: { kind: "testing", attempt, sshRun } });
      });
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
    pinCheck?.abort();
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
    canReviewSsh(selection) {
      const { target } = state.draft;
      return target.defaultMethod === "ssh" && (target.ssh.transport.kind === "direct" || !!selection)
        && completeSsh(sshWithSelection(target.ssh, selection)) !== null;
    },
    editSsh,
    canReviewS3() {
      const { target } = state.draft;
      return target.defaultMethod === "s3" && completeS3(target.s3);
    },
    editS3,
    verifySshHost,
    invalidateSsmSelection,
    selectGroup,
    test,
    cancelSshTest,
    save,
    close,
    retryCleanup() {
      if (state.remoteCleanup?.kind === "unconfirmed" && !locked()) void cancelPending();
    },
    abandon() {
      pinCheck?.abort();
      if (state.status.kind === "closing") {
        state = { ...state, status: { kind: "closed" } };
      } else if (pending.size || state.status.kind === "saving" || state.sshPin.kind === "checking") {
        state = { ...state, status: { kind: "closed" } };
        void cancelPending();
      }
    },
  };
}
