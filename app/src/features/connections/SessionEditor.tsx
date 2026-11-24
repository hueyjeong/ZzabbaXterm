import { useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type KeyboardEvent } from "react";
import { X } from "lucide-react";
import type { SavedConnections } from "./connectionIpc";
import { SsmSelectionView } from "../ssm/SsmSelectionView";
import { useSsmSelection } from "../ssm/ssm";
import { createSsmDiscovery } from "../ssm/ssmDiscovery";
import { TerminalView } from "../terminal/TerminalView";
import { sshTestIpc } from "../terminal/session";
import { IconButton, ProtocolIcon } from "../ui/controls";
import type { ConnectionMethod } from "./profiles";
import { createSessionEditor, editorFields, shownError, validate, type EditorActions, type EditorRequest } from "./sessionEditor";
import { DesktopFields } from "./fields/DesktopFields";
import { LocalFields } from "./fields/LocalFields";
import { S3Fields } from "./fields/S3Fields";
import { SshFields } from "./fields/SshFields";
import "./editor.css";

export type SessionEditorProps = EditorActions & Readonly<{
  request: EditorRequest;
  groups: SavedConnections["groups"];
  // RDP·VNC의 "SSH 세션" 후보를 고를 저장된 세션이다.
  targets?: SavedConnections["targets"];
  // 저장 암호 인증 가용 여부를 창을 열 때 확인한다.
  capabilities?: () => Promise<{ storedAuth: boolean }>;
}>;

const methods: readonly (readonly [ConnectionMethod, string])[] = [
  ["local", "로컬"], ["ssh", "SSH"], ["ssm", "AWS SSM"], ["s3", "S3"], ["rdp", "RDP"], ["vnc", "VNC"],
];
const discovery = createSsmDiscovery();
const INDENT = "   ";

function MethodTiles({ value, disabled, onSelect }: Readonly<{ value: ConnectionMethod; disabled: boolean; onSelect(method: ConnectionMethod): void }>) {
  const group = useRef<HTMLDivElement>(null);
  function move(event: KeyboardEvent<HTMLDivElement>) {
    const index = methods.findIndex(([method]) => method === value);
    const next = event.key === "ArrowRight" || event.key === "ArrowDown" ? (index + 1) % methods.length
      : event.key === "ArrowLeft" || event.key === "ArrowUp" ? (index - 1 + methods.length) % methods.length
        : event.key === "Home" ? 0 : event.key === "End" ? methods.length - 1 : -1;
    if (next < 0 || disabled) return;
    event.preventDefault();
    onSelect(methods[next][0]);
    group.current?.querySelectorAll<HTMLButtonElement>('[role="radio"]')[next]?.focus();
  }
  return <div ref={group} className="method-tiles" role="radiogroup" aria-label="접속 방식" onKeyDown={move}>
    {methods.map(([method, label]) => <button key={method} type="button" role="radio" className="method-tile" value={method}
      aria-checked={value === method} tabIndex={value === method ? 0 : -1} disabled={disabled}
      onClick={() => onSelect(method)}>
      <ProtocolIcon kind={method} />
      <span>{label}</span>
    </button>)}
  </div>;
}

/** 중첩 그룹을 부모 아래에 깊이만큼 들여 쓴 선택지로 펼친다. 부모가 없는 그룹은 최상위에 둔다. */
export function groupOptions(groups: SavedConnections["groups"]) {
  const known = new Set(groups.map(({ profile }) => profile.id));
  const children = new Map<string | undefined, SavedConnections["groups"][number]["profile"][]>();
  for (const { profile } of groups) {
    const parent = profile.parentId && known.has(profile.parentId) ? profile.parentId : undefined;
    children.set(parent, [...children.get(parent) ?? [], profile]);
  }
  const options: { id: string; label: string }[] = [];
  const seen = new Set<string>();
  const walk = (parent: string | undefined, depth: number) => {
    for (const group of children.get(parent) ?? []) {
      if (seen.has(group.id)) continue;
      seen.add(group.id);
      options.push({ id: group.id, label: INDENT.repeat(depth) + group.displayName });
      walk(group.id, depth + 1);
    }
  };
  walk(undefined, 0);
  return options;
}

