import { useCallback, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState, type Ref } from "react";
import type { SavedConnections } from "../connections/connectionIpc";
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

  const reloginStatus = loggingIn && <p role="status">SSO 재로그인 중</p>;
  if (session) return <>
    <div inert={loggingIn}><S3StoragePanel session={session} actions={storageActions} refresh={refresh} onRelogin={() => void relogin()}
      editEnabled={active && !closing && !loggingIn} onEditFile={onEditFile && (opened => {
        const current = attempt.current;
        if (!current || current.id !== session.sessionId || current.closing) return false;
        onEditFile(opened);
      })} /></div>
    {reloginStatus}
    {problem && <StorageProblem problem={problem} session={session} />}
  </>;

  return <section className="s3-storage" aria-label="AWS S3">
    <header className="storage-heading"><h2>AWS S3</h2></header>
    <form className="storage-controls" onSubmit={event => { event.preventDefault(); if (selectedId) void open(selectedId); }}>
      {!pinnedId && <label>SSO 프로필<select value={selectedId ?? ""} disabled={closing || opening || loggingIn} onChange={event => {
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
      {(!pinnedId || (booted.current && !opening && !loggingIn)) && <button type="submit" disabled={!selectedId || closing || opening || loggingIn}>스토리지 연결</button>}
    </form>
    {opening && <p role="status">SSO 프로필 확인 중</p>}
    {reloginStatus}
    {problem && <StorageProblem problem={problem} />}
    {selectedId && attempt.current?.targetId === selectedId && attempt.current.needsLogin && !opening &&
      <button type="button" disabled={closing || opening || loggingIn} onClick={() => void relogin()}>SSO 재로그인</button>}
    {selectedId && problem && problem !== "unavailable" &&
      (problem.failure.kind === "retestRequired" || problem.failure.kind === "accountMismatch") &&
      <button type="button" onClick={() => onRetest(selectedId)}>연결 재시험</button>}
  </section>;
}
