import { RotateCcw } from "lucide-react";
import { useEffect, useImperativeHandle, useLayoutEffect, useRef, useState, type Ref, type SubmitEvent } from "react";
import { RemoteDesktopView } from "../remoteDesktop/RemoteDesktopView";
import type { DesktopPreset } from "../remoteDesktop/desktopProfile";
import { createFitScheduler, fitSize, type DesktopFit } from "../remoteDesktop/fit";
import type { RemoteDesktopSize } from "../remoteDesktop/session";
import type { DesktopTunnelSource } from "../tunnels/tunnelIpc";
import { IconButton } from "../ui/controls";
import { BUSY_NOTICE, connect, type Capabilities, type Connection } from "./session";

const noChannels: Capabilities = { copy: false, paste: false, resize: false };
/** 창 크기 변경이 이 시간 동안 멈추면 원격 해상도를 맞춘다. */
export const FIT_DELAY_MS = 400;
/** 첫 화면 영역 크기를 이 시간 안에 받지 못하면 기본 크기로 연결한다. */
const VIEWPORT_WAIT_MS = 1000;
/** 직접 요청한 해상도가 이 시간 뒤에도 그대로면 알린다. */
export const APPLY_WAIT_MS = 5000;
export const NOT_APPLIED_NOTICE = "서버가 해상도를 바꾸지 않았습니다";
/** 같은 크기의 창 맞춤 요청이 이 횟수를 넘게 `retry`로 돌아오면 알린다. */
export const RETRY_NOTICE_AFTER = 5;
export const RESIZE_WAIT_NOTICE = "해상도 변경 대기";
const scaleOf = () => Math.round((window.devicePixelRatio || 1) * 100);
/** 서버가 정한 크기를 다시 연결 크기로 쓸 때 Display Control 범위(짝수 폭, 200 이상)로 맞춘다. */
const evenSize = ({ width, height }: RemoteDesktopSize): DesktopFit =>
  ({ width: Math.max(200, width - (width % 2)), height: Math.max(200, height), scale: scaleOf() });

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
  const [fit, setFit] = useState(true);
  const [remoteSize, setRemoteSize] = useState<RemoteDesktopSize>();
  /** 크기 변경 콜백과 타이머가 읽는 최신 값이다. */
  const latest = useRef({ connection, ready, resize: channels.resize, fit, active });
  /** `scale`은 서버에 마지막으로 알린 배율이다. `requested`는 크기와 배율을 합친 마지막 요청이다. */
  const display = useRef<{
    view?: RemoteDesktopSize; remote?: RemoteDesktopSize; scale?: number; requested?: string;
    /** 같은 크기 요청이 `retry`로 이어진 횟수다. 알림을 낸 뒤에는 요청 크기가 바뀔 때까지 다시 내지 않는다. */
    retries?: { key: string; count: number };
    initial?: (size: DesktopFit | undefined) => void; wait?: ReturnType<typeof setTimeout>;
  }>({});
  const fitter = useRef<ReturnType<typeof createFitScheduler>>(undefined);
  fitter.current ??= createFitScheduler(FIT_DELAY_MS, () => requestFit());
  const applyCheck = useRef<ReturnType<typeof setTimeout>>(undefined);
  const forwardedAddress = forwarded(source);
  const loginMode = opening?.tlsOnly ? "NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)" : "NLA";

  useLayoutEffect(() => () => {
    stopTimers();
    void cancel.current?.().catch(() => { });
  }, []);
  useLayoutEffect(() => { latest.current = { connection, ready, resize: channels.resize, fit, active }; });
  useEffect(() => { if (fit && ready) requestFit(); }, [fit, ready, remoteSize, channels.resize, active]);
  useImperativeHandle(ref, () => ({ requestClose }));

  /**
   * 창에 맞춤에서 화면 영역의 장치 픽셀 크기와 배율을 서버에 요청한다. 서버가 다른 크기를 적용해도 같은 요청을 되풀이하지 않는다.
   * 화면 전환이나 작업 중이라 보내지 못한 요청은 잠시 뒤 다시 요청한다.
   */
  function requestFit() {
    const { connection, ready, resize, fit, active } = latest.current;
    const { view, remote, scale, requested } = display.current;
    const target = view && fitSize(view, window.devicePixelRatio);
    if (!fit || !target || !connection || !ready || !resize || !active) return;
    const key = `${target.width}x${target.height}@${target.scale}`;
    if ((remote?.width === target.width && remote.height === target.height && scale === target.scale) || requested === key) return;
    display.current.requested = key;
    void connection.resize(target).then((result) => {
      if (result === "sent") {
        display.current.scale = target.scale;
        display.current.retries = undefined;
        setNotice((current) => current === RESIZE_WAIT_NOTICE ? "" : current);
      }
      if (result !== "retry" || display.current.requested !== key) return;
      display.current.requested = undefined;
      const retries = display.current.retries?.key === key ? display.current.retries : { key, count: 0 };
      display.current.retries = { key, count: retries.count + 1 };
      if (retries.count === RETRY_NOTICE_AFTER) setNotice((current) => current || RESIZE_WAIT_NOTICE);
      fitter.current?.schedule();
    });
  }

  /** 첫 크기 대기, 창 맞춤 지연, 적용 확인 타이머를 멈춘다. 기다리던 첫 크기는 기본 크기로 끝낸다. */
  function stopTimers() {
    fitter.current?.cancel();
    clearTimeout(applyCheck.current);
    clearTimeout(display.current.wait);
    const initial = display.current.initial;
    display.current.initial = undefined;
    initial?.(undefined);
  }

  /** 첫 측정은 연결 크기로 쓴다. 영역이 0이면 기본 크기로 연결하고, 숨겨진 동안의 0은 무시한다. */
  function viewport(size: RemoteDesktopSize) {
    const initial = display.current.initial;
    display.current.initial = undefined;
    if (size.width > 0 && size.height > 0) display.current.view = size;
    if (initial) {
      clearTimeout(display.current.wait);
      initial(fitSize(size, window.devicePixelRatio));
    } else if (latest.current.fit && display.current.view === size) fitter.current?.schedule();
  }

  function applyFixed(width: number, height: number) {
    const connection = latest.current.connection;
    if (!connection) return;
    const before = display.current.remote;
    const scale = scaleOf();
    setNotice("");
    void connection.resize({ width, height, scale }).then((result) => {
      if (result === "sent") display.current.scale = scale;
      else if (result === "retry") setNotice(BUSY_NOTICE);
    });
    clearTimeout(applyCheck.current);
    applyCheck.current = setTimeout(() => {
      const now = display.current.remote;
      if (now && before && now.width === before.width && now.height === before.height && (now.width !== width || now.height !== height)) {
        setNotice((current) => current || NOT_APPLIED_NOTICE);
      }
    }, APPLY_WAIT_MS);
  }

  function reset() {
    stopTimers();
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
    // 창에 맞춤이면 화면 영역을 잰 크기로, 고정이면 직전 원격 크기로 연결한다.
    const initial = Promise.withResolvers<DesktopFit | undefined>();
    const fixed = display.current.remote;
    display.current.requested = undefined;
    display.current.retries = undefined;
    void initial.promise.then((size) => { display.current.scale = size?.scale; });
    if (!fit && fixed) initial.resolve(evenSize(fixed));
    else {
      display.current.initial = initial.resolve;
      display.current.wait = setTimeout(() => {
        if (display.current.initial !== initial.resolve) return;
        display.current.initial = undefined;
        initial.resolve(undefined);
      }, VIEWPORT_WAIT_MS);
    }
    cancel.current = connect(draft, setConnection,
      setChannels, (value, size) => {
        setReady(value);
        if (!size) return;
        display.current.remote = size;
        setRemoteSize(size);
      }, setNotice, (message, closed, kind) => {
        reset();
        if (closed) cancel.current = undefined;
        setCloseStatus(closed ? undefined : "unconfirmed");
        setError(message);
        setRouteLost(!!draft.source && kind === "transport");
      }, undefined, initial.promise);
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
  return <section className="remote-desktop-tab" aria-label={label}>
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
        <form key={remoteSize ? `${remoteSize.width}x${remoteSize.height}` : ""} className="remote-desktop-toolbar" onSubmit={(event) => {
          event.preventDefault();
          if (!event.nativeEvent.isTrusted || fit || !active || !ready || !channels.resize) return;
          const size = new FormData(event.currentTarget);
          applyFixed(Number(size.get("width")), Number(size.get("height")));
        }}>
          <label><input type="checkbox" checked={fit} onChange={(event) => {
            display.current.requested = undefined;
            setFit(event.target.checked);
          }} /> 창에 맞춤</label>
          <label>너비 <input name="width" type="number" defaultValue={remoteSize?.width ?? 1024} min={200} max={4096} step={2} required disabled={fit} /></label>
          <label>높이 <input name="height" type="number" defaultValue={remoteSize?.height ?? 768} min={200} max={4096} required disabled={fit} /></label>
          <button type="submit" disabled={fit || !active || !ready || !channels.resize}>해상도 변경</button>
        </form>
      </>}
    </div>}
    {closeStatus === "closing" && <div role="status">종료 중</div>}
    {closeStatus === "unconfirmed" && <button type="button" onClick={() => { void requestClose(); }}>종료 다시 확인</button>}
    {(notice || error) && <div className="remote-desktop-notices">
      {notice && <div role="status">{notice}</div>}
      {error && <div role="alert">{error}</div>}
    </div>}
    {connection && <RemoteDesktopView key={connection.sessionId} session={connection} active={active} label={`${label} 화면`} onViewport={viewport} />}
  </section>;
}
