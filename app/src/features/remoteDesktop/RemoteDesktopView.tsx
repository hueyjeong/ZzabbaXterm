import { useLayoutEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { createRemoteDesktopViewSession, type RemoteDesktopSession, type RemoteDesktopStatus, type RemoteDesktopViewSession } from "./session";
import "./remoteDesktop.css";

interface RemoteDesktopViewProps {
  readonly session: RemoteDesktopSession;
  readonly active: boolean;
  readonly label: string;
}

function statusText(status: RemoteDesktopStatus) {
  switch (status.kind) {
    case "waiting": return "첫 화면 대기 중";
    case "connected": return `연결됨 · ${status.size.width} × ${status.size.height}`;
    case "disconnected": return `연결 끊김: ${status.message}`;
    case "failed": return `연결 실패: ${status.message}`;
  }
}

export function RemoteDesktopView({ session, active, label }: RemoteDesktopViewProps) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const bound = useRef<RemoteDesktopViewSession>(null);
  const [status, setStatus] = useState<RemoteDesktopStatus>({ kind: "waiting" });
  const keys = useRef(new Map<string, string>());
  const pointer = useRef<number | null>(null);

  function release() {
    const id = pointer.current;
    pointer.current = null;
    const held = keys.current.size > 0 || id !== null;
    keys.current.clear();
    const element = canvas.current;
    if (id !== null && element?.hasPointerCapture(id)) element.releasePointerCapture(id);
    if (held) bound.current?.send({ kind: "release-input" });
  }

  useLayoutEffect(() => {
    const element = canvas.current;
    if (!element) return;
    const connection = createRemoteDesktopViewSession(session, element, setStatus);
    bound.current = connection;
    const window = element.ownerDocument.defaultView;
    const blur = () => { release(); element.blur(); };
    window?.addEventListener("blur", blur);
    return () => {
      window?.removeEventListener("blur", blur);
      bound.current = null;
      session.setActive(false);
      release();
      element.blur();
      connection.dispose();
    };
  }, [session]);

  const connected = status.kind === "connected";
  useLayoutEffect(() => {
    session.setActive(active);
    if (!active || !connected) {
      release();
      canvas.current?.blur();
    }
  }, [session, active, connected]);

  function sendKey(event: KeyboardEvent<HTMLCanvasElement>, down: boolean) {
    const connection = bound.current;
    if (!active || !connection?.size || !event.nativeEvent.isTrusted ||
      event.currentTarget !== event.currentTarget.ownerDocument.activeElement) return;
    const id = `${event.code || event.key}:${event.location}`;
    const pressed = keys.current.get(id);
    // Ctrl+Tab은 로컬로 보내되 앞서 전달한 Tab 누름은 해제한다.
    if (event.key === "Tab" && event.ctrlKey && pressed === undefined) return;
    event.preventDefault();
    event.stopPropagation();
    if (down) {
      if (event.repeat && pressed === undefined) return;
      keys.current.set(id, pressed ?? event.key);
    } else if (pressed === undefined) {
      return;
    } else {
      keys.current.delete(id);
    }
    connection.send({ kind: "key", down, code: event.code, key: pressed ?? event.key, location: event.location, repeat: event.repeat });
  }

  function sendPointer(event: PointerEvent<HTMLCanvasElement>) {
    const connection = bound.current;
    const size = connection?.size;
    if (!active || !size || !event.nativeEvent.isTrusted || !event.isPrimary ||
      (pointer.current !== null && pointer.current !== event.pointerId)) return;
    const element = event.currentTarget;
    const bounds = element.getBoundingClientRect();
    if (!(bounds.width > 0 && bounds.height > 0)) return;
    if (event.type === "pointerdown") {
      element.focus({ preventScroll: true });
      element.setPointerCapture(event.pointerId);
      pointer.current = event.pointerId;
    }
    if (element.ownerDocument.activeElement !== element || connection !== bound.current || !connection.size) return;
    if (pointer.current === null && (event.buttons !== 0 || event.type === "pointerup")) return;
    event.preventDefault();
    event.stopPropagation();
    const x = Math.max(0, Math.min(size.width - 1, Math.floor((event.clientX - bounds.left) * size.width / bounds.width)));
    const y = Math.max(0, Math.min(size.height - 1, Math.floor((event.clientY - bounds.top) * size.height / bounds.height)));
    if (event.buttons === 0) pointer.current = null;
    connection.send({ kind: "pointer", x, y, buttons: event.buttons });
  }

  return (
    <section className="remote-desktop-view" aria-label={label} hidden={!active} inert={!active}>
      <div className="remote-desktop-toolbar">
        <span role="status">{statusText(status)}</span>
        {session.requestClipboard && (["copy", "paste"] as const).map((action) => (
          <button key={action} type="button" disabled={!connected || !active} onClick={(event) => {
            if (active && event.nativeEvent.isTrusted) bound.current?.clipboard(action);
          }}>{action === "copy" ? "복사" : "붙여넣기"}</button>
        ))}
      </div>
      <canvas
        ref={canvas}
        aria-label="원격 화면"
        tabIndex={active && connected ? 0 : -1}
        onBlur={release}
        onKeyDown={(event) => sendKey(event, true)}
        onKeyUp={(event) => sendKey(event, false)}
        onPointerDown={sendPointer}
        onPointerMove={sendPointer}
        onPointerUp={sendPointer}
        onPointerCancel={(event) => { if (pointer.current === event.pointerId) release(); }}
        onLostPointerCapture={(event) => { if (pointer.current === event.pointerId) release(); }}
        onContextMenu={(event) => { if (active && connected) event.preventDefault(); }}
      />
    </section>
  );
}
