import { RotateCcw } from "lucide-react";
import { useEffect, useImperativeHandle, useLayoutEffect, useRef, useState, type Ref, type SubmitEvent } from "react";
import { RemoteDesktopView } from "../remoteDesktop/RemoteDesktopView";
import type { DesktopPreset } from "../remoteDesktop/desktopProfile";
import type { DesktopTunnelSource } from "../tunnels/tunnelIpc";
import { IconButton } from "../ui/controls";
import { connect, type Capabilities, type Connection } from "./session";

const noChannels: Capabilities = { copy: false, paste: false, resize: false };

type Draft = { host: string; port: number; tlsOnly: boolean; source?: DesktopTunnelSource };

function forwarded(source?: DesktopTunnelSource) {
  const destination = source?.destination;
  return destination && `${destination.host.includes(":") ? `[${destination.host}]` : destination.host}:${destination.port}`;
}

export interface RdpTabActions { requestClose(): Promise<boolean> }

export function RdpTab({ active, label, source, preset, autoStart, onSecurityMode, onReopenRoute, ref }: {
  readonly active: boolean; readonly label: string; readonly source?: DesktopTunnelSource; readonly preset?: DesktopPreset;
  /** 저장 프로필로 연 탭은 입력칸 없이 한 번 바로 연결한다. */
  readonly autoStart?: boolean;
  readonly onSecurityMode?: (tlsOnly: boolean) => void;
  /** 터널 경로가 끊겨 재연결할 때 Workspace가 같은 프로필의 경로를 다시 열어 새 source를 넘긴다. 실패하면 false다. */
  readonly onReopenRoute?: () => Promise<boolean>;
  readonly ref?: Ref<RdpTabActions>;
}) {
  const cancel = useRef<(() => Promise<void>) | undefined>(undefined);
  const last = useRef<Draft>(undefined);
  const autoStarted = useRef(false);
  const awaitingRoute = useRef(false);
  const [editing, setEditing] = useState(!(autoStart && preset));
  const [routeLost, setRouteLost] = useState(false);
  const [host, setHost] = useState(preset?.host ?? "");
  const [port, setPort] = useState(String(preset?.port ?? 3389));
  const [tlsOnly, setTlsOnly] = useState(preset?.tlsOnly ?? false);
  const [opening, setOpening] = useState<{ host: string; port: number; forwardedAddress: string | undefined; tlsOnly: boolean }>();
  const [connection, setConnection] = useState<Connection>();
  const [channels, setChannels] = useState(noChannels);
  const [ready, setReady] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const [closeStatus, setCloseStatus] = useState<"closing" | "unconfirmed">();
  const forwardedAddress = forwarded(source);
  const loginMode = opening?.tlsOnly ? "NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)" : "NLA";

  useLayoutEffect(() => () => { void cancel.current?.().catch(() => { }); }, []);
  useImperativeHandle(ref, () => ({ requestClose }));

  function reset() {
    setOpening(undefined);
    setConnection(undefined);
    setChannels(noChannels);
    setReady(false);
    setNotice("");
  }

  async function requestClose() {
    const close = cancel.current;
    if (!close) return true;
    reset();
    setCloseStatus("closing");
    try {
      await close();
      if (cancel.current === close) cancel.current = undefined;
      setCloseStatus(undefined);
      setError("");
      return true;
    } catch (error) {
      setCloseStatus("unconfirmed");
      setError(String(error));
      return false;
    }
  }

  function draftOf(hostName: string, tls: boolean): Draft {
    return source ? { host: hostName, port: source.destination.port, tlsOnly: tls, source } : { host: hostName, port: Number(port), tlsOnly: tls };
  }

  /** 승인·계정·인증서 확인은 Rust 네이티브 창이 맡는다. 여기서는 같은 탭의 중복 연결만 막는다. */
  function start(draft: Draft) {
    if (!active || cancel.current) return;
    last.current = draft;
    awaitingRoute.current = false;
    setEditing(false);
    setRouteLost(false);
    setError("");
    setOpening({ host: draft.source ? draft.host.toLowerCase() : draft.host, port: draft.port, forwardedAddress: forwarded(draft.source), tlsOnly: draft.tlsOnly });
    onSecurityMode?.(draft.tlsOnly);
    cancel.current = connect(draft, setConnection,
      setChannels, setReady, setNotice, (message, closed, kind) => {
        reset();
        if (closed) cancel.current = undefined;
        setCloseStatus(closed ? undefined : "unconfirmed");
        setError(message);
        setRouteLost(!!draft.source && kind === "transport");
      });
  }

  useEffect(() => {
    if (!autoStart || !preset || !active || autoStarted.current) return;
    let stopped = false;
    // StrictMode에서 즉시 해제된 첫 마운트는 연결을 시작하지 않는다.
    void Promise.resolve().then(() => {
      if (stopped || autoStarted.current) return;
      autoStarted.current = true;
      start(draftOf(preset.host, preset.tlsOnly ?? false));
    });
    return () => { stopped = true; };
  }, [autoStart, preset, active]);

  useEffect(() => {
    const draft = last.current;
    if (!awaitingRoute.current || !active || !source || !draft || draft.source === source) return;
    // 다시 연 경로가 준비된 뒤에만 연결한다.
    awaitingRoute.current = false;
    start({ ...draft, port: source.destination.port, source });
  }, [active, source]);

  function submit(event: SubmitEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!event.nativeEvent.isTrusted) return;
    start(draftOf(host.trim(), tlsOnly));
  }

  function reconnect() {
    const draft = last.current;
    if (!draft || awaitingRoute.current) return;
    if (routeLost && onReopenRoute) {
      awaitingRoute.current = true;
      void onReopenRoute().then((reopened) => { if (!reopened) awaitingRoute.current = false; });
      return;
    }
    start(draft);
  }

  const idle = !(opening || connection || closeStatus);
  return <section aria-label={label}>
    {idle && !editing && last.current && <div className="remote-desktop-toolbar">
      <IconButton icon={RotateCcw} label="다시 연결" small disabled={!active} onClick={reconnect} />
      <button type="button" onClick={() => { awaitingRoute.current = false; setEditing(true); }}>연결 설정 편집</button>
    </div>}
    {idle && editing && <form className="remote-desktop-toolbar" onSubmit={submit}>
      {forwardedAddress && <span>포워드 목적지: {forwardedAddress}</span>}
      <label>{source ? "TLS 인증 이름" : "서버 이름"} <input value={host} onChange={(event) => setHost(event.target.value)} required maxLength={253} /></label>
      {!source && <label>포트 <input type="number" value={port} min={1} max={65535} onChange={(event) => setPort(event.target.value)} required /></label>}
      <label><input type="checkbox" checked={tlsOnly} onChange={(event) => setTlsOnly(event.target.checked)} /> NLA를 지원하지 않는 서버(TLS 전용) 허용</label>
      <button type="submit" disabled={!active}>{source ? "터널 연결" : "직접 연결"}</button>
    </form>}
    {opening && <div className="remote-desktop-toolbar">
      <span>{opening.forwardedAddress ? <>포워드 목적지: {opening.forwardedAddress} · TLS 인증 이름: {opening.host} · {loginMode}</> : `${opening.host}:${opening.port} · ${loginMode}`}</span>
      {!ready && <span role="status">연결 중</span>}
      <button type="button" onClick={() => { void requestClose(); }}>연결 종료</button>
      {connection && <>
        <button type="button" disabled={!active || !ready || !channels.copy} onClick={(event) => {
          if (event.nativeEvent.isTrusted) { setNotice(""); connection.clipboard("copy"); }
        }}>복사</button>
        <button type="button" disabled={!active || !ready || !channels.paste} onClick={(event) => {
          if (event.nativeEvent.isTrusted) { setNotice(""); connection.clipboard("paste"); }
        }}>붙여넣기</button>
        <form className="remote-desktop-toolbar" onSubmit={(event) => {
          event.preventDefault();
          if (!event.nativeEvent.isTrusted || !active || !ready || !channels.resize) return;
          const size = new FormData(event.currentTarget);
          setNotice("");
          connection.resize(Number(size.get("width")), Number(size.get("height")));
        }}>
          <label>너비 <input name="width" type="number" defaultValue={1024} min={200} max={4096} step={2} required /></label>
          <label>높이 <input name="height" type="number" defaultValue={768} min={200} max={4096} required /></label>
          <button type="submit" disabled={!active || !ready || !channels.resize}>해상도 변경</button>
        </form>
      </>}
    </div>}
    {closeStatus === "closing" && <div role="status">종료 중</div>}
    {closeStatus === "unconfirmed" && <button type="button" onClick={() => { void requestClose(); }}>종료 다시 확인</button>}
    {notice && <div role="status">{notice}</div>}
    {error && <div role="alert">{error}</div>}
    {connection && <RemoteDesktopView key={connection.sessionId} session={connection} active={active} label={`${label} 화면`} />}
  </section>;
}
