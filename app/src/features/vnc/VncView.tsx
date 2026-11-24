import { useEffect, useImperativeHandle, useLayoutEffect, useRef, useState, type Ref, type SubmitEvent } from "react";
import { RemoteDesktopView } from "../remoteDesktop/RemoteDesktopView";
import type { DesktopPreset } from "../remoteDesktop/desktopProfile";
import type { DesktopTunnelSource } from "../tunnels/tunnelIpc";
import { CLOSE_UNCONFIRMED, closeOwner, connect, createOwner, type Connection, type Owner } from "./session";

export interface VncTabActions { requestClose(): Promise<boolean> }

type Draft = Parameters<typeof connect>[1];

export function VncView({ active, source, preset, autoStart, ref }: {
  readonly active: boolean;
  readonly source?: DesktopTunnelSource;
  readonly preset?: DesktopPreset;
  /** 저장 프로필로 연 탭은 기본 인증(VNC 비밀번호)으로 한 번 바로 연결한다. */
  readonly autoStart?: boolean;
  readonly ref?: Ref<VncTabActions>;
}) {
  const autoStarted = useRef(false);
  const owner = useRef<Promise<Owner> | null>(null);
  const cancel = useRef<(() => void) | null>(null);
  const busy = useRef(false);
  const mounted = useRef(false);
  const closing = useRef<Promise<boolean> | null>(null);
  const unconfirmed = useRef(false);
  const sealed = useRef(false);
  const [session, setSession] = useState<Connection>();
  const [connecting, setConnecting] = useState(false);
  const [notice, setNotice] = useState("");
  const [cleanupBlocked, setCleanupBlocked] = useState(false);

  function markUnconfirmed() {
    unconfirmed.current = true;
    setCleanupBlocked(true);
  }

  function closeCurrent(retry: boolean): Promise<boolean> {
    if (closing.current) return closing.current;
    const current = owner.current;
    cancel.current?.();
    cancel.current = null;
    busy.current = false;
    setConnecting(false);
    setSession(undefined);
    if (!current) return Promise.resolve(true);
    const pending = current.then((id) => closeOwner(id, retry), () => true).then((confirmed) => {
      if (mounted.current && owner.current === current) {
        if (confirmed) {
          owner.current = null;
          setNotice("");
          unconfirmed.current = false;
          setCleanupBlocked(false);
        } else {
          sealed.current = false;
          markUnconfirmed();
        }
      }
      return confirmed;
    }).finally(() => { if (closing.current === pending) closing.current = null; });
    closing.current = pending;
    return pending;
  }

  useImperativeHandle(ref, () => ({
    requestClose() {
      sealed.current = true;
      return closeCurrent(unconfirmed.current);
    },
  }));

  useLayoutEffect(() => {
    if (!active && busy.current) void closeCurrent(false);
  }, [active]);

  useLayoutEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      cancel.current?.();
      cancel.current = null;
      const current = owner.current;
      owner.current = null;
      if (current && !closing.current && !unconfirmed.current) void current.then((id) => closeOwner(id)).catch(() => { });
    };
  }, []);

  function open(event: SubmitEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!event.nativeEvent.isTrusted) return;
    const fields = new FormData(event.currentTarget);
    const password = fields.get("auth") === "password";
    void start(source ? { source, password } : {
      host: String(fields.get("host") ?? "").trim(), port: Number(fields.get("port")), password,
    });
  }

  useEffect(() => {
    if (!autoStart || !active || autoStarted.current || !(source || preset)) return;
    let stopped = false;
    // StrictMode에서 즉시 해제된 첫 마운트는 연결을 시작하지 않는다.
    void Promise.resolve().then(() => {
      if (stopped || autoStarted.current) return;
      autoStarted.current = true;
      void start(source ? { source, password: true } : { host: preset!.host, port: preset!.port, password: true });
    });
    return () => { stopped = true; };
  }, [autoStart, active, source, preset]);

  /** 승인·자격 증명 확인은 Rust 네이티브 창이 맡는다. 여기서는 중복·종료 중 연결만 막는다. */
  async function start(draft: Draft) {
    if (!active || busy.current || closing.current || unconfirmed.current || sealed.current) return;
    busy.current = true;
    cancel.current?.();
    cancel.current = null;
    setSession(undefined);
    setNotice("");
    setConnecting(true);
    const current = owner.current ??= createOwner();
    const settle = () => { busy.current = false; setConnecting(false); };
    try {
      const id = await current;
      if (owner.current !== current || closing.current || sealed.current) return;
      const dispose = connect(id, draft, (connection) => { setSession(connection); settle(); }, setNotice,
        (message) => { setNotice(message); settle(); }, undefined, {
          closeOpening() {
            if (mounted.current && cancel.current === dispose && !closing.current) void closeCurrent(false);
          },
          unconfirmed() {
            if (mounted.current && cancel.current === dispose && !closing.current) markUnconfirmed();
          },
        });
      cancel.current = dispose;
    } catch (error) {
      if (mounted.current && owner.current === current && !closing.current) {
        owner.current = null;
        setNotice(String(error));
        settle();
      }
    }
  }

  function resize(event: SubmitEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!active || !event.nativeEvent.isTrusted || !session || unconfirmed.current || closing.current || sealed.current) return;
    const fields = new FormData(event.currentTarget);
    session.resize(Number(fields.get("width")), Number(fields.get("height")));
  }

  return <section className="remote-desktop-view" aria-label="VNC" hidden={!active} inert={!active}>
    <form className="remote-desktop-toolbar" onSubmit={open}>
      {source ? <span>SSH 터널 · [{source.destination.host}]:{source.destination.port}</span> : <>
        <label>호스트 <input name="host" maxLength={253} required autoComplete="off" defaultValue={preset?.host} disabled={connecting || cleanupBlocked} /></label>
        <label>포트 <input name="port" type="number" min={1} max={65535} defaultValue={preset?.port ?? 5900} required disabled={connecting || cleanupBlocked} /></label>
      </>}
      <label>서비스 인증 <select name="auth" defaultValue="password" disabled={connecting || cleanupBlocked}>
        <option value="password">VNC 비밀번호</option>
        <option value="none">없음</option>
      </select></label>
      <button type="submit" disabled={connecting || cleanupBlocked}>{source ? "SSH 터널로 연결" : session ? "다시 연결" : "직접 연결"}</button>
    </form>
    {connecting && <span role="status">VNC 연결 대기 중</span>}
    {cleanupBlocked ? <span role="alert">{CLOSE_UNCONFIRMED}</span> : notice && <span role="status">{notice}</span>}
    {session && !cleanupBlocked && <>
      <form className="remote-desktop-toolbar" onSubmit={resize}>
        <label>너비 <input name="width" type="number" min={1} max={8192} required /></label>
        <label>높이 <input name="height" type="number" min={1} max={8192} required /></label>
        <button type="submit">서버 해상도 변경</button>
      </form>
      <RemoteDesktopView session={session} active={active} label="VNC 원격 화면" />
    </>}
  </section>;
}
