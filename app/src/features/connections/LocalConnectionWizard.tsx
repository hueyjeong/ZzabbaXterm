import { useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type KeyboardEvent } from "react";
import { X } from "lucide-react";
import type { SavedConnections } from "./connectionIpc";
import { SsmSelectionView } from "../ssm/SsmSelectionView";
import { useSsmSelection } from "../ssm/ssm";
import { createSsmDiscovery } from "../ssm/ssmDiscovery";
import { SshMethodFields, SshAuthFields, SshReview } from "./SshFields";
import { TerminalView } from "../terminal/TerminalView";
import { sshTestIpc } from "../terminal/session";
import { IconButton, ProtocolIcon, type ProtocolKind } from "../ui/controls";
import {
  createLocalConnectionWizard,
  setStoredAuthAvailable,
  type DesktopInput,
  type LocalConnectionActions,
  type InitialWizardDraft,
} from "./profiles";
import "./wizard.css";

export type LocalConnectionWizardProps = LocalConnectionActions & Readonly<{
  draftId: string;
  initialDraft: InitialWizardDraft;
  groups: SavedConnections["groups"];
  // RDP·VNC의 SSH 경로로 고를 저장 대상이다. 직접 SSH와 Agent·키 파일 인증만 표시한다.
  targets?: SavedConnections["targets"];
  capabilities?: () => Promise<{ storedAuth: boolean }>;
}>;

export function LocalConnectionWizard(props: LocalConnectionWizardProps) {
  return <Wizard key={props.draftId} {...props} />;
}

const steps = [["target", "대상"], ["method", "접속 방식"], ["auth", "인증"], ["verify", "확인"]] as const;
type Method = "local" | "ssh" | "ssm" | "s3" | "rdp" | "vnc";
const methods: readonly (readonly [Method, string])[] = [
  ["local", "로컬"], ["ssh", "SSH"], ["ssm", "AWS SSM"], ["s3", "S3"], ["rdp", "RDP"], ["vnc", "VNC"],
];
const routeLabels = { direct: "직접", ssh: "SSH 터널", ssm: "AWS SSM" } as const;

const discovery = createSsmDiscovery();
const s3Fields = [["profile", "IAM Identity Center 프로필"], ["region", "버킷 리전"],
  ["bucket", "버킷"], ["prefix", "접두어 (선택)"]] as const;

function desktopHostLabel(kind: "rdp" | "vnc", route: DesktopInput["route"]["kind"]) {
  if (kind === "vnc") return route === "direct" ? "TLS 호스트" : null;
  return route === "ssh" ? "TLS 인증 DNS" : route === "ssm" ? "원격 호스트 DNS (선택)" : "서버 DNS";
}

function MethodTiles({ value, disabled, onSelect }: Readonly<{ value: Method; disabled: boolean; onSelect(method: Method): void }>) {
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
      <ProtocolIcon kind={method as ProtocolKind} />
      <span>{label}</span>
    </button>)}
  </div>;
}

