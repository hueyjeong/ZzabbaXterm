import { useLayoutEffect, useRef, useState, useSyncExternalStore } from "react";
import type { SavedConnections } from "./connectionIpc";
import { SsmSelectionView } from "../ssm/SsmSelectionView";
import { useSsmSelection } from "../ssm/ssm";
import { createSsmDiscovery } from "../ssm/ssmDiscovery";
import {
  createLocalConnectionWizard,
  type LocalConnectionActions,
  type LocalProfileDraft,
} from "./profiles";

export type LocalConnectionWizardProps = LocalConnectionActions & Readonly<{
  draftId: string;
  // 호출자가 현재 OS의 지원 셸을 initialDraft.target.local.shell에 넣는다.
  initialDraft: LocalProfileDraft;
  groups: SavedConnections["groups"];
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

function Wizard(props: LocalConnectionWizardProps) {
  const [wizard] = useState(() => createLocalConnectionWizard(props.draftId, props.initialDraft));
  const { step, draft, status } = useSyncExternalStore(wizard.subscribe, wizard.getSnapshot);
  const { target } = draft;
  const ssm = useSsmSelection(discovery, target.defaultMethod === "ssm" &&
    status.kind !== "closing" && status.kind !== "closed" && status.kind !== "closeFailed");
  const dialog = useRef<HTMLDialogElement>(null);
  useLayoutEffect(() => {
    const modal = dialog.current;
    modal?.showModal();
    return () => modal?.close();
  }, []);
  useLayoutEffect(() => wizard.abandon, [wizard]);

  if (status.kind === "closed") return null;
  const locked = status.kind === "saving" || status.kind === "closing" || status.kind === "closeFailed";
  const canSave = status.kind === "passed" || status.kind === "saveFailed";

  return (
    <dialog ref={dialog} className="connection-wizard" aria-label="연결 추가" onCancel={(event) => {
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
            <div><label>접속 방식 <select value={target.defaultMethod} onChange={(event) => {
              wizard.selectMethod(event.currentTarget.value === "ssm" ? "ssm" : "local");
            }}>
              <option value="local">로컬</option>
              <option value="ssm">AWS Systems Manager</option>
            </select></label></div>
            {target.defaultMethod === "local" && <>
              <div>셸: {target.local.shell === "powerShell" ? "PowerShell" : "Bash"}</div>
              <div><label>시작 디렉터리 <input value={target.local.startDirectory} onChange={event => wizard.edit("startDirectory", event.target.value)} /></label></div>
            </>}
          </>
        )}
        {step === "auth" && (target.defaultMethod === "local" ? <div>현재 사용자</div> : <SsmSelectionView state={ssm} />)}
        {step === "verify" && (
          <>
            <div>{draft.group.displayName} / {draft.target.displayName}</div>
            <div>{target.defaultMethod === "local" ? target.local.startDirectory : target.ssm &&
              `${target.ssm.profile} / ${target.ssm.region} / ${target.ssm.instanceId}`}</div>
            <button type="button" disabled={status.kind === "testing"} onClick={() => void wizard.test(props)}>연결 테스트</button>
            <button type="button" disabled={!canSave} onClick={() => void wizard.save(props)}>저장</button>
          </>
        )}
        {step !== "target" && <button type="button" onClick={() => wizard.go(-1)}>이전</button>}
        {step !== "verify" && <button type="button"
          disabled={step === "auth" && target.defaultMethod === "ssm" && !ssm.selection}
          onClick={() => wizard.go(1, ssm.selection)}>다음</button>}
      </fieldset>
      <button type="button" disabled={locked && status.kind !== "closeFailed"} onClick={() => void wizard.close(props)}>취소</button>
      {status.kind === "testing" && <div role="status">테스트 중</div>}
      {status.kind === "passed" && <div role="status">테스트 성공</div>}
      {status.kind === "saving" && <div role="status">저장 중</div>}
      {status.kind === "closing" && <div role="status">취소 중</div>}
      {"error" in status && <div role="alert">{status.error}</div>}
    </dialog>
  );
}
