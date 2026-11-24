import { useLayoutEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { createRemoteDesktopViewSession, type RemoteDesktopSession, type RemoteDesktopSize, type RemoteDesktopStatus, type RemoteDesktopViewSession } from "./session";
import { imeKeyDown, modifierEvents, releasedModifiers, textCharacters, type ModifierState } from "./input";
import "./remoteDesktop.css";

interface RemoteDesktopViewProps {
  readonly session: RemoteDesktopSession;
  readonly active: boolean;
  readonly label: string;
  /** 원격 화면을 그릴 영역의 CSS 픽셀 크기를 처음 한 번과 바뀔 때마다 알린다. 숨겨지면 0이다. */
  readonly onViewport?: (size: RemoteDesktopSize) => void;
}

function statusText(status: RemoteDesktopStatus) {
  switch (status.kind) {
    case "waiting": return "첫 화면 대기 중";
    case "connected": return `연결됨 · ${status.size.width} × ${status.size.height}`;
    case "disconnected": return `연결 끊김: ${status.message}`;
    case "failed": return `연결 실패: ${status.message}`;
  }
}

export function RemoteDesktopView({ session, active, label, onViewport }: RemoteDesktopViewProps) {
  const canvas = useRef<HTMLCanvasElement>(null);
  /** IME 조합을 받는 숨은 입력칸이다. 키보드 포커스는 canvas가 아니라 여기에 둔다. */
  const keyboard = useRef<HTMLTextAreaElement>(null);
  const screen = useRef<HTMLDivElement>(null);
  const viewport = useRef(onViewport);
  const bound = useRef<RemoteDesktopViewSession>(null);
  const [status, setStatus] = useState<RemoteDesktopStatus>({ kind: "waiting" });
  const keys = useRef(new Map<string, string>());
  const modifiers = useRef<ModifierState>({ shift: false, ctrl: false, alt: false, meta: false });
  const pointer = useRef<number | null>(null);
  const [composing, setComposing] = useState(false);
  /** 조합 진행 여부, 조합을 확정하려고 입력칸을 blur하는 중인지, 확정 뒤 포커스를 돌려주기를 기다리는 중인지다. */
  const ime = useRef({ composing: false, committing: false, refocusing: false });
  const refocus = useRef<ReturnType<typeof setTimeout>>(undefined);
  const live = useRef(false);
  const [keyboardFocused, setKeyboardFocused] = useState(false);
  const typed = useRef<(text: string) => void>(() => { });
  const entered = useRef<() => void>(() => { });

  function release() {
    const id = pointer.current;
    pointer.current = null;
    const connection = bound.current;
    const released = releasedModifiers(modifiers.current);
    modifiers.current = released.state;
    for (const event of released.events) connection?.send({ kind: "key", ...event, repeat: false });
    const held = keys.current.size > 0 || id !== null;
    keys.current.clear();
    const element = canvas.current;
    if (id !== null && element?.hasPointerCapture(id)) element.releasePointerCapture(id);
    if (held) connection?.send({ kind: "release-input" });
  }

  /** 조합 중이면 입력칸을 blur해 조합을 확정한다. 확정 글자는 compositionend에서 한 번 보내고, 눌린 키 해제는 하지 않는다. */
  function commit() {
    const input = keyboard.current;
    if (!input || !ime.current.composing || input !== input.ownerDocument.activeElement) return false;
    ime.current.committing = true;
    try { input.blur(); } finally { ime.current.committing = false; }
    return true;
  }

  function cancelRefocus() {
    clearTimeout(refocus.current);
    refocus.current = undefined;
    ime.current.refocusing = false;
  }

  useLayoutEffect(() => {
    const element = canvas.current;
    if (!element) return;
    const connection = createRemoteDesktopViewSession(session, element, setStatus);
    bound.current = connection;
    const window = element.ownerDocument.defaultView;
    // 창이 포커스를 잃을 때도 조합 중인 글자를 한 번 확정해 보낸다.
    const blur = () => { cancelRefocus(); commit(); release(); keyboard.current?.blur(); };
    window?.addEventListener("blur", blur);
    return () => {
      window?.removeEventListener("blur", blur);
      cancelRefocus();
      bound.current = null;
      session.setActive(false);
      release();
      keyboard.current?.blur();
      connection.dispose();
    };
  }, [session]);

  // React의 onBeforeInput은 inputType이 없는 합성 이벤트라 네이티브 이벤트를 직접 받는다.
  useLayoutEffect(() => {
    const element = keyboard.current;
    if (!element) return;
    /** 마지막 신뢰 compositionupdate의 글자다. 확정할 때도 확정 글자로 한 번 더 온다. */
    let composed = "";
    let timer: ReturnType<typeof setTimeout> | undefined;
    const focused = () => element === element.ownerDocument.activeElement;
    // 조합이 끝난 글자는 입력칸에 남는다. 다음 조합이 바로 이어지지 않으면 비운다.
    const clear = () => {
      clearTimeout(timer);
      timer = setTimeout(() => { if (!ime.current.composing) element.value = ""; });
    };
    const MAX_KEPT = 256;
    const start = (event: CompositionEvent) => {
      if (!event.isTrusted) return;
      ime.current.composing = true;
      composed = "";
      setComposing(true);
    };
    const update = (event: CompositionEvent) => {
      if (event.isTrusted && ime.current.composing) composed = event.data;
    };
    const end = (event: CompositionEvent) => {
      if (!ime.current.composing) return;
      ime.current.composing = false;
      setComposing(false);
      // Chromium(WebView2)은 compositionend만 isTrusted=false로 보낸다. 그때는 직전 신뢰 compositionupdate와
      // 같은 글자일 때만 보낸다. 취소한 조합(빈 글자)과 앞당겨 만든 확정은 보내지 않는다.
      const text = event.isTrusted ? event.data : event.data === composed ? composed : "";
      if ((focused() || ime.current.committing) && text) typed.current(text);
      if (element.value.length > MAX_KEPT) element.value = "";
      clear();
    };
    const before = (event: InputEvent) => {
      // 조합 중 입력은 compositionend에서 한 번에 보낸다.
      if (ime.current.composing || event.isComposing) return;
      // 스캔코드로 보낸 키는 keydown에서 기본 동작을 막으므로 여기에 오지 않는다. 여기 오는 입력은 IME나 자동화가 넣은 글자다.
      event.preventDefault();
      if (!event.isTrusted || !focused()) return;
      if (event.inputType === "insertText" && event.data) typed.current(event.data);
      else if (event.inputType === "insertLineBreak" || event.inputType === "insertParagraph") entered.current();
    };
    element.addEventListener("compositionstart", start);
    element.addEventListener("compositionupdate", update);
    element.addEventListener("compositionend", end);
    element.addEventListener("beforeinput", before);
    return () => {
      clearTimeout(timer);
      element.removeEventListener("compositionstart", start);
      element.removeEventListener("compositionupdate", update);
      element.removeEventListener("compositionend", end);
      element.removeEventListener("beforeinput", before);
    };
  }, []);

  useLayoutEffect(() => { viewport.current = onViewport; });
  useLayoutEffect(() => {
    const element = screen.current;
    if (!element) return;
    const report = () => {
      const { width, height } = element.getBoundingClientRect();
      viewport.current?.({ width, height });
    };
    report();
    const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(report);
    observer?.observe(element);
    // 창을 배율이 다른 모니터로 옮기면 CSS 크기는 그대로여도 장치 픽셀 크기가 바뀐다.
    const view = element.ownerDocument.defaultView;
    let media: MediaQueryList | undefined;
    const rescaled = () => {
      watch();
      report();
    };
    const watch = () => {
      media?.removeEventListener("change", rescaled);
      media = view?.matchMedia?.(`(resolution: ${view.devicePixelRatio}dppx)`);
      media?.addEventListener("change", rescaled);
    };
    watch();
    return () => {
      observer?.disconnect();
      media?.removeEventListener("change", rescaled);
    };
  }, []);

  const connected = status.kind === "connected";
  useLayoutEffect(() => {
    session.setActive(active);
    live.current = active;
    if (!active || !connected) {
      cancelRefocus();
      release();
      keyboard.current?.blur();
    }
  }, [session, active, connected]);

  useLayoutEffect(() => {
    typed.current = (text) => {
      const connection = bound.current;
      if (!active || !connection?.size) return;
      for (const character of textCharacters(text)) connection.send({ kind: "text", text: character });
    };
    // IME가 조합을 끝내며 넘긴 Enter다. keydown은 IME 키로 걸러졌으므로 여기서 한 번만 보낸다.
    entered.current = () => {
      const connection = bound.current;
      if (!active || !connection?.size) return;
      for (const down of [true, false]) connection.send({ kind: "key", down, code: "Enter", key: "Enter", location: 0, repeat: false });
    };
  });

  function sendKey(event: KeyboardEvent<HTMLTextAreaElement>, down: boolean) {
    const connection = bound.current;
    if (!active || !connection?.size || !event.nativeEvent.isTrusted ||
      event.currentTarget !== event.currentTarget.ownerDocument.activeElement) return;
    // IME 키는 기본 동작을 막지 않아야 조합이 진행된다. 결과 글자는 compositionend에서 보낸다.
    if (down && imeKeyDown({ isComposing: event.nativeEvent.isComposing, key: event.key, keyCode: event.keyCode })) return;
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
    // 자동화·IME는 Shift keydown 없이 shiftKey=true만 보낼 수 있다. 서버의 수식 키 상태를
    // 먼저 맞춰야 문자가 올바른 대소문자로 도착한다.
    const synchronized = modifierEvents(modifiers.current, {
      code: event.code, down,
      shiftKey: event.shiftKey, ctrlKey: event.ctrlKey, altKey: event.altKey, metaKey: event.metaKey,
    });
    modifiers.current = synchronized.state;
    for (const modifier of synchronized.events) connection.send({ kind: "key", ...modifier, repeat: false });
    connection.send({ kind: "key", down, code: event.code, key: pressed ?? event.key, location: event.location, repeat: event.repeat });
  }

  function sendPointer(event: PointerEvent<HTMLCanvasElement>) {
    const connection = bound.current;
    if (!active || !event.nativeEvent.isTrusted || !event.isPrimary ||
      (pointer.current !== null && pointer.current !== event.pointerId)) return;
    const element = event.currentTarget;
    if (event.type === "pointerdown") {
      if (commit()) {
        // 조합 중 클릭은 조합을 먼저 확정해 확정 글자가 클릭보다 먼저 원격에 간다. 포커스를 바로 돌리면
        // TSF IME가 조합 끝을 다 처리하기 전에 입력칸이 다시 조합을 받아 글자가 두 번 들어갈 수 있어 다음 작업에서 돌린다.
        cancelRefocus();
        ime.current.refocusing = true;
        refocus.current = setTimeout(() => {
          refocus.current = undefined;
          ime.current.refocusing = false;
          if (live.current) keyboard.current?.focus({ preventScroll: true });
        });
      } else if (!ime.current.refocusing) {
        // 연결 중이거나 첫 화면 전에도 포커스를 입력칸으로 옮긴다. 키 전송은 sendKey가 막는다.
        keyboard.current?.focus({ preventScroll: true });
      }
    }
    const size = connection?.size;
    if (!size) return;
    const bounds = element.getBoundingClientRect();
    if (!(bounds.width > 0 && bounds.height > 0)) return;
    if (event.type === "pointerdown") {
      element.setPointerCapture(event.pointerId);
      pointer.current = event.pointerId;
    }
    if ((element.ownerDocument.activeElement !== keyboard.current && !ime.current.refocusing) ||
      connection !== bound.current || !connection.size) return;
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
      <div className="remote-desktop-screen" ref={screen}>
        <textarea
          ref={keyboard}
          className="remote-desktop-keyboard"
          aria-label="원격 키보드 입력"
          data-composing={composing || undefined}
          wrap="off"
          tabIndex={active && connected ? 0 : -1}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          spellCheck={false}
          onFocus={() => setKeyboardFocused(true)}
          onBlur={() => {
            setKeyboardFocused(false);
            if (!ime.current.committing) release();
          }}
          onKeyDown={(event) => sendKey(event, true)}
          onKeyUp={(event) => sendKey(event, false)}
        />
        <canvas
          ref={canvas}
          aria-label="원격 화면"
          data-keyboard={connected && keyboardFocused ? "focused" : undefined}
          // 누를 때 포커스가 입력칸에서 canvas나 body로 옮겨 가지 않게 한다.
          onMouseDown={(event) => event.preventDefault()}
          onPointerDown={sendPointer}
          onPointerMove={sendPointer}
          onPointerUp={sendPointer}
          onPointerCancel={(event) => { if (pointer.current === event.pointerId) release(); }}
          onLostPointerCapture={(event) => { if (pointer.current === event.pointerId) release(); }}
          onContextMenu={(event) => { if (active && connected) event.preventDefault(); }}
        />
      </div>
    </section>
  );
}