export function SessionEditor(props: SessionEditorProps) {
  const latest = useRef(props);
  const [storedAuth, setStoredAuth] = useState(false);
  const storedAuthRef = useRef(false);
  useEffect(() => {
    let active = true;
    props.capabilities?.().then(value => {
      if (!active) return;
      storedAuthRef.current = value.storedAuth === true;
      setStoredAuth(storedAuthRef.current);
    }, () => { /* 확인하지 못하면 저장 암호를 막는다. */ });
    return () => { active = false; };
  }, [props.capabilities]);
  useLayoutEffect(() => { latest.current = props; });
  const [editor] = useState(() => createSessionEditor(props.request, {
    test: (...args) => latest.current.test(...args),
    cancelTest: attempt => latest.current.cancelTest(attempt),
    save: request => latest.current.save(request),
    saveTarget: draft => latest.current.saveTarget(draft),
    onClose: result => latest.current.onClose(result),
  }, { storedAuth: () => storedAuthRef.current }));
  const state = useSyncExternalStore(editor.subscribe, editor.getSnapshot);
  const { draft, status, remoteCleanup } = state;
  const errors = useMemo(() => validate(draft, storedAuth), [draft, storedAuth]);
  const error = (field: string) => shownError(state, errors, field);
  const desktop = draft.defaultMethod === "rdp" ? draft.rdp : draft.defaultMethod === "vnc" ? draft.vnc : null;
  const closingState = status.kind === "closing" || status.kind === "closed" || status.kind === "closeFailed";
  const useSsm = draft.defaultMethod === "ssm" || (draft.defaultMethod === "ssh" && draft.ssh.transport.kind === "ssm")
    || desktop?.route.kind === "ssm";
  const ssm = useSsmSelection(discovery, useSsm && !closingState, editor.invalidateSsmSelection);
  const selection = ssm.selection;
  useEffect(() => { if (selection) editor.selectSsm(selection); }, [selection, editor]);
  const dialog = useRef<HTMLDialogElement>(null);
  useLayoutEffect(() => {
    const modal = dialog.current;
    modal?.showModal();
    return () => modal?.close();
  }, []);
  useLayoutEffect(() => editor.abandon, [editor]);
  const sshRun = status.kind === "testing" ? status.sshRun : undefined;
  const sshAttempt = status.kind === "testing" ? status.attempt : undefined;
  const sshIpc = useMemo(() => sshRun && sshAttempt && sshTestIpc(sshRun, () => editor.cancelSshTest(sshAttempt)), [sshRun, sshAttempt, editor]);
  const sshTargets = useMemo(() => (props.targets ?? []).flatMap(({ profile }) => profile.defaultMethod === "ssh"
    && profile.ssh.transport.kind === "direct" && (profile.ssh.auth.kind === "agent" || profile.ssh.auth.kind === "keyFile")
    && profile.id !== draft.id ? [profile] : []), [props.targets, draft.id]);
  const groups = useMemo(() => groupOptions(props.groups), [props.groups]);

  if (status.kind === "closed") return null;
  const locked = status.kind === "saving" || closingState;
  const title = editor.targetRevision === null ? "세션 추가" : "세션 편집";
  const close = () => void editor.close();
  const touch = editor.touch;
  // 저장된 SSM 대상은 새 노드를 고르기 전까지 그대로 둔다.
  const savedSsm = draft.defaultMethod === "ssm" ? draft.ssm
    : draft.defaultMethod === "ssh" && draft.ssh.transport.kind === "ssm" && draft.ssh.transport.instanceId ? draft.ssh.transport
      : desktop?.route.kind === "ssm" && desktop.route.instanceId ? desktop.route : null;
  // 닫는 중에는 SSM 조회를 멈추므로 선택 화면을 숨긴다.
  const ssmView = useSsm && !closingState && <>
    <SsmSelectionView state={ssm} />
    {savedSsm && !selection && <div className="editor-field"><span>현재 노드</span> <output>{savedSsm.profile} / {savedSsm.region} / {savedSsm.instanceId}</output></div>}
    {["ssm.profile", "ssm.region", "ssm.instanceId"].map(field => error(field) && <div key={field} role="alert" className="field-error">{error(field)}</div>)}
  </>;
  const saveError = status.kind === "saveFailed" && !(status.field && (editorFields as readonly string[]).includes(status.field))
    ? status.field ? `${status.error}: ${status.field}` : status.error : undefined;

  return (
    <dialog ref={dialog} className="connection-wizard session-editor" aria-label={title} onCancel={(event) => {
      event.preventDefault();
      close();
    }}>
      <header className="editor-header">
        <h2>{title}</h2>
        <IconButton icon={X} label="닫기" small disabled={locked && status.kind !== "closeFailed"} onClick={close} />
      </header>
      <fieldset className="editor-body" disabled={locked}>
        <div className="editor-field">
          <label>이름 <input value={draft.displayName} onChange={event => editor.setName(event.target.value)} onBlur={() => touch("displayName")} /></label>
          {error("displayName") && <div role="alert" className="field-error">{error("displayName")}</div>}
        </div>
        <div className="editor-field">
          <label>그룹 <select value={draft.groupId ?? ""} onChange={event => editor.setGroup(event.currentTarget.value || undefined)}>
            <option value="">최상위</option>
            {groups.map(({ id, label }) => <option key={id} value={id}>{label}</option>)}
            {draft.groupId && !groups.some(({ id }) => id === draft.groupId) && <option value={draft.groupId}>{draft.groupId}</option>}
          </select></label>
          {error("groupId") && <div role="alert" className="field-error">{error("groupId")}</div>}
        </div>
        <div className="editor-field">
          <span>접속 방식</span>
          <MethodTiles value={draft.defaultMethod} disabled={locked} onSelect={editor.selectMethod} />
        </div>
        <div className="editor-method">
          {draft.defaultMethod === "local" && <LocalFields local={draft.local} edit={editor.editLocal} error={error} touch={touch} />}
          {draft.defaultMethod === "ssm" && ssmView}
          {draft.defaultMethod === "ssh" && <SshFields ssh={draft.ssh} edit={editor.editSsh} storedAuth={storedAuth} ssm={ssmView}
            error={error} touch={touch} />}
          {draft.defaultMethod === "s3" && <S3Fields s3={draft.s3} edit={editor.editS3} error={error} touch={touch} />}
          {desktop && (draft.defaultMethod === "rdp" || draft.defaultMethod === "vnc") && <DesktopFields kind={draft.defaultMethod}
            desktop={desktop} edit={editor.editDesktop} sshTargets={sshTargets} ssm={ssmView} error={error} touch={touch} />}
        </div>
      </fieldset>
      {sshRun && sshAttempt && sshIpc && <div className="ssh-test-terminal">
        <TerminalView key={sshRun.runId} active focusRequested label="SSH 시험 터미널" ipc={sshIpc} />
        <button type="button" onClick={() => void editor.cancelSshTest(sshAttempt)}>SSH 시험 닫기</button>
      </div>}
      <div className="editor-status">
        {status.kind === "testing" && <div role="status">시험 중</div>}
        {status.kind === "passed" && <div role="status">시험 성공</div>}
        {status.kind === "saving" && <div role="status">저장 중</div>}
        {status.kind === "closing" && !remoteCleanup && <div role="status">취소 중</div>}
        {remoteCleanup && <div role={remoteCleanup.kind === "cancelling" ? "status" : "alert"}>
          {remoteCleanup.kind === "cancelling" ? "시험 취소 중" : "종료 확인 불가"}: {remoteCleanup.target.profile} / {remoteCleanup.target.region} / {remoteCleanup.target.instanceId}
          {remoteCleanup.kind === "unconfirmed" && !locked && <button type="button" onClick={editor.retryCleanup}>종료 다시 확인</button>}
        </div>}
        {(status.kind === "testFailed" || (status.kind === "closeFailed" && !remoteCleanup)) && <div role="alert">{status.error}</div>}
        {saveError && <div role="alert">{saveError}</div>}
      </div>
      <footer className="editor-footer">
        {!desktop && <button type="button" disabled={!editor.canTest()} onClick={() => void editor.test()}>연결 시험</button>}
        <button type="button" className="primary" disabled={!editor.canSave()} onClick={() => void editor.save()}>저장</button>
        <button type="button" disabled={locked && status.kind !== "closeFailed"} onClick={close}>
          {status.kind === "closeFailed" && remoteCleanup ? "종료 다시 확인" : "취소"}
        </button>
      </footer>
    </dialog>
  );
}
