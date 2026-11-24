import type { SsmTarget } from "../ssm/ssm";
import type { ConnectionEditError } from "./connectionIpc";
import { createConnectionTest, type TestState, type TestStatus } from "./connectionTest";
import {
  completeDesktop, completeS3, completeSsh, desktopHostUsed, desktopPorts, fingerprintValid, hostAddress, portValid, rdpHost,
  desktopInput, sshInput,
  type ConnectionClose, type ConnectionMethod, type ConnectionTestDraft, type DesktopInput, type EditorTarget,
  type LocalConnectionActions, type LocalShell, type S3Settings, type SavedTarget, type SshInput, type TargetDraft,
} from "./profiles";

export type EditorActions = LocalConnectionActions & Readonly<{
  // 시험하지 않은 초안을 입력 검증만 거쳐 저장한다.
  saveTarget(draft: TargetDraft): Promise<{ revision: number }>;
  onClose(result: ConnectionClose): void;
  /** 저장 충돌 뒤 목록을 다시 읽어 세션의 현재 개정을 돌려준다. 없으면 undefined다. */
  latestRevision?(targetId: string): Promise<number | undefined>;
}>;
/** groupId만 있으면 새 세션, target이 있으면 저장된 세션 편집, copy가 있으면 그 설정을 복제한 새 세션이다. */
export type EditorRequest = Readonly<{ groupId?: string; target?: never; copy?: never }>
  | Readonly<{ target: SavedTarget; revision: number; copy?: never }>
  | Readonly<{ copy: SavedTarget; target?: never }>;
export type FieldError = Readonly<{ text: string; required?: true }>;
export type FieldErrors = Readonly<Record<string, FieldError>>;
export type EditorState = TestState & Readonly<{ draft: EditorTarget; touched: ReadonlySet<string> }>;

const ssmFields = ["ssm.profile", "ssm.region", "ssm.instanceId"];

/** Rust ValidationError::field 경로 중 현재 초안의 화면에서 필드 아래에 표시하는 항목이다. */
export function shownFields(target: EditorTarget): readonly string[] {
  const fields = ["displayName", "groupId"];
  switch (target.defaultMethod) {
    case "local": fields.push("local.startDirectory"); break;
    case "ssm": fields.push(...ssmFields); break;
    case "s3": fields.push("s3.profile", "s3.region", "s3.bucket", "s3.prefix"); break;
    case "ssh": {
      const { transport, auth } = target.ssh;
      fields.push("ssh.host", "ssh.port", "ssh.account", "ssh.hostKeyFingerprint", "ssh.auth");
      if (transport.kind === "ssm") fields.push(...ssmFields);
      if (auth?.kind === "agent" || auth?.kind === "keyFile") fields.push("ssh.auth.keyPath");
      if (auth?.kind === "storedPassword") fields.push("ssh.auth.itemId");
      break;
    }
    case "rdp": case "vnc": {
      const { route } = target.defaultMethod === "rdp" ? target.rdp : target.vnc;
      fields.push("desktop.port", "desktop.route");
      if (desktopHostUsed(target.defaultMethod, route.kind)) fields.push("desktop.host");
      if (route.kind === "ssm") fields.push(...ssmFields);
      break;
    }
  }
  return fields;
}

/** 연결 시험을 지원하는 초안이다. RDP와 VNC, 저장 암호 SSH는 시험하지 않는다. */
export function testSupported(target: EditorTarget) {
  return target.defaultMethod !== "rdp" && target.defaultMethod !== "vnc"
    && !(target.defaultMethod === "ssh" && target.ssh.auth?.kind === "storedPassword");
}

const required: FieldError = { text: "필수 입력", required: true };
export const CONFLICT_TEXT = "다른 작업이 세션을 변경했습니다";

