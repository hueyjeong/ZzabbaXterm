import { useLayoutEffect, useRef, useState, type FormEvent } from "react";
import { RemoteDesktopView } from "../remoteDesktop/RemoteDesktopView";
import { closeOwner, connect, createOwner, type Connection, type Owner } from "./session";

export function VncView({ active }: { readonly active: boolean }) {
  const owner = useRef<Promise<Owner> | null>(null);
  const cancel = useRef<(() => void) | null>(null);
  const busy = useRef(false);
  const mounted = useRef(false);
  const [session, setSession] = useState<Connection>();
  const [connecting, setConnecting] = useState(false);
  const [notice, setNotice] = useState("");

  useLayoutEffect(() => {
    if (active || !busy.current) return;
    const pending = owner.current;
    owner.current = null;
    cancel.current?.();
    cancel.current = null;
    if (pending) void pending.then(closeOwner).catch(() => { });
    busy.current = false;
    setConnecting(false);
  }, [active]);

  useLayoutEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      cancel.current?.();
      cancel.current = null;
      const current = owner.current;
      owner.current = null;
      if (current) void current.then(closeOwner).catch(() => { });
    };
  }, []);

  async function open(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!active || !event.nativeEvent.isTrusted || busy.current) return;
    const fields = new FormData(event.currentTarget);
    const draft = { host: String(fields.get("host") ?? "").trim(), port: Number(fields.get("port")), password: fields.get("auth") === "password" };
    busy.current = true;
    cancel.current?.();
    setSession(undefined);
    setNotice("");
    setConnecting(true);
    const current = owner.current ??= createOwner();
    const settle = () => { busy.current = false; setConnecting(false); };
    try {
      const id = await current;
      if (owner.current !== current) return;
      cancel.current = connect(id, draft, (connection) => { setSession(connection); settle(); }, setNotice,
        (message) => { setNotice(message); settle(); });
    } catch (error) {
      if (mounted.current && owner.current === current) {
        owner.current = null;
        setNotice(String(error));
        settle();
      }
    }
  }

  function resize(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!active || !event.nativeEvent.isTrusted || !session) return;
    const fields = new FormData(event.currentTarget);
    session.resize(Number(fields.get("width")), Number(fields.get("height")));
  }

  return <section className="remote-desktop-view" aria-label="VNC" hidden={!active} inert={!active}>
    <form className="remote-desktop-toolbar" onSubmit={(event) => void open(event)}>
      <label>호스트 <input name="host" maxLength={253} required autoComplete="off" disabled={connecting} /></label>
      <label>포트 <input name="port" type="number" min={1} max={65535} defaultValue={5900} required disabled={connecting} /></label>
      <label>서비스 인증 <select name="auth" defaultValue="password" disabled={connecting}>
        <option value="password">VNC 비밀번호</option>
        <option value="none">없음</option>
      </select></label>
      <button type="submit" disabled={connecting}>{session ? "다시 연결" : "TLS 연결"}</button>
    </form>
    {connecting && <span role="status">VNC 연결 대기 중</span>}
    {notice && <span role="status">{notice}</span>}
    {session && <>
      <form className="remote-desktop-toolbar" onSubmit={resize}>
        <label>너비 <input name="width" type="number" min={1} max={8192} required /></label>
        <label>높이 <input name="height" type="number" min={1} max={8192} required /></label>
        <button type="submit">서버 해상도 변경</button>
      </form>
      <RemoteDesktopView session={session} active={active} label="VNC 원격 화면" />
    </>}
  </section>;
}
