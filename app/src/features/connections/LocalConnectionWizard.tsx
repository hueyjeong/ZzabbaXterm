import { useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import type { SavedConnections } from "./connectionIpc";
import { SsmSelectionView } from "../ssm/SsmSelectionView";
import { useSsmSelection } from "../ssm/ssm";
import { createSsmDiscovery } from "../ssm/ssmDiscovery";
import { SshMethodFields, SshAuthFields, SshReview } from "./SshFields";
import { TerminalView } from "../terminal/TerminalView";
import { sshTestIpc } from "../terminal/session";
import {
  createLocalConnectionWizard,
  setStoredAuthAvailable,
  type LocalConnectionActions,
  type InitialWizardDraft,
} from "./profiles";

export type LocalConnectionWizardProps = LocalConnectionActions & Readonly<{
  draftId: string;
  initialDraft: InitialWizardDraft;
  groups: SavedConnections["groups"];
  capabilities?: () => Promise<{ storedAuth: boolean }>;
}>;

export function LocalConnectionWizard(props: LocalConnectionWizardProps) {
  return <Wizard key={props.draftId} {...props} />;
}

const titles = {
  target: "대상",
  method: "접속 방식",
  auth: "인증",
  verify: "연결 테스트·저장",
} as const;

const discovery = createSsmDiscovery();
const s3Fields = [["profile", "IAM Identity Center 프로필"], ["region", "버킷 리전"],
  ["bucket", "버킷"], ["prefix", "접두어 (선택)"]] as const;

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
  const useSsm = target.defaultMethod === "ssm" || (target.defaultMethod === "ssh" && target.ssh.transport.kind === "ssm" && (step === "auth" || step === "verify"));
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

  if (status.kind === "closed") return null;
  const locked = status.kind === "saving" || status.kind === "closing" || status.kind === "closeFailed";
  const ssh = target.defaultMethod === "ssh" ? target.ssh : null;
  const canSave = status.kind === "passed" || status.kind === "saveFailed"
    || (storedAuth && ssh?.auth?.kind === "storedPassword" && sshPin.kind === "verified");
  const s3 = target.defaultMethod === "s3" ? target.s3 : null;
  const retestingS3 = props.initialDraft.targetRevision !== null && props.initialDraft.target.defaultMethod === "s3";

  return (
    <dialog ref={dialog} className="connection-wizard" aria-label={retestingS3 ? "S3 연결 재시험" : "연결 추가"} onCancel={(event) => {
      event.preventDefault();
      void wizard.close(props);
    }}>
      <fieldset disabled={locked}>
        <legend>{titles[step]}</legend>
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
            <div><label>접속 방식 <select value={target.defaultMethod} disabled={retestingS3} onChange={(event) => {
              const method = event.currentTarget.value;
              if (method === "local" || method === "ssm" || method === "ssh" || method === "s3") wizard.selectMethod(method);
            }}>
              <option value="local">로컬</option>
              <option value="ssm">AWS Systems Manager</option>
              <option value="ssh">SSH</option>
              <option value="s3">S3</option>
            </select></label></div>
            {target.defaultMethod === "local" && <>
              <div>셸: {target.local.shell === "powerShell" ? "PowerShell" : "Bash"}</div>
              <div><label>시작 디렉터리 <input value={target.local.startDirectory} onChange={event => wizard.edit("startDirectory", event.target.value)} /></label></div>
            </>}
            {ssh && <SshMethodFields ssh={ssh} edit={wizard.editSsh} />}
          </>
        )}
        {step === "auth" && <>
          {target.defaultMethod === "local" && <div>현재 사용자</div>}
          {useSsm && <SsmSelectionView state={ssm} />}
          {ssh && <SshAuthFields ssh={ssh} edit={wizard.editSsh} storedAuth={storedAuth} />}
          {s3 && s3Fields.map(([field, label]) => <div key={field}><label>{label} <input
            value={s3[field]} onChange={event => wizard.editS3(field, event.currentTarget.value)}
          /></label></div>)}
        </>}
        {step === "verify" && (
          <>
            <div>{draft.group.displayName} / {target.displayName}</div>
            {target.defaultMethod === "local" && <div>{target.local.startDirectory}</div>}
            {target.defaultMethod === "ssm" && target.ssm && <dl aria-label="SSM 연결 대상">
              <div><dt>SSO 프로필</dt><dd>{target.ssm.profile}</dd></div>
              <div><dt>리전</dt><dd>{target.ssm.region}</dd></div>
              <div><dt>인스턴스 ID</dt><dd>{target.ssm.instanceId}</dd></div>
            </dl>}
            {s3 && <dl aria-label="S3 연결 대상">
              {s3Fields.map(([field, label]) => <div key={field}><dt>{label}</dt><dd>{s3[field]}</dd></div>)}
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
            <button type="button" disabled={status.kind === "testing" || !!remoteCleanup || (!!ssh && sshPin.kind !== "verified")}
              onClick={() => void wizard.test(props)}>연결 테스트</button>
            <button type="button" disabled={!canSave} onClick={() => void wizard.save(props)}>저장</button>
            {sshRun && sshAttempt && sshIpc && <div className="ssh-test-terminal">
              <TerminalView key={sshRun.runId} active focusRequested label="SSH 시험 터미널" ipc={sshIpc} />
              <button type="button" onClick={() => void wizard.cancelSshTest(sshAttempt)}>SSH 시험 닫기</button>
            </div>}
          </>
        )}
        {step !== "target" && <button type="button" disabled={sshPin.kind === "checking"} onClick={() => wizard.go(-1)}>이전</button>}
        {step !== "verify" && <button type="button"
          disabled={step === "auth" && ((useSsm && !ssm.selection) || (!!ssh && !wizard.canReviewSsh(ssm.selection)) || (!!s3 && !wizard.canReviewS3()))}
          onClick={() => wizard.go(1, ssm.selection)}>다음</button>}
      </fieldset>
      <button type="button" disabled={locked && status.kind !== "closeFailed"} onClick={() => void wizard.close(props)}>
        {status.kind === "closeFailed" && remoteCleanup ? "종료 다시 확인" : "취소"}
      </button>
      {status.kind === "testing" && <div role="status">테스트 중</div>}
      {status.kind === "passed" && <div role="status">테스트 성공</div>}
      {status.kind === "saving" && <div role="status">저장 중</div>}
      {status.kind === "closing" && !remoteCleanup && <div role="status">취소 중</div>}
      {remoteCleanup && <div role={remoteCleanup.kind === "cancelling" ? "status" : "alert"}>
        {remoteCleanup.kind === "cancelling" ? "시험 취소 중" : "종료 확인 불가"}: {remoteCleanup.target.profile} / {remoteCleanup.target.region} / {remoteCleanup.target.instanceId}
        {remoteCleanup.kind === "unconfirmed" && !locked && <button type="button" onClick={wizard.retryCleanup}>종료 다시 확인</button>}
      </div>}
      {"error" in status && !(status.kind === "closeFailed" && remoteCleanup) && <div role="alert">{status.error}</div>}
    </dialog>
  );
}