/** 클라이언트 검증이다. 빈 필수 입력은 required로 표시해 건드린 필드에서만 보인다. */
export function validate(target: EditorTarget, storedAuth: boolean): FieldErrors {
  const errors: Record<string, FieldError> = {};
  const need = (field: string, value: string) => { if (!value.trim()) errors[field] = required; };
  const port = (field: string, value: string) => {
    if (!value.trim()) errors[field] = required;
    else if (!portValid(value)) errors[field] = { text: "포트 형식 오류" };
  };
  need("displayName", target.displayName);
  switch (target.defaultMethod) {
    case "local": {
      // 비우면 홈 디렉터리에서 시작한다. 입력하면 드라이브나 /로 시작하는 절대 경로여야 한다. OS별 검사는 Rust가 한다.
      const { startDirectory } = target.local;
      if (startDirectory && !/^([A-Za-z]:[\\/]|\/)/.test(startDirectory)) {
        errors["local.startDirectory"] = { text: "절대 경로 필요" };
      }
      break;
    }
    case "ssm":
      if (!target.ssm) errors["ssm.instanceId"] = required;
      break;
    case "ssh": {
      const { ssh } = target;
      need("ssh.host", ssh.host);
      port("ssh.port", ssh.port);
      need("ssh.account", ssh.account);
      if (!fingerprintValid(ssh.hostKeyFingerprint)) errors["ssh.hostKeyFingerprint"] = { text: "지문 형식 오류" };
      if (ssh.transport.kind === "ssm" && !ssh.transport.instanceId) errors["ssm.instanceId"] = required;
      const { auth } = ssh;
      if (!auth) errors["ssh.auth"] = required;
      else if (auth.kind === "agent") need("ssh.auth.keyPath", auth.publicKeyPath);
      else if (auth.kind === "keyFile") need("ssh.auth.keyPath", auth.privateKeyPath);
      else if (auth.kind === "storedPassword") {
        need("ssh.auth.itemId", auth.itemId);
        if (!storedAuth) errors["ssh.auth"] = { text: "저장 인증 사용 불가" };
      }
      break;
    }
    case "s3":
      need("s3.profile", target.s3.profile);
      need("s3.region", target.s3.region);
      need("s3.bucket", target.s3.bucket);
      break;
    case "rdp": case "vnc": {
      const input = target.defaultMethod === "rdp" ? target.rdp : target.vnc;
      const host = input.host.trim();
      if (desktopHostUsed(target.defaultMethod, input.route.kind)) {
        if (!host) errors["desktop.host"] = required;
        else if (!(target.defaultMethod === "rdp" ? rdpHost(host) : hostAddress(host))) errors["desktop.host"] = { text: "호스트 형식 오류" };
      } else if (host) errors["desktop.host"] = { text: "호스트 형식 오류" };
      port("desktop.port", input.port);
      if (input.route.kind === "ssh" && !input.route.targetId) errors["desktop.route"] = required;
      if (input.route.kind === "ssm" && !input.route.instanceId) errors["ssm.instanceId"] = required;
      break;
    }
  }
  return errors;
}

/** 저장 형식으로 바꾼다. 입력이 완성되지 않았으면 null이다. */
export function completeTarget(target: EditorTarget): SavedTarget | null {
  switch (target.defaultMethod) {
    case "local": return target;
    case "ssm": return target.ssm && { ...target, ssm: target.ssm };
    case "s3": return completeS3(target.s3) ? target : null;
    case "ssh": {
      const ssh = completeSsh(target.ssh);
      return ssh && { ...target, ssh };
    }
    case "rdp": {
      const rdp = completeDesktop("rdp", target.rdp);
      return rdp && { ...target, rdp };
    }
    case "vnc": {
      const vnc = completeDesktop("vnc", target.vnc);
      return vnc && { ...target, vnc };
    }
  }
}

function serverFieldText(field: string) {
  if (/port$/i.test(field)) return "포트 형식 오류";
  if (/host$/i.test(field)) return "호스트 형식 오류";
  if (field === "ssh.hostKeyFingerprint") return "지문 형식 오류";
  if (field === "local.startDirectory") return "경로 형식 오류";
  return "입력 오류";
}

