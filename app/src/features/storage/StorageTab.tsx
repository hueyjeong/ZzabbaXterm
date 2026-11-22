import { useImperativeHandle, useRef, useState, type Ref } from "react";
import type { SavedConnections } from "../connections/connectionIpc";
import { S3StoragePanel } from "./S3StoragePanel";
import { StorageProblem, type Problem } from "./StorageStatus";
import { closeStorage, openStorage, reloginStorage, storageActions } from "./storageIpc";
import type { S3Session } from "./s3";

type Props = {
  profiles: SavedConnections | undefined;
  closing: boolean;
  closeRef: Ref<{ confirmClose(): Promise<void> }>;
  onRetest(targetId: string): void;
};

export function StorageTab({ profiles, closing, closeRef, onRetest }: Props) {
  const [targetId, setTargetId] = useState("");
  const [session, setSession] = useState<S3Session>();
  const [opening, setOpening] = useState(false);
  const [problem, setProblem] = useState<Problem>();
  const [loggingIn, setLoggingIn] = useState(false);
  const attempt = useRef<{ id: string; targetId: string; closing: boolean } | null>(null);
  const approved = profiles?.targets.filter(({ profile }) => profile.defaultMethod === "ssm") ?? [];
  const selected = approved.find(({ profile }) => profile.id === targetId);

  useImperativeHandle(closeRef, () => ({
    async confirmClose() {
      const current = attempt.current;
      if (!current) return;
      current.closing = true;
      await closeStorage(current.id);
      attempt.current = null;
    },
  }), []);

  async function open() {
    if (!selected || closing || opening || loggingIn) return;
    const previous = attempt.current;
    const current = { id: crypto.randomUUID(), targetId: selected.profile.id, closing: false };
    setOpening(true);
    setProblem(undefined);
    try {
      if (previous) await closeStorage(previous.id);
      if (previous?.closing) return;
      attempt.current = current;
      const result = await openStorage(selected.profile.id, current.id);
      if (current.closing || attempt.current !== current) return;
      if (result.ok) setSession(result.value);
      else {
        setProblem(result.error);
        if (result.error.failure.kind !== "ssoLoginRequired") attempt.current = null;
      }
    } catch {
      if (!current.closing && !previous?.closing) setProblem("unavailable");
    } finally {
      setOpening(false);
    }
  }

  async function relogin() {
    const current = attempt.current;
    if (!current || current.targetId !== targetId || !selected || current.closing || closing || opening || loggingIn) return;
    setLoggingIn(true);
    setProblem(undefined);
    try {
      const result = await reloginStorage(current.id);
      if (current.closing || attempt.current !== current) return;
      if (!result.ok) setProblem(result.error);
      if (session) await closeStorage(current.id);
      if (current.closing || attempt.current !== current) return;
      attempt.current = null;
      setSession(undefined);
    } catch {
      if (!current.closing) setProblem("unavailable");
    } finally {
      setLoggingIn(false);
    }
  }

  const reloginStatus = loggingIn && <p role="status">SSO 재로그인 중</p>;
  if (session) return <>
    <div inert={loggingIn}><S3StoragePanel session={session} actions={storageActions} onRelogin={() => void relogin()} /></div>
    {reloginStatus}
    {problem && <StorageProblem problem={problem} session={session} />}
  </>;

  return <section className="s3-storage" aria-label="AWS S3">
    <header className="storage-heading"><h2>AWS S3</h2></header>
    <form className="storage-controls" onSubmit={event => { event.preventDefault(); open(); }}>
      <label>SSO 프로필<select value={selected ? targetId : ""} disabled={closing || opening || loggingIn} onChange={event => {
        setTargetId(event.target.value);
        setProblem(undefined);
      }}>
        <option value="">{approved.length ? "프로필 선택" : "저장된 SSO 연결 없음"}</option>
        {approved.map(({ profile }) => profile.defaultMethod === "ssm" && <option key={profile.id} value={profile.id}>
          {profile.ssm.profile} · {profile.ssm.region} · {profile.displayName}
        </option>)}
      </select></label>
      <button type="submit" disabled={!selected || closing || opening || loggingIn}>스토리지 연결</button>
    </form>
    {opening && <p role="status">SSO 프로필 확인 중</p>}
    {reloginStatus}
    {problem && <StorageProblem problem={problem} />}
    {selected && problem && problem !== "unavailable" && problem.failure.kind === "ssoLoginRequired" &&
      <button type="button" disabled={closing || opening || loggingIn} onClick={() => void relogin()}>SSO 재로그인</button>}
    {selected && problem && problem !== "unavailable" &&
      (problem.failure.kind === "retestRequired" || problem.failure.kind === "accountMismatch") &&
      <button type="button" onClick={() => onRetest(selected.profile.id)}>연결 재시험</button>}
  </section>;
}
