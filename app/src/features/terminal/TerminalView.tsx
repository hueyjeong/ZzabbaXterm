import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { RotateCcw } from "lucide-react";
import { rightClick, type TerminalClipboard } from "./rightClick";
import { useImperativeHandle, useLayoutEffect, useRef, useState, type Ref } from "react";
import type { ElevationRequest } from "../elevation/elevation";
import { SsoLoginAction, ssoExpired, useSsoLogin, type SsoLoginIpc } from "../ssm/ssoLogin";
import { IconButton } from "../ui/controls";
import { connectLocalTerminal, type LocalTerminal, type LocalTerminalIpc, type TerminalStatus } from "./session";
import "./terminal.css";

export interface TerminalViewProps {
  readonly active: boolean;
  readonly focusRequested: boolean;
  /** 같은 탭의 포커스를 다시 요청할 때마다 바뀐다. */
  readonly focusSeq?: number;
  readonly clipboard?: TerminalClipboard;
  readonly label: string;
  readonly targetId?: string;
  readonly ipc?: LocalTerminalIpc;
  readonly remote?: boolean;
  readonly ssh?: boolean;
  readonly remoteEditor?: true;
  readonly elevation?: ElevationRequest;
  readonly closing?: boolean;
  readonly closeRef?: Ref<{ confirmClose(): Promise<void> }>;
  readonly onStatus?: (status: TerminalStatus, sessionId?: string) => void;
  /** 열기에 실패하면 다시 연결 단추를 보인다. */
  readonly reconnect?: boolean;
  readonly ssoIpc?: SsoLoginIpc;
}

function text(status: TerminalStatus) {
  switch (status.kind) {
    case "opening": return "여는 중";
    case "connecting": return "연결 중";
    case "running": return status.notice ?? null;
    case "exited": return `종료 코드 ${status.value}`;
    case "disconnected": return `단절: ${status.value}`;
    case "failed": return `열기 실패: ${status.value}`;
  }
}