/** Rust 편집 오류를 표시 문구로 바꾼다. invalid는 field를 함께 돌려준다. */
export function editError(error: unknown): { error: string; field?: string } {
  const edit = error as Partial<ConnectionEditError> | null;
  switch (typeof edit === "object" && edit ? edit.kind : undefined) {
    case "invalid": {
      const { field } = edit as Extract<ConnectionEditError, { kind: "invalid" }>;
      return { error: serverFieldText(field), field };
    }
    case "conflict": return { error: CONFLICT_TEXT };
    case "referenced": return { error: `참조 중인 세션: ${(edit as Extract<ConnectionEditError, { kind: "referenced" }>).targets.join(", ")}` };
    case "failed": return { error: (edit as Extract<ConnectionEditError, { kind: "failed" }>).message };
    default: return { error: error instanceof Error ? error.message : String(error) };
  }
}

/** 필드 아래에 표시할 오류다. Rust 오류가 먼저이고, 빈 필수 입력은 건드린 필드에서만 보인다. */
export function shownError(state: EditorState, errors: FieldErrors, field: string) {
  const { status, touched } = state;
  if ((status.kind === "saveFailed" || status.kind === "testFailed") && status.field === field) return status.error;
  const error = errors[field];
  return error && (!error.required || touched.has(field)) ? error.text : undefined;
}

function blankMethod(method: ConnectionMethod, shell: LocalShell): EditorTarget {
  const identity = { id: "", displayName: "" };
  const desktop = (kind: "rdp" | "vnc"): DesktopInput => ({ host: "", port: String(desktopPorts[kind]), tlsOnly: false, route: { kind: "direct" } });
  switch (method) {
    case "local": return { ...identity, defaultMethod: method, local: { shell, startDirectory: "" } };
    case "ssm": return { ...identity, defaultMethod: method, ssm: null };
    case "s3": return { ...identity, defaultMethod: method, s3: { profile: "", region: "", bucket: "", prefix: "" } };
    case "rdp": return { ...identity, defaultMethod: method, rdp: desktop("rdp") };
    case "vnc": return { ...identity, defaultMethod: method, vnc: desktop("vnc") };
    case "ssh": return {
      ...identity, defaultMethod: method,
      ssh: { host: "", port: "22", account: "", hostKeyFingerprint: "", transport: { kind: "direct" }, auth: null },
    };
  }
}

export function editorTarget(target: SavedTarget): EditorTarget {
  switch (target.defaultMethod) {
    case "ssh": return { ...target, ssh: sshInput(target.ssh) };
    case "rdp": return { ...target, rdp: desktopInput(target.rdp) };
    case "vnc": return { ...target, vnc: desktopInput(target.vnc) };
    default: return target;
  }
}

const sameSsm = (a: Partial<SsmTarget>, b: SsmTarget) => a.profile === b.profile && a.region === b.region && a.instanceId === b.instanceId;

export type SessionEditor = ReturnType<typeof createSessionEditor>;

