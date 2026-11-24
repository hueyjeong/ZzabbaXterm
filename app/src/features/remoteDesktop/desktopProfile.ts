import type { RdpSettings, VncSettings } from "../connections/profiles";
import { tunnelIpc, type DesktopTunnelSource, type TunnelRequest, type TunnelStatus } from "../tunnels/tunnelIpc";

export type DesktopProfile = Readonly<{ id: string; displayName: string } & (
  | { defaultMethod: "rdp"; rdp: RdpSettings }
  | { defaultMethod: "vnc"; vnc: VncSettings }
)>;

/** RdpTab·VncView 입력칸에 미리 채울 값이다. 터널 경로에서 포트는 source의 목적지가 대신한다. */
export type DesktopPreset = Readonly<{ host: string; port: number; tlsOnly?: boolean }>;

export type OpenedDesktop = Readonly<{
  kind: "rdp" | "vnc";
  label: string;
  preset: DesktopPreset;
  source?: DesktopTunnelSource;
  /** 화면 탭을 닫을 때 부른다. 경로용 터널 탭 핸들을 닫으며 직접 경로에서는 아무것도 하지 않는다. */
  close(): Promise<void>;
}>;

type Options = Readonly<{
  signal?: AbortSignal;
  ipc?: Pick<typeof tunnelIpc, "open" | "start" | "status" | "close">;
  listenPort?: () => number;
  pollMs?: number;
  timeoutMs?: number;
}>;

/** 경로 터널을 열다 실패했고 그 터널의 종료도 확인하지 못했다. 호출자는 handle로 종료를 다시 확인할 탭을 남긴다. */
export class DesktopRouteError extends Error {
  constructor(message: string, readonly handle: string, readonly targetId: string, readonly method: "ssh" | "ssm" = "ssh") { super(message); }
}

function ephemeralPort() {
  const [value] = crypto.getRandomValues(new Uint16Array(1));
  return 49152 + (value % 16384);
}

const wait = (ms: number, signal?: AbortSignal) => new Promise<void>((resolve, reject) => {
  const timer = setTimeout(done, ms);
  function done() { signal?.removeEventListener("abort", stop); resolve(); }
  function stop() { clearTimeout(timer); reject(new Error("원격 화면 열기가 취소되었습니다")); }
  signal?.addEventListener("abort", stop, { once: true });
});

/**
 * 저장된 RDP·VNC 프로필을 열 정보로 바꾼다. SSH 경로는 그 SSH 대상의 서버 루프백으로 로컬 전달을 열고,
 * SSM 경로는 프로필 자신을 경로 대상으로 공식 SSM 포트 전달을 연다(RDP는 TLS 인증 이름을 노드에서 접속할
 * 원격 호스트로, VNC는 노드 자신의 서비스로). Rust가 준비 완료로 확인한 전달만 source로 돌려준다.
 * 서비스 연결은 탭에서 사용자가 시작한다.
 */
export async function openDesktopProfile(profile: DesktopProfile, options: Options = {}): Promise<OpenedDesktop> {
  const { signal, ipc = tunnelIpc, listenPort = ephemeralPort, pollMs = 300, timeoutMs = 120_000 } = options;
  const kind = profile.defaultMethod;
  const settings = profile.defaultMethod === "rdp" ? profile.rdp : profile.vnc;
  const label = profile.displayName;
  const preset: DesktopPreset = profile.defaultMethod === "rdp"
    ? { host: profile.rdp.host, port: profile.rdp.port, tlsOnly: profile.rdp.tlsOnly }
    : { host: profile.vnc.host, port: profile.vnc.port };
  const { route } = settings;
  if (route.kind === "direct") return { kind, label, preset, close: async () => { } };

  const method = route.kind;
  const routeTarget = route.kind === "ssh" ? route.targetId : profile.id;
  const request: TunnelRequest = route.kind === "ssh"
    ? { direction: "local", listen: listenPort(), host: "127.0.0.1", port: settings.port, protocol: kind }
    : profile.defaultMethod === "rdp"
      ? { direction: "ssm", listen: listenPort(), port: settings.port, host: profile.rdp.host, protocol: kind }
      : { direction: "ssm", listen: listenPort(), port: settings.port, protocol: kind };
  signal?.throwIfAborted();
  const handle = await ipc.open(routeTarget, `${label} · ${kind.toUpperCase()} 경로`);
  const close = async () => { await ipc.close(handle); };
  try {
    signal?.throwIfAborted();
    const id = await ipc.start(handle, request);
    if (id === null) throw new Error(`${method.toUpperCase()} 터널 승인이 취소되었습니다`);
    const deadline = Date.now() + timeoutMs;
    for (; ;) {
      signal?.throwIfAborted();
      const tunnel: TunnelStatus | undefined = (await ipc.status(handle)).find(item => item.id === id);
      if (!tunnel) throw new Error("터널 탭이 닫혔습니다");
      if (tunnel.state === "failed" || tunnel.state === "disconnected") throw new Error(tunnel.error ?? "터널을 시작할 수 없습니다");
      if (tunnel.state === "ready") {
        const desktop = tunnel.desktop;
        if (desktop?.protocol !== kind) throw new Error("승인된 터널 서비스가 일치하지 않습니다");
        return {
          kind, label, preset, close,
          source: { tabHandle: handle, tunnelId: id, targetId: desktop.targetId, destination: desktop.destination },
        };
      }
      if (Date.now() >= deadline) throw new Error("터널 준비 시간이 초과되었습니다");
      await wait(pollMs, signal);
    }
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    try { await close(); } catch (failure) { throw new DesktopRouteError(`${reason} · 터널 종료 확인 불가: ${String(failure)}`, handle, routeTarget, method); }
    throw error instanceof Error ? error : new Error(reason);
  }
}
