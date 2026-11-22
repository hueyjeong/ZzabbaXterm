import { useEffect, useRef, useState, type FormEvent } from "react";
import { tunnelIpc, type DesktopTunnelSource, type TunnelRequest, type TunnelStatus } from "./tunnelIpc";
import "./tunnel.css";

type CloseState = { kind: "closing" } | { kind: "unconfirmed"; error: string };
type Props = Readonly<{
  tab: string;
  targetId: string;
  method: "ssh" | "ssm";
  closeState?: CloseState;
  onShare(targetId: string, tab: string, id: string): Promise<void>;
  onOpenDesktop(protocol: "rdp" | "vnc", source: DesktopTunnelSource): void;
}>;

const states = {
  starting: "준비 중", ready: "수신 준비됨", failed: "시작 실패", disconnected: "끊김",
} satisfies Record<TunnelStatus["state"], string>;

export function TunnelTab({ tab, targetId, method, closeState, onShare, onOpenDesktop }: Props) {
  const windows = /Windows/.test(navigator.userAgent);
  const [direction, setDirection] = useState<TunnelRequest["direction"]>("local");
  const [tunnels, setTunnels] = useState<readonly TunnelStatus[]>([]);
  const [pending, setPending] = useState(false);
  const busy = useRef(false);
  const [error, setError] = useState("");
  const [statusError, setStatusError] = useState("");
  const closing = closeState !== undefined;

  useEffect(() => {
    if (closing) return;
    let stopped = false;
    let timer: number | undefined;
    async function observe() {
      try {
        const snapshot = await tunnelIpc.status(tab);
        if (!stopped) { setTunnels(snapshot); setStatusError(""); }
      } catch (failure) {
        if (!stopped) setStatusError(String(failure));
      } finally {
        if (!stopped) timer = window.setTimeout(observe, 300);
      }
    }
    void observe();
    return () => { stopped = true; window.clearTimeout(timer); };
  }, [tab, closing]);

  async function run(action: () => Promise<unknown>) {
    if (closing || busy.current) return;
    busy.current = true;
    setPending(true);
    setError("");
    try { await action(); } catch (failure) { setError(String(failure)); }
    finally { busy.current = false; setPending(false); }
  }

  function start(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (method !== "ssh" || (windows && direction === "remote")) return;
    const fields = new FormData(event.currentTarget);
    const listen = Number(fields.get("listen"));
    const port = Number(fields.get("port"));
    const host = String(fields.get("host") ?? "").trim();
    const service = fields.get("protocol");
    const protocol = service === "rdp" || service === "vnc" ? service : undefined;
    const request: TunnelRequest = direction === "local" ? { direction, listen, host, port, protocol }
      : direction === "remote" ? { direction, listen, host, port }
        : { direction, listen };
    void run(() => tunnelIpc.start(tab, request));
  }

  return <section className="tunnel-panel" aria-label="터널">
    <h2>터널</h2>
    {closeState?.kind === "closing" && <p role="status">종료 중</p>}
    {closeState?.kind === "unconfirmed" && <p role="alert">{closeState.error}</p>}
    {method === "ssm" ? <p role="status">공식 SSM 포트 전달의 바이너리 보존을 확인할 수 없습니다</p> : <>
      <form onSubmit={start}>
        <fieldset disabled={closing || pending}>
          <label>방향 <select value={direction} onChange={event => {
            const next = event.target.value;
            if (next === "local" || next === "dynamic" || (next === "remote" && !windows)) setDirection(next);
          }}>
            <option value="local">-L 로컬</option>
            <option value="remote" disabled={windows}>-R 원격</option>
            <option value="dynamic">-D SOCKS</option>
          </select></label>
          <label>수신 포트 <input name="listen" type="number" min="1" max="65535" required /></label>
          {direction !== "dynamic" && <>
            <label>목적지 호스트 <input name="host" defaultValue="127.0.0.1" maxLength={253} required /></label>
            <label>목적지 포트 <input name="port" type="number" min="1" max="65535" required /></label>
          </>}
          {direction === "local" && <label>서비스 <select name="protocol">
            <option value="">TCP</option><option value="rdp">RDP</option><option value="vnc">VNC</option>
          </select></label>}
          <button type="submit">시작</button>
        </fieldset>
      </form>
      {windows && <p role="status">Windows에서 -R 원격 수신을 지원하지 않음</p>}
    </>}
    {!closing && error && <div role="alert">{error}</div>}
    {!closing && statusError && <div role="alert">상태 확인 실패: {statusError}</div>}
    <ul aria-label="이 탭의 터널">
      {tunnels.map(tunnel => {
        const desktop = !closing && !statusError && tunnel.state === "ready" ? tunnel.desktop : undefined;
        return <li key={tunnel.id}>
          <strong>{tunnel.route}</strong> <span role="status">{states[tunnel.state]}</span>
          <div>{tunnel.endpoint} · 소유 탭: {tunnel.owners.join(", ")}</div>
          {tunnel.error && <div role="alert">{tunnel.error}</div>}
          {(tunnel.state === "starting" || tunnel.state === "ready") &&
            <button type="button" disabled={closing || pending} onClick={() => void run(() => onShare(targetId, tab, tunnel.id))}>새 탭에 공유</button>}
          {(desktop?.protocol === "rdp" || desktop?.protocol === "vnc") &&
            <button type="button" onClick={() => onOpenDesktop(desktop.protocol, {
              tabHandle: tab, tunnelId: tunnel.id, targetId: desktop.targetId, destination: desktop.destination,
            })}>{desktop.protocol === "rdp" ? "RDP로 열기" : "VNC로 열기"}</button>}
        </li>;
      })}
    </ul>
  </section>;
}