function Wizard(props: LocalConnectionWizardProps) {
  const [wizard] = useState(() => createLocalConnectionWizard(props.draftId, props.initialDraft));
  const [storedAuth, setStoredAuth] = useState(false);
  useEffect(() => {
    let active = true;
    props.capabilities?.().then(
      value => {
        if (!active) return;
        setStoredAuth(value.storedAuth);
        setStoredAuthAvailable(value.storedAuth);
      },
      () => { if (active) setStoredAuthAvailable(false); },
    );
    return () => { active = false; };
  }, [props.capabilities]);
  const { step, draft, status, sshPin, remoteCleanup } = useSyncExternalStore(wizard.subscribe, wizard.getSnapshot);
  const { target } = draft;
  const desktopKind = target.defaultMethod === "rdp" || target.defaultMethod === "vnc" ? target.defaultMethod : null;
  const desktop = target.defaultMethod === "rdp" ? target.rdp : target.defaultMethod === "vnc" ? target.vnc : null;
  const useSsm = target.defaultMethod === "ssm" || ((step === "auth" || step === "verify")
    && ((target.defaultMethod === "ssh" && target.ssh.transport.kind === "ssm") || desktop?.route.kind === "ssm"));
  const ssm = useSsmSelection(discovery, useSsm &&
    status.kind !== "closing" && status.kind !== "closed" && status.kind !== "closeFailed", wizard.invalidateSsmSelection);
  const dialog = useRef<HTMLDialogElement>(null);
  useLayoutEffect(() => {
    const modal = dialog.current;
    modal?.showModal();
    return () => modal?.close();
  }, []);
  useLayoutEffect(() => wizard.abandon, [wizard]);
  const sshRun = status.kind === "testing" ? status.sshRun : undefined;
  const sshAttempt = status.kind === "testing" ? status.attempt : undefined;
  const sshIpc = useMemo(() => sshRun && sshAttempt && sshTestIpc(sshRun, () => wizard.cancelSshTest(sshAttempt)), [sshRun, sshAttempt, wizard]);
  const sshTargets = useMemo(() => (props.targets ?? []).flatMap(({ profile }) => profile.defaultMethod === "ssh"
    && profile.ssh.transport.kind === "direct" && (profile.ssh.auth.kind === "agent" || profile.ssh.auth.kind === "keyFile")
    && profile.id !== target.id ? [profile] : []), [props.targets, target.id]);

  if (status.kind === "closed") return null;
  const locked = status.kind === "saving" || status.kind === "closing" || status.kind === "closeFailed";
  const ssh = target.defaultMethod === "ssh" ? target.ssh : null;
  const canSave = status.kind === "passed" || status.kind === "saveFailed"
    || (storedAuth && ssh?.auth?.kind === "storedPassword")
    || (!!desktop && status.kind === "idle");
  const s3 = target.defaultMethod === "s3" ? target.s3 : null;
  const retestingS3 = props.initialDraft.targetRevision !== null && props.initialDraft.target.defaultMethod === "s3";
  const title = props.initialDraft.targetRevision !== null ? props.initialDraft.target.displayName : "새 연결";
  const stepIndex = steps.findIndex(([id]) => id === step);
  const hostLabel = desktopKind && desktop && desktopHostLabel(desktopKind, desktop.route.kind);
  const routeTarget = desktop?.route.kind === "ssh" ? desktop.route.targetId : "";
  const close = () => void wizard.close(props);

  return (
    <dialog ref={dialog} className="connection-wizard" aria-label={retestingS3 ? "S3 연결 재시험" : "연결 추가"} onCancel={(event) => {
      event.preventDefault();
      close();
    }}>
      <header className="wizard-header">
        <h2>{title}</h2>
        <IconButton icon={X} label="닫기" small disabled={locked && status.kind !== "closeFailed"} onClick={close} />
      </header>
      <ol className="wizard-steps">
        {steps.map(([id, label], index) => <li key={id} aria-current={id === step ? "step" : undefined}
          data-done={index < stepIndex || undefined}>{label}</li>)}
      </ol>
      <fieldset disabled={locked}>
        {step === "target" && (
          <>
            {props.groups.length > 0 && <div><label>그룹 선택 <select
              value={draft.groupRevision === null ? "" : draft.group.id} onChange={(event) => {
                const saved = props.groups.find(({ profile }) => profile.id === event.target.value);
                wizard.selectGroup(saved?.profile ?? props.initialDraft.group, saved?.revision ?? null);
              }}>
              <option value="">새 그룹</option>
              {props.groups.map(({ profile }) => <option key={profile.id} value={profile.id}>
                {profile.displayName}{props.groups.some(({ profile: other }) => other.id !== profile.id && other.displayName === profile.displayName) && ` (${profile.id})`}
              </option>)}
            </select></label></div>}
            {draft.groupRevision === null && <div><label>그룹 <input value={draft.group.displayName} onChange={event => wizard.edit("groupName", event.target.value)} /></label></div>}
            <div><label>표시명 <input value={draft.target.displayName} onChange={event => wizard.edit("targetName", event.target.value)} /></label></div>
          </>
        )}
        {step === "method" && (
          <>
            <MethodTiles value={target.defaultMethod} disabled={retestingS3} onSelect={wizard.selectMethod} />
            {target.defaultMethod === "local" && <>
              <div>셸: {target.local.shell === "powerShell" ? "PowerShell" : "Bash"}</div>
              <div><label>시작 디렉터리 <input value={target.local.startDirectory} onChange={event => wizard.edit("startDirectory", event.target.value)} /></label></div>
            </>}
            {ssh && <SshMethodFields ssh={ssh} edit={wizard.editSsh} />}
            {desktopKind && desktop && <>
              <div><label>경로 <select value={desktop.route.kind} onChange={event => {
                const kind = event.currentTarget.value;
                const host = desktopKind === "vnc" && kind !== "direct" ? "" : desktop.host;
                if (kind === "direct") wizard.editDesktop({ host, route: { kind } });
                if (kind === "ssh") wizard.editDesktop({ host, route: { kind, targetId: sshTargets[0]?.id ?? "" } });
                if (kind === "ssm") wizard.editDesktop({ host, route: { kind } });
              }}>
                {Object.entries(routeLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}
              </select></label></div>
              {hostLabel && <div><label>{hostLabel} <input value={desktop.host} maxLength={253} autoComplete="off"
                onChange={event => wizard.editDesktop({ host: event.target.value })} /></label></div>}
              <div><label>포트 <input inputMode="numeric" value={desktop.port} onChange={event => wizard.editDesktop({ port: event.target.value })} /></label></div>
              {desktopKind === "rdp" && <label className="wizard-check"><input type="checkbox" checked={desktop.tlsOnly}
                onChange={event => wizard.editDesktop({ tlsOnly: event.target.checked })} /> NLA를 지원하지 않는 서버(TLS 전용) 허용</label>}
            </>}
          </>
        )}
        {step === "auth" && <>
          {target.defaultMethod === "local" && <div>현재 사용자</div>}
          {useSsm && <SsmSelectionView state={ssm} />}
          {ssh && <SshAuthFields ssh={ssh} edit={wizard.editSsh} storedAuth={storedAuth} />}
          {s3 && s3Fields.map(([field, label]) => <div key={field}><label>{label} <input
            value={s3[field]} onChange={event => wizard.editS3(field, event.currentTarget.value)}
          /></label></div>)}
          {desktop?.route.kind === "direct" && <div>직접 연결</div>}
          {desktop?.route.kind === "ssh" && <div><label>SSH 대상 <select value={routeTarget}
            onChange={event => wizard.editDesktop({ route: { kind: "ssh", targetId: event.currentTarget.value } })}>
            <option value="">{sshTargets.length ? "SSH 대상 선택" : "저장된 직접 SSH 대상 없음"}</option>
            {sshTargets.map(profile => <option key={profile.id} value={profile.id}>{profile.displayName}</option>)}
            {routeTarget && !sshTargets.some(profile => profile.id === routeTarget) && <option value={routeTarget}>{routeTarget}</option>}
          </select></label></div>}
        </>}
        {step === "verify" && (
          <>
            <div>{draft.group.displayName} / {target.displayName}</div>
            {target.defaultMethod === "local" && <div>{target.local.startDirectory}</div>}
            {target.defaultMethod === "ssm" && target.ssm && <dl aria-label="SSM 연결 대상">
              <div><dt>AWS 프로필</dt><dd>{target.ssm.profile}</dd></div>
              <div><dt>리전</dt><dd>{target.ssm.region}</dd></div>
              <div><dt>인스턴스 ID</dt><dd>{target.ssm.instanceId}</dd></div>
            </dl>}
            {s3 && <dl aria-label="S3 연결 대상">
              {s3Fields.map(([field, label]) => <div key={field}><dt>{label}</dt><dd>{s3[field]}</dd></div>)}
            </dl>}
            {desktopKind && desktop && <dl aria-label={`${desktopKind.toUpperCase()} 연결 대상`}>
              <div><dt>경로</dt><dd>{routeLabels[desktop.route.kind]}</dd></div>
              {hostLabel && desktop.host && <div><dt>{hostLabel}</dt><dd>{desktop.host}</dd></div>}
              <div><dt>포트</dt><dd>{desktop.port}</dd></div>
              {desktopKind === "rdp" && <div><dt>TLS 전용 허용</dt><dd>{desktop.tlsOnly ? "예" : "아니요"}</dd></div>}
              {desktop.route.kind === "ssh" && <div><dt>SSH 대상</dt><dd>
                {sshTargets.find(profile => profile.id === routeTarget)?.displayName ?? routeTarget}</dd></div>}
              {desktop.route.kind === "ssm" && "profile" in desktop.route && <>
                <div><dt>AWS 프로필</dt><dd>{desktop.route.profile}</dd></div>
                <div><dt>리전</dt><dd>{desktop.route.region}</dd></div>
                <div><dt>인스턴스 ID</dt><dd>{desktop.route.instanceId}</dd></div>
              </>}
            </dl>}
            {ssh && <>
              <SshReview ssh={ssh} />
              <button type="button" disabled={!props.verifySshHost || sshPin.kind === "checking" || sshPin.kind === "verified"}
                onClick={() => void wizard.verifySshHost(props)}>호스트 키 선택 및 등록</button>
              {!props.verifySshHost && <div role="status">호스트 키 확인 사용 불가</div>}
              {sshPin.kind === "checking" && <div role="status">호스트 키 확인 중</div>}
              {sshPin.kind === "verified" && <div role="status">호스트 키 확인 완료</div>}
              {sshPin.kind === "failed" && <div role="alert">{sshPin.error}</div>}
            </>}
            <div className="wizard-actions">
              {!desktop && <button type="button" disabled={status.kind === "testing" || !!remoteCleanup}
                onClick={() => void wizard.test(props)}>연결 테스트</button>}
              <button type="button" disabled={!canSave} onClick={() => void wizard.save(props)}>저장</button>
            </div>
            {sshRun && sshAttempt && sshIpc && <div className="ssh-test-terminal">
              <TerminalView key={sshRun.runId} active focusRequested label="SSH 시험 터미널" ipc={sshIpc} />
              <button type="button" onClick={() => void wizard.cancelSshTest(sshAttempt)}>SSH 시험 닫기</button>
            </div>}
          </>
        )}
      </fieldset>
      {status.kind === "testing" && <div role="status">테스트 중</div>}
      {status.kind === "passed" && <div role="status">테스트 성공</div>}
      {status.kind === "saving" && <div role="status">저장 중</div>}
      {status.kind === "closing" && !remoteCleanup && <div role="status">취소 중</div>}
      {remoteCleanup && <div role={remoteCleanup.kind === "cancelling" ? "status" : "alert"}>
        {remoteCleanup.kind === "cancelling" ? "시험 취소 중" : "종료 확인 불가"}: {remoteCleanup.target.profile} / {remoteCleanup.target.region} / {remoteCleanup.target.instanceId}
        {remoteCleanup.kind === "unconfirmed" && !locked && <button type="button" onClick={wizard.retryCleanup}>종료 다시 확인</button>}
      </div>}
      {"error" in status && !(status.kind === "closeFailed" && remoteCleanup) && <div role="alert">{status.error}</div>}
      <footer className="wizard-footer">
        {step !== "target" && <button type="button" disabled={locked || sshPin.kind === "checking"} onClick={() => wizard.go(-1)}>이전</button>}
        {step !== "verify" && <button type="button" className="primary"
          disabled={locked || (step === "auth" && ((useSsm && !ssm.selection) || (!!ssh && !wizard.canReviewSsh(ssm.selection))
            || (!!s3 && !wizard.canReviewS3()) || (!!desktop && !wizard.canReviewDesktop(ssm.selection))))}
          onClick={() => wizard.go(1, ssm.selection)}>다음</button>}
        <button type="button" disabled={locked && status.kind !== "closeFailed"} onClick={close}>
          {status.kind === "closeFailed" && remoteCleanup ? "종료 다시 확인" : "취소"}
        </button>
      </footer>
    </dialog>
  );
}