export function createSessionEditor(request: EditorRequest, actions: EditorActions, options: Readonly<{
  draftId?: string;
  storedAuth?: () => boolean;
  shell?: LocalShell;
}> = {}) {
  const draftId = options.draftId ?? crypto.randomUUID();
  const shell = options.shell ?? (/Windows/.test(navigator.userAgent) ? "powerShell" : "bash");
  let targetRevision = request.target ? request.revision : null;
  const initial: EditorTarget = request.target ? editorTarget(request.target)
    : request.copy ? { ...editorTarget(request.copy), id: crypto.randomUUID(), displayName: `${request.copy.displayName} (복사본)` }
      : { ...blankMethod("local", shell), id: crypto.randomUUID(), ...(request.groupId ? { groupId: request.groupId } : {}) };
  let state: EditorState = { draft: initial, touched: new Set(), status: { kind: "idle" } };
  let draftVersion = 0;
  // 방식을 바꿨다가 돌아오면 그 방식에서 입력한 값을 되살린다.
  const remembered = new Map<ConnectionMethod, EditorTarget>([[initial.defaultMethod, initial]]);
  const listeners = new Set<() => void>();
  const storedAuth = () => options.storedAuth?.() ?? false;

  function update(change: Partial<EditorState>, silent?: boolean) {
    state = { ...state, ...change };
    if (!silent) for (const listener of listeners) listener();
  }
  const tester = createConnectionTest({ get: () => state, set: update }, {
    test: (...args) => actions.test(...args),
    cancelTest: attempt => actions.cancelTest(attempt),
  }, editError);

  // 입력을 바꾸면 이전 시험 성공을 무효로 하고 열린 시험을 취소한다.
  function updateDraft(draft: EditorTarget, field?: string) {
    if (tester.locked()) return;
    draftVersion += 1;
    const touched = field && !state.touched.has(field) ? new Set([...state.touched, field]) : state.touched;
    remembered.set(draft.defaultMethod, draft);
    update({ draft, touched, status: { kind: "idle" } });
    void tester.cancelPending();
  }

  function touch(field: string) {
    if (state.touched.has(field)) return;
    update({ touched: new Set([...state.touched, field]) });
  }

  function editDesktop(change: Partial<DesktopInput>, field?: string) {
    const { draft } = state;
    if (draft.defaultMethod === "rdp") updateDraft({ ...draft, rdp: { ...draft.rdp, ...change } }, field);
    if (draft.defaultMethod === "vnc") updateDraft({ ...draft, vnc: { ...draft.vnc, ...change } }, field);
  }

  function testable() {
    const { draft, status, remoteCleanup } = state;
    return testSupported(draft) && !tester.locked()
      && status.kind !== "testing" && !remoteCleanup && Object.keys(validate(draft, storedAuth())).length === 0
      && completeTarget(draft) !== null;
  }

  function ticket(status: TestStatus) {
    return status.kind === "passed" || (status.kind === "saveFailed" && status.request && status.attempt)
      ? { request: status.request!, attempt: status.attempt! } : null;
  }

  function savable() {
    const { draft, status } = state;
    if (tester.locked() || status.kind === "testing") return false;
    return !!ticket(status) || (Object.keys(validate(draft, storedAuth())).length === 0 && completeTarget(draft) !== null);
  }

  async function test() {
    if (!testable()) return;
    const target = completeTarget(state.draft)!;
    const attempt: ConnectionTestDraft = { draftId, draftVersion, draft: { version: 2, targetRevision, target } };
    await tester.test(attempt);
  }

  // 같은 초안의 시험 성공이 있으면 티켓으로 저장해 확인한 계정을 함께 남기고, 없으면 입력 검증만 거쳐 저장한다.
  async function save() {
    if (!savable()) return;
    const passed = ticket(state.status);
    const target = passed ? passed.request.draft.target : completeTarget(state.draft)!;
    const saving = { kind: "saving", ...passed } as const;
    update({ status: saving });
    try {
      if (passed) await actions.save(passed.request);
      else await actions.saveTarget({ version: 2, targetRevision, target });
    } catch (error) {
      if (state.status !== saving) return;
      if ((error as Partial<ConnectionEditError> | null)?.kind === "conflict" && targetRevision !== null && actions.latestRevision) {
        // 다음 저장은 저장본의 새 개정으로 보낸다. 시험 티켓은 이전 개정에 묶여 있으므로 버리고 실행을 회수한다.
        const latest = await actions.latestRevision(target.id).catch(() => undefined);
        if (latest !== undefined) targetRevision = latest;
        if (state.status !== saving) return;
        if (passed) draftVersion += 1;
        update({ status: { kind: "saveFailed", ...editError(error) } });
        if (passed) void tester.cancelPending();
        return;
      }
      update({ status: { kind: "saveFailed", ...editError(error), ...passed } });
      return;
    }
    if (passed) tester.settle(passed.attempt);
    if (state.status !== saving) return;
    update({ status: { kind: "closed" } });
    actions.onClose({ kind: "saved", draftId, targetId: target.id });
  }

  return {
    draftId,
    get targetRevision() { return targetRevision; },
    getSnapshot: () => state,
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
    canTest: testable,
    canSave: savable,
    touch,
    setName(displayName: string) {
      if (state.draft.displayName !== displayName) updateDraft({ ...state.draft, displayName }, "displayName");
    },
    setGroup(groupId: string | undefined) {
      const { draft } = state;
      if (draft.groupId === groupId) return;
      const { groupId: _previous, ...rest } = draft;
      updateDraft((groupId ? { ...rest, groupId } : rest) as EditorTarget, "groupId");
    },
    selectMethod(method: ConnectionMethod) {
      const { draft } = state;
      if (draft.defaultMethod === method) return;
      const { id, groupId, displayName } = draft;
      const { id: _id, displayName: _name, groupId: _group, ...settings } = remembered.get(method) ?? blankMethod(method, shell);
      updateDraft({ ...settings, id, displayName, ...(groupId ? { groupId } : {}) } as EditorTarget);
    },
    editLocal(startDirectory: string) {
      const { draft } = state;
      if (draft.defaultMethod !== "local" || draft.local.startDirectory === startDirectory) return;
      updateDraft({ ...draft, local: { ...draft.local, startDirectory } }, "local.startDirectory");
    },
    editSsh(change: Partial<SshInput>, field?: string) {
      const { draft } = state;
      if (draft.defaultMethod === "ssh") updateDraft({ ...draft, ssh: { ...draft.ssh, ...change } }, field);
    },
    editS3(field: keyof S3Settings, value: string) {
      const { draft } = state;
      if (draft.defaultMethod !== "s3" || draft.s3[field] === value) return;
      updateDraft({ ...draft, s3: { ...draft.s3, [field]: value } }, `s3.${field}`);
    },
    editDesktop,
    /** SSM 노드 선택을 현재 방식의 대상에 넣는다. 같은 선택이면 초안을 바꾸지 않는다. */
    selectSsm(selection: SsmTarget) {
      const { draft } = state;
      const { profile, region, instanceId } = selection;
      if (draft.defaultMethod === "ssm" && !(draft.ssm && sameSsm(draft.ssm, selection))) {
        updateDraft({ ...draft, ssm: { profile, region, instanceId } }, "ssm.instanceId");
      }
      if (draft.defaultMethod === "ssh" && draft.ssh.transport.kind === "ssm" && !sameSsm(draft.ssh.transport, selection)) {
        updateDraft({ ...draft, ssh: { ...draft.ssh, transport: { kind: "ssm", profile, region, instanceId } } }, "ssm.instanceId");
      }
      if (draft.defaultMethod === "rdp" || draft.defaultMethod === "vnc") {
        const { route } = draft.defaultMethod === "rdp" ? draft.rdp : draft.vnc;
        if (route.kind === "ssm" && !sameSsm(route, selection)) editDesktop({ route: { kind: "ssm", profile, region, instanceId } }, "ssm.instanceId");
      }
    },
    invalidateSsmSelection() {
      const { draft } = state;
      if (draft.defaultMethod === "ssm" && draft.ssm) updateDraft({ ...draft, ssm: null }, "ssm.instanceId");
      if (draft.defaultMethod === "ssh" && draft.ssh.transport.kind === "ssm" && draft.ssh.transport.instanceId) {
        updateDraft({ ...draft, ssh: { ...draft.ssh, transport: { kind: "ssm" } } }, "ssm.instanceId");
      }
      if (draft.defaultMethod === "rdp" || draft.defaultMethod === "vnc") {
        const { route } = draft.defaultMethod === "rdp" ? draft.rdp : draft.vnc;
        if (route.kind === "ssm" && route.instanceId) editDesktop({ route: { kind: "ssm" } }, "ssm.instanceId");
      }
    },
    test,
    cancelSshTest: tester.cancelSshTest,
    save,
    close: () => tester.close(() => actions.onClose({ kind: "cancelled", draftId })),
    retryCleanup: tester.retryCleanup,
    abandon: tester.abandon,
  };
}
