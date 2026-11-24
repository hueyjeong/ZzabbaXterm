import type { SsmTarget } from "../ssm/ssm";

export type LocalShell = "powerShell" | "bash";

// groupId가 없으면 그룹 없는 최상위 세션이다.
type TargetIdentity = Readonly<{ id: string; groupId?: string; displayName: string }>;
type LocalMethod = Readonly<{
  defaultMethod: "local";
  local: Readonly<{ shell: LocalShell; startDirectory: string }>;
  ssm?: never;
  ssh?: never;
  s3?: never;
}>;
type SsmMethod<T = SsmTarget> = Readonly<{ defaultMethod: "ssm"; ssm: T; local?: never; ssh?: never; s3?: never }>;
type SshMethod<T = SshSettings> = Readonly<{ defaultMethod: "ssh"; ssh: T; local?: never; ssm?: never; s3?: never }>;
type S3Method = Readonly<{ defaultMethod: "s3"; s3: S3Settings; local?: never; ssm?: never; ssh?: never }>;
export type S3Settings = Readonly<{ profile: string; region: string; bucket: string; prefix: string }>;
type RdpMethod<T = RdpSettings> = Readonly<{ defaultMethod: "rdp"; rdp: T; local?: never; ssm?: never; ssh?: never; s3?: never; vnc?: never }>;
type VncMethod<T = VncSettings> = Readonly<{ defaultMethod: "vnc"; vnc: T; local?: never; ssm?: never; ssh?: never; s3?: never; rdp?: never }>;

// ssh는 저장된 직접 SSH 대상의 서버 루프백, ssm은 host가 비면 노드 자신, 있으면 노드에서 본 원격 호스트로 전달한다.
export type DesktopRoute =
  | Readonly<{ kind: "direct" }>
  | Readonly<{ kind: "ssh"; targetId: string }>
  | (Readonly<{ kind: "ssm" }> & SsmTarget);
export type RdpSettings = Readonly<{ host: string; port: number; tlsOnly: boolean; route: DesktopRoute }>;
export type VncSettings = Readonly<{ host: string; port: number; route: DesktopRoute }>;
export type DesktopInput = Readonly<{
  host: string;
  port: string;
  tlsOnly: boolean;
  route: DesktopRoute | Readonly<{ kind: "ssm"; profile?: never; region?: never; instanceId?: never }>;
}>;
export type DesktopKind = "rdp" | "vnc";

export type SshTransport = Readonly<{ kind: "direct" }> | (Readonly<{ kind: "ssm" }> & SsmTarget);
export type SshAuth =
  | Readonly<{ kind: "agent"; publicKeyPath: string }>
  | Readonly<{ kind: "keyFile"; privateKeyPath: string }>
  | Readonly<{ kind: "manualPassword" }>
  | Readonly<{ kind: "storedPassword"; itemId: string }>;
export type SshSettings = Readonly<{
  host: string;
  port: number;
  account: string;
  /** 입력하지 않으면 첫 연결에서 서버 키를 확인한다. */
  hostKeyFingerprint?: string;
  transport: SshTransport;
  auth: SshAuth;
}>;
export type SshInput = Omit<SshSettings, "port" | "transport" | "auth" | "hostKeyFingerprint"> & Readonly<{
  hostKeyFingerprint: string;
  port: string;
  auth: SshAuth | null;
  transport: SshTransport | Readonly<{ kind: "ssm"; profile?: never; region?: never; instanceId?: never }>;
}>;

/** Rust에 저장된 세션 형식이다. */
export type SavedTarget = TargetIdentity & (LocalMethod | SsmMethod | SshMethod | S3Method | RdpMethod | VncMethod);
/** 편집 창 입력이다. 숫자 필드는 문자열로, 아직 고르지 않은 선택은 null로 둔다. */
export type EditorTarget = TargetIdentity & (LocalMethod | SsmMethod<SsmTarget | null> | SshMethod<SshInput> | S3Method
  | RdpMethod<DesktopInput> | VncMethod<DesktopInput>);
export type ConnectionMethod = SavedTarget["defaultMethod"];

// version은 Rust 저장 스키마 버전이다. 새 세션은 targetRevision이 null이다.
export type TargetDraft = Readonly<{ version: 2; targetRevision: number | null; target: SavedTarget }>;

type DraftIdentity = Readonly<{ draftId: string; draftVersion: number }>;
// draftVersion은 이번 편집의 버전이다.
export type ConnectionTestDraft = DraftIdentity & Readonly<{ draft: TargetDraft }>;
export type ConnectionTestRun = DraftIdentity & Readonly<{ runId: string }>;
export type ConnectionRequest = Readonly<{ run: ConnectionTestRun; draft: TargetDraft }>;

export type ConnectionClose =
  | Readonly<{ kind: "cancelled"; draftId: string }>
  | Readonly<{ kind: "saved"; draftId: string; targetId: string }>;

export interface LocalConnectionActions {
  test(attempt: ConnectionTestDraft, interactive?: (run: ConnectionTestRun) => void): Promise<ConnectionTestRun>;
  cancelTest(attempt: ConnectionTestDraft): Promise<void>;
  // 같은 초안의 시험 성공 티켓으로 저장한다. Rust의 저장 성공만 이행하고 오류는 그대로 거절한다.
  save(request: ConnectionRequest): Promise<void>;
}

