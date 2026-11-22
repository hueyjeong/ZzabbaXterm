import { useImperativeHandle, useRef, useState, type Ref } from "react";
import type { SavedConnections } from "../connections/connectionIpc";
import { S3StoragePanel } from "./S3StoragePanel";
import { StorageProblem, type Problem } from "./StorageStatus";
import { closeStorage, openStorage, storageActions } from "./storageIpc";
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
  const attempt = useRef<Promise<S3Session | undefined> | null>(null);
  const approved = profiles?.targets.filter(({ profile }) => profile.defaultMethod === "ssm") ?? [];
  const selected = approved.find(({ profile }) => profile.id === targetId);

  useImperativeHandle(closeRef, () => ({
    async confirmClose() {
      const current = await attempt.current;
      if (current) await closeStorage(current.sessionId);
    },
  }), []);

  function open() {
    if (!selected || closing || attempt.current) return;
    setOpening(true);
    setProblem(undefined);
    const request = openStorage(selected.profile.id).then<S3Session | undefined, undefined>(result => {
      if (result.ok) {
        setSession(result.value);
        return result.value;
      }
      setProblem(result.error);
    }, () => { setProblem("unavailable"); return undefined; });
    attempt.current = request;
    void request.then(value => {
      if (!value) attempt.current = null;
      setOpening(false);
    });
  }

  if (session) return <S3StoragePanel session={session} actions={storageActions} />;

  return <section className="s3-storage" aria-label="AWS S3">
    <header className="storage-heading"><h2>AWS S3</h2></header>
    <form className="storage-controls" onSubmit={event => { event.preventDefault(); open(); }}>
      <label>SSO 프로필<select value={selected ? targetId : ""} disabled={closing || opening} onChange={event => {
        setTargetId(event.target.value);
        setProblem(undefined);
      }}>
        <option value="">{approved.length ? "프로필 선택" : "저장된 SSO 연결 없음"}</option>
        {approved.map(({ profile }) => profile.defaultMethod === "ssm" && <option key={profile.id} value={profile.id}>
          {profile.ssm.profile} · {profile.ssm.region} · {profile.displayName}
        </option>)}
      </select></label>
      <button type="submit" disabled={!selected || closing || opening}>스토리지 연결</button>
    </form>
    {opening && <p role="status">SSO 프로필 확인 중</p>}
    {problem && <StorageProblem problem={problem} />}
    {selected && problem && problem !== "unavailable" &&
      (problem.failure.kind === "retestRequired" || problem.failure.kind === "accountMismatch") &&
      <button type="button" onClick={() => onRetest(selected.profile.id)}>연결 재시험</button>}
  </section>;
}
