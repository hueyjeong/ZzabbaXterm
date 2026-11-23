import type { LocalConnectionWizardState, SshAuth, SshInput } from "./profiles";

type Edit = LocalConnectionWizardState["editSsh"];
type Props = Readonly<{ ssh: SshInput; edit: Edit }>;

const authLabels = {
  agent: "SSH Agent",
  keyFile: "키 파일",
  manualPassword: "직접 입력",
  storedPassword: "저장된 인증 사용",
} satisfies Record<SshAuth["kind"], string>;

export function SshMethodFields({ ssh, edit }: Props) {
  return <>
    <div><label>서버 호스트 <input value={ssh.host} onChange={event => edit({ host: event.target.value })} /></label></div>
    <div><label>SSH 포트 <input inputMode="numeric" value={ssh.port} onChange={event => edit({ port: event.target.value })} /></label></div>
    <div><label>SSH 계정 <input value={ssh.account} onChange={event => edit({ account: event.target.value })} /></label></div>
    <div><label>전송 방식 <select value={ssh.transport.kind} onChange={event => {
      edit({ transport: event.currentTarget.value === "ssm" ? { kind: "ssm" } : { kind: "direct" } });
    }}>
      <option value="direct">직접 SSH</option>
      <option value="ssm" disabled>SSH-over-SSM (사용 불가)</option>
    </select></label></div>
    <div><label>호스트 키 SHA256 지문 <input value={ssh.hostKeyFingerprint} onChange={event => edit({ hostKeyFingerprint: event.target.value })} /></label></div>
  </>;
}

export function SshAuthFields({ ssh: { auth }, edit }: Props) {
  return <>
    <div><label>SSH 인증 방식 <select value={auth?.kind ?? ""} onChange={event => {
      switch (event.currentTarget.value) {
        case "agent": edit({ auth: { kind: "agent", publicKeyPath: "" } }); break;
        case "keyFile": edit({ auth: { kind: "keyFile", privateKeyPath: "" } }); break;
        case "manualPassword": edit({ auth: { kind: "manualPassword" } }); break;
        case "storedPassword": edit({ auth: { kind: "storedPassword", itemId: "" } }); break;
        default: edit({ auth: null });
      }
    }}>
      <option value="">인증 방식 선택</option>
      {Object.entries(authLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}
    </select></label></div>
    {auth?.kind === "agent" && <div><label>Agent 공개 키 파일 경로 <input value={auth.publicKeyPath}
      onChange={event => edit({ auth: { kind: "agent", publicKeyPath: event.target.value } })} /></label></div>}
    {auth?.kind === "keyFile" && <div><label>개인 키 파일 경로 <input value={auth.privateKeyPath}
      onChange={event => edit({ auth: { kind: "keyFile", privateKeyPath: event.target.value } })} /></label></div>}
    {auth?.kind === "storedPassword" && <div><label>저장 암호 항목 ID <input value={auth.itemId}
      onChange={event => edit({ auth: { kind: "storedPassword", itemId: event.target.value } })} /></label></div>}
  </>;
}

export function SshReview({ ssh }: Readonly<{ ssh: SshInput }>) {
  const { auth, transport } = ssh;
  return <dl aria-label="SSH 연결 대상" style={{ overflowWrap: "anywhere" }}>
    <div><dt>서버 호스트</dt><dd>{ssh.host}</dd></div>
    <div><dt>SSH 포트</dt><dd>{ssh.port}</dd></div>
    <div><dt>SSH 계정</dt><dd>{ssh.account}</dd></div>
    <div><dt>호스트 키 SHA256 지문</dt><dd>{ssh.hostKeyFingerprint}</dd></div>
    <div><dt>전송 방식</dt><dd>{transport.kind === "direct" ? "직접 SSH" : "SSH-over-SSM"}</dd></div>
    {transport.kind === "ssm" && "profile" in transport && <>
      <div><dt>SSO 프로필</dt><dd>{transport.profile}</dd></div>
      <div><dt>리전</dt><dd>{transport.region}</dd></div>
      <div><dt>인스턴스 ID</dt><dd>{transport.instanceId}</dd></div>
    </>}
    {auth && <div><dt>SSH 인증 방식</dt><dd>{authLabels[auth.kind]}</dd></div>}
    {auth?.kind === "agent" && <div><dt>Agent 공개 키 파일 경로</dt><dd>{auth.publicKeyPath}</dd></div>}
    {auth?.kind === "keyFile" && <div><dt>개인 키 파일 경로</dt><dd>{auth.privateKeyPath}</dd></div>}
    {auth?.kind === "storedPassword" && <div><dt>저장 암호 항목 ID</dt><dd>{auth.itemId}</dd></div>}
  </dl>;
}