/** 지문은 선택 항목이다. 입력했을 때만 형식을 검사한다. */
export function fingerprintValid(fingerprint: string) {
  return fingerprint === "" || /^SHA256:[A-Za-z0-9+/]{42}[AEIMQUYcgkosw048]$/.test(fingerprint);
}

export function portValid(port: string) {
  return /^\d+$/.test(port) && Number(port) >= 1 && Number(port) <= 65535;
}

export function completeSsh(input: SshInput): SshSettings | null {
  const { host, account, hostKeyFingerprint, transport, auth } = input;
  const port = Number(input.port);
  if (!auth || !host.trim() || !account.trim() || !fingerprintValid(hostKeyFingerprint)
    || !portValid(input.port) || (transport.kind === "ssm" && !transport.instanceId)) return null;
  if ((auth.kind === "agent" && !auth.publicKeyPath.trim()) || (auth.kind === "keyFile" && !auth.privateKeyPath.trim())
    || (auth.kind === "storedPassword" && !auth.itemId.trim())) return null;
  // 빈 지문은 필드를 생략해 보낸다.
  return { host, port, account, ...(hostKeyFingerprint ? { hostKeyFingerprint } : {}), transport: transport as SshTransport, auth };
}

export function completeS3({ profile, region, bucket }: S3Settings) {
  return !!(profile.trim() && region.trim() && bucket.trim());
}

const dnsLabel = /^(?!-)[A-Za-z0-9-]{1,63}(?<!-)$/;
const ipv4 = (host: string) => /^\d{1,3}(\.\d{1,3}){3}$/.test(host) && host.split(".").every(part => Number(part) <= 255 && String(Number(part)) === part);
const ipv6 = (host: string) => {
  if (!host.includes(":") || !/^[0-9A-Fa-f:.]+$/.test(host)) return false;
  try { return new URL(`http://[${host}]/`).hostname.length > 2; } catch { return false; }
};
export const ipLiteral = (host: string) => ipv4(host) || ipv6(host);
// 숫자만으로 된 최종 라벨(2852039166, 127.1, 0x7f000001)은 리졸버가 IP로 해석하므로 이름으로 받지 않는다.
const numericTail = (host: string) => /^(\d+|0[xX][0-9A-Fa-f]*)$/.test(host.split(".").pop() ?? host);
export const dnsName = (host: string) => host.length <= 253 && !ipLiteral(host) && !numericTail(host) && host.split(".").every(label => dnsLabel.test(label));
/** IP 리터럴 또는 DNS 이름이다. */
export const hostAddress = (host: string) => ipLiteral(host) || dnsName(host);
// Rust가 다시 출력한 문자열과 같은 표준 표기 IP만 받는다. 대문자, 0 채움, 괄호, zone, 지정되지 않은 주소는 거부한다.
const canonicalIp = (host: string) => {
  if (host === "0.0.0.0" || host === "::") return false;
  if (ipv4(host)) return true;
  if (!ipv6(host)) return false;
  try { return new URL(`http://[${host}]/`).hostname === `[${host}]`; } catch { return false; }
};
/** RDP 서버 이름이다. 모든 경로에서 DNS 이름 또는 표준 표기 IP를 받는다. */
export const rdpHost = (host: string) => dnsName(host) || canonicalIp(host);

export const desktopPorts = { rdp: 3389, vnc: 5900 } as const;

/** 호스트 입력이 필요한지 정한다. VNC는 터널 경로에서 터널 끝 호스트의 서비스만 열어 호스트를 받지 않는다. */
export function desktopHostUsed(kind: DesktopKind, route: DesktopInput["route"]["kind"]) {
  return kind === "rdp" || route === "direct";
}

// Rust 검증과 같은 규칙이다. RDP는 모든 경로에서 DNS 이름 또는 표준 표기 IP를 받는다.
export function completeDesktop(kind: "rdp", input: DesktopInput): RdpSettings | null;
export function completeDesktop(kind: "vnc", input: DesktopInput): VncSettings | null;
export function completeDesktop(kind: DesktopKind, input: DesktopInput): RdpSettings | VncSettings | null;
export function completeDesktop(kind: DesktopKind, input: DesktopInput): RdpSettings | VncSettings | null {
  const { route, tlsOnly } = input;
  const host = input.host.trim();
  const port = Number(input.port);
  if (!portValid(input.port)) return null;
  if (route.kind === "ssm" && !route.instanceId) return null;
  if (route.kind === "ssh" && !/^[A-Za-z0-9_][A-Za-z0-9_-]*$/.test(route.targetId)) return null;
  const valid = kind === "rdp" ? rdpHost(host) : route.kind === "direct" ? hostAddress(host) : !host;
  if (!valid) return null;
  const complete = route as DesktopRoute;
  return kind === "rdp" ? { host, port, tlsOnly, route: complete } : { host, port, route: complete };
}

export function desktopInput(settings: RdpSettings | VncSettings): DesktopInput {
  return { host: settings.host, port: String(settings.port), tlsOnly: "tlsOnly" in settings && settings.tlsOnly, route: settings.route };
}

export function sshInput(settings: SshSettings): SshInput {
  return { ...settings, port: String(settings.port), hostKeyFingerprint: settings.hostKeyFingerprint ?? "" };
}