export function TerminalView({ active, focusRequested, focusSeq, clipboard, label, targetId, ipc, remote, ssh, remoteEditor, elevation, closing, closeRef, onStatus, reconnect, ssoIpc }: TerminalViewProps) {
  const host = useRef<HTMLDivElement>(null);
  const session = useRef<LocalTerminal | undefined>(undefined);
  const clipboardRef = useRef(clipboard);
  clipboardRef.current = clipboard;
  const stopRequested = useRef(false);
  useImperativeHandle(closeRef, () => ({
    async confirmClose() {
      stopRequested.current = true;
      await session.current?.confirmClose();
    },
  }), []);
  const refresh = useRef<((visible: boolean, ready: boolean, requested: boolean) => void) | null>(null);
  const [status, setStatus] = useState<TerminalStatus>({ kind: "opening" });
  const statusListener = useRef(onStatus);
  useLayoutEffect(() => { statusListener.current = onStatus; }, [onStatus]);
  // 세대를 올리면 기존 세션을 닫고 같은 대상으로 새 세션을 연다.
  const [generation, setGeneration] = useState(0);
  function reopen() {
    const opening: TerminalStatus = { kind: "opening" };
    setStatus(opening);
    statusListener.current?.(opening);
    setGeneration(value => value + 1);
  }
  const closingNow = useRef(closing);
  useLayoutEffect(() => { closingNow.current = closing; }, [closing]);
  // 닫히는 탭은 로그인에 성공해도 다시 열지 않는다.
  const ssoLogin = useSsoLogin(targetId, () => { if (!closingNow.current) reopen(); }, ssoIpc);
  const cancelSsoLogin = ssoLogin.cancel;
  useLayoutEffect(() => { if (closing) cancelSsoLogin(); }, [closing, cancelSsoLogin]);

  useLayoutEffect(() => {
    const element = host.current;
    if (!element) return;
    const term = new Terminal({
      allowProposedApi: false,
      fontFamily: '"Cascadia Mono", "D2Coding", "DejaVu Sans Mono", "Malgun Gothic", monospace',
      fontSize: 14,
      lineHeight: 1.15,
      // ui/tokens.css의 --bg-app·--text·--accent와 같은 색이다.
      theme: {
        background: "#0f1417", foreground: "#dbe4e0", cursor: "#5fd1a8", cursorAccent: "#0f1417",
        selectionBackground: "#2f4a44",
        black: "#1b2226", red: "#f07a6e", green: "#7fcf8f", yellow: "#e8b85c",
        blue: "#6cb6ff", magenta: "#c792ea", cyan: "#5fd1c8", white: "#c9d3cf",
        brightBlack: "#5d6b67", brightRed: "#ff9b90", brightGreen: "#a3e6b0", brightYellow: "#f5d08a",
        brightBlue: "#94caff", brightMagenta: "#dcb1f5", brightCyan: "#8ce6df", brightWhite: "#f2f6f4",
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(element);
    let acceptsUserInput = false;
    term.attachCustomKeyEventHandler((event) => {
      if (event.type !== "keydown" || event.key !== "Enter" || !event.shiftKey ||
        event.ctrlKey || event.altKey || event.metaKey || event.keyCode === 229) return true;
      if (event.isComposing) return false;
      event.preventDefault();
      if (acceptsUserInput && !stopRequested.current) term.input("\x1b[13;2u", true);
      return false;
    });
    const inputEvents = ["keydown", "keypress", "keyup", "beforeinput", "input", "paste", "compositionstart", "compositionupdate",
      "compositionend", "drop", "mousedown", "mouseup", "mousemove", "wheel", "auxclick", "contextmenu"];
    function blockUserInput(event: Event) {
      if (acceptsUserInput) return;
      event.preventDefault();
      event.stopImmediatePropagation();
    }
    // xterm의 onData에는 입력 출처가 없다. 준비 전 DOM 입력만 차단하고 파서 응답은 전달한다.
    if (remote) for (const type of inputEvents) element.addEventListener(type, blockUserInput, { capture: true, passive: false });
    let disposed = false;
    // 웹뷰 기본 메뉴 대신 복사·붙여넣기를 한다. 캡처 단계에서 멈춰 xterm의 우클릭 선택 처리도 막는다.
    const onContextMenu = (event: MouseEvent) => {
      event.preventDefault();
      event.stopPropagation();
      rightClick(term, clipboardRef.current ?? navigator.clipboard, () => !disposed && acceptsUserInput && !stopRequested.current);
    };
    element.addEventListener("contextmenu", onContextMenu, { capture: true });
    const subscription = term.onData((data) => session.current?.input(data, "terminal"));
    const layout = () => {
      if (!element.clientWidth || !element.clientHeight) return;
      fit.fit();
      const dimensions = { cols: term.cols, rows: term.rows };
      if (!session.current) {
        if (stopRequested.current) return;
        session.current = connectLocalTerminal(dimensions, (bytes, parsed) => term.write(bytes, parsed), (next, id) => {
          setStatus(next);
          statusListener.current?.(next, id);
        }, ipc, targetId, remote, !!(remote || ssh), remoteEditor, elevation);
      } else {
        session.current.resize(dimensions);
      }
    };
    const observer = new ResizeObserver(layout);
    observer.observe(element);
    refresh.current = (visible, ready, requested) => {
      acceptsUserInput = visible && ready;
      if (visible) layout();
      if (!visible || !ready) term.blur();
      else if (requested) term.focus();
    };
    return () => {
      refresh.current = null;
      observer.disconnect();
      subscription.dispose();
      if (remote) for (const type of inputEvents) element.removeEventListener(type, blockUserInput, true);
      disposed = true;
      element.removeEventListener("contextmenu", onContextMenu, true);
      session.current?.close();
      session.current = undefined;
      term.dispose();
    };
  }, [targetId, ipc, remote, ssh, remoteEditor, elevation, generation]);

  useLayoutEffect(() => {
    refresh.current?.(active, !closing && status.kind === "running", focusRequested);
  }, [active, closing, status.kind, focusRequested, focusSeq]);

  const message = text(status);
  const failed = status.kind === "failed" && !closing;
  const expired = failed && !!targetId && !elevation && ssoExpired(status.value);
  return (
    <section className="terminal-view" aria-label={label} hidden={!active}>
      <div className="terminal-screen" ref={host} inert={closing || status.kind !== "running"} />
      {(message || expired || (failed && reconnect)) && <div className="terminal-status">
        {message && <span role="status">{message}</span>}
        {expired ? <SsoLoginAction login={ssoLogin} />
          : failed && reconnect && <IconButton icon={RotateCcw} label="다시 연결" small onClick={reopen} />}
      </div>}
    </section>
  );
}
