import { useCallback, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState, type Ref } from "react";
import { LogIn, Plug } from "lucide-react";
import type { SavedConnections } from "../connections/connectionIpc";
import { ProtocolIcon } from "../ui/controls";
import { S3StoragePanel } from "./S3StoragePanel";
import { StorageProblem, type Problem } from "./StorageStatus";
import { closeStorage, openStorage, reloginStorage, storageActions } from "./storageIpc";
import type { S3Session, StorageEditorOpen } from "./s3";

type Props = {
  profiles?: SavedConnections;
  targetId?: string;
  active?: boolean;
  closing: boolean;
  closeRef: Ref<{ confirmClose(): Promise<void> }>;
  onRetest(targetId: string): void;
  onEditFile?(opened: StorageEditorOpen): void;
};

export function StorageTab({ profiles, targetId: pinnedId, active = true, closing, closeRef, onRetest, onEditFile }: Props) {
  const [targetId, setTargetId] = useState("");
  const [session, setSession] = useState<S3Session>();
  const [opening, setOpening] = useState(false);
  const [problem, setProblem] = useState<Problem>();
  const [loggingIn, setLoggingIn] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const attempt = useRef<{ id: string; targetId: string; closing: boolean; needsLogin: boolean } | null>(null);
  const booted = useRef(false);
  const approved = profiles?.targets.filter(({ profile }) => profile.defaultMethod === "ssm" || profile.defaultMethod === "s3") ?? [];
  const selectedId = pinnedId ?? approved.find(({ profile }) => profile.id === targetId)?.profile.id;
  const loginTarget = useRef<string | undefined>(undefined);
  useLayoutEffect(() => {
    loginTarget.current = closing ? undefined : selectedId;
    return () => { loginTarget.current = undefined; };
  }, [closing, selectedId]);

  useImperativeHandle(closeRef, () => ({
    async confirmClose() {
      const current = attempt.current;
      if (!current) return;
      current.closing = true;
      await closeStorage(current.id);
      attempt.current = null;
    },
  }), []);

  const open = useCallback(async (id: string) => {
    if (closing || opening || loggingIn) return;
    const previous = attempt.current;
    const current = { id: crypto.randomUUID(), targetId: id, closing: false, needsLogin: false };
    setOpening(true);
    setProblem(undefined);
    try {
      if (previous) await closeStorage(previous.id);
      if (previous?.closing) return;
      attempt.current = current;
      const result = await openStorage(id, current.id);
      if (current.closing || attempt.current !== current) return;
      if (result.ok) setSession(result.value);
      else {
        setProblem(result.error);
        current.needsLogin = result.error.failure.kind === "ssoLoginRequired";
        if (!current.needsLogin) attempt.current = null;
      }
    } catch {
      if (!current.closing && !previous?.closing) setProblem("unavailable");
    } finally {
      setOpening(false);
    }
  }, [closing, opening, loggingIn]);

  useEffect(() => {
    if (!pinnedId || !active || closing || booted.current) return;
    booted.current = true;
    void open(pinnedId);
  }, [active, closing, open, pinnedId]);

  async function relogin() {
    const current = attempt.current;
    if (!current || current.targetId !== selectedId || current.closing || closing || opening || loggingIn) return;
    setLoggingIn(true);
    setProblem(undefined);
    try {
      const result = await reloginStorage(current.id);
      if (current.closing || attempt.current !== current || loginTarget.current !== current.targetId) return;
      if (!result.ok) setProblem(result.error);
      else if (session) setRefresh(value => value + 1);
      else attempt.current = null;
    } catch {
      if (!current.closing && attempt.current === current && loginTarget.current === current.targetId) setProblem("unavailable");
    } finally {
      setLoggingIn(false);
    }
  }

  const reloginStatus = loggingIn && <p role="status" className="storage-notice">SSO 재로그인 중</p>;
  if (session) return <div className="storage-host">
    {reloginStatus}
    {problem && <StorageProblem problem={problem} session={session} />}
    <div className="storage-host-body" inert={loggingIn}><S3StoragePanel session={session} actions={storageActions} refresh={refresh} onRelogin={() => void relogin()}
      editEnabled={active && !closing && !loggingIn} onEditFile={onEditFile && (opened => {
        const current = attempt.current;
        if (!current || current.id !== session.sessionId || current.closing) return false;
        onEditFile(opened);
      })} /></div>
  </div>;

  return <section className="storage-connect" aria-label="AWS S3">
    <form className="storage-card" onSubmit={event => { event.preventDefault(); if (selectedId) void open(selectedId); }}>
      <header className="storage-card-head"><ProtocolIcon kind="s3" /><h2>AWS S3</h2></header>
      {!pinnedId && <label className="ui-label">SSO 프로필<select className="ui-field" value={selectedId ?? ""} disabled={closing || opening || loggingIn} onChange={event => {
        setTargetId(event.target.value);
        setProblem(undefined);
      }}>
        <option value="">{approved.length ? "프로필 선택" : "저장된 SSO 연결 없음"}</option>
        {approved.map(({ profile }) => {
          const settings = profile.defaultMethod === "s3" ? profile.s3 : profile.defaultMethod === "ssm" ? profile.ssm : null;
          return settings && <option key={profile.id} value={profile.id}>
            {settings.profile} · {settings.region} · {profile.displayName}
          </option>;
        })}
      </select></label>}
      {opening && <p role="status" className="storage-busy">SSO 프로필 확인 중</p>}
      {loggingIn && <p role="status" className="storage-busy">SSO 재로그인 중</p>}
      {problem && <StorageProblem problem={problem} />}
      <div className="storage-actions">
        {selectedId && problem && problem !== "unavailable" &&
          (problem.failure.kind === "retestRequired" || problem.failure.kind === "accountMismatch") &&
          <button type="button" className="ui-button" onClick={() => onRetest(selectedId)}>연결 재시험</button>}
        {selectedId && attempt.current?.targetId === selectedId && attempt.current.needsLogin && !opening &&
          <button type="button" className="ui-button" disabled={closing || opening || loggingIn} onClick={() => void relogin()}><LogIn aria-hidden="true" />SSO 재로그인</button>}
        {(!pinnedId || (booted.current && !opening && !loggingIn)) &&
          <button type="submit" className="ui-button primary" disabled={!selectedId || closing || opening || loggingIn}><Plug aria-hidden="true" />스토리지 연결</button>}
      </div>
    </form>
  </section>;
}
