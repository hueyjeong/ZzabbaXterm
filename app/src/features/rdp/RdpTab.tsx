import { useLayoutEffect, useRef, useState, type SubmitEvent } from "react";
import { RemoteDesktopView } from "../remoteDesktop/RemoteDesktopView";
import type { DesktopTunnelSource } from "../tunnels/tunnelIpc";
import { connect, type Capabilities, type Connection } from "./session";

const noChannels: Capabilities = { copy: false, paste: false, resize: false };

export function RdpTab({ active, label, source }: { readonly active: boolean; readonly label: string; readonly source?: DesktopTunnelSource }) {
  const cancel = useRef<(() => void) | undefined>(undefined);
  const [host, setHost] = useState("");
  const [port, setPort] = useState("3389");
  const [opening, setOpening] = useState<{ host: string; port: number; forwardedAddress: string | undefined }>();
  const [connection, setConnection] = useState<Connection>();
  const [channels, setChannels] = useState(noChannels);
  const [ready, setReady] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const destination = source?.destination;
  const forwardedAddress = destination && `${destination.host.includes(":") ? `[${destination.host}]` : destination.host}:${destination.port}`;

  useLayoutEffect(() => () => cancel.current?.(), []);

  function reset() {
    cancel.current?.();
    cancel.current = undefined;
    setOpening(undefined);
    setConnection(undefined);
    setChannels(noChannels);
    setReady(false);
    setNotice("");
  }

  function submit(event: SubmitEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!event.nativeEvent.isTrusted || !active || cancel.current) return;
    setError("");
    const hostName = host.trim();
    const draft = source ? { host: hostName, port: source.destination.port, source } : { host: hostName, port: Number(port) };
    setOpening({ host: source ? hostName.toLowerCase() : hostName, port: draft.port, forwardedAddress });
    cancel.current = connect(draft, setConnection,
      setChannels, setReady, setNotice, (message) => {
        reset();
        setError(message);
      });
  }

  return <section aria-label={label}>
    {!(opening || connection) && <form className="remote-desktop-toolbar" onSubmit={submit}>
      {forwardedAddress && <span>포워드 목적지: {forwardedAddress}</span>}
      <label>{source ? "TLS 인증 DNS" : "서버 DNS"} <input value={host} onChange={(event) => setHost(event.target.value)} required maxLength={253} /></label>
      {!source && <label>포트 <input type="number" value={port} min={1} max={65535} onChange={(event) => setPort(event.target.value)} required /></label>}
      <button type="submit" disabled={!active}>{source ? "터널 연결" : "직접 연결"}</button>
    </form>}
    {opening && <div className="remote-desktop-toolbar">
      <span>{opening.forwardedAddress ? <>포워드 목적지: {opening.forwardedAddress} · TLS 인증 DNS: {opening.host} · NLA</> : `${opening.host}:${opening.port} · NLA`}</span>
      {!ready && <span role="status">연결 중</span>}
      <button type="button" onClick={reset}>연결 종료</button>
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
    {notice && <div role="status">{notice}</div>}
    {error && <div role="alert">{error}</div>}
    {connection && <RemoteDesktopView key={connection.sessionId} session={connection} active={active} label={`${label} 화면`} />}
  </section>;
}
