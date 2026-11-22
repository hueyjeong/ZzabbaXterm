import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useImperativeHandle, useLayoutEffect, useRef, useState, type Ref } from "react";
import { connectLocalTerminal, type LocalTerminal, type TerminalStatus } from "./session";
import "./terminal.css";

export interface TerminalViewProps {
  readonly active: boolean;
  readonly focusRequested: boolean;
  readonly label: string;
  readonly targetId?: string;
  readonly remote?: boolean;
  readonly closing?: boolean;
  readonly closeRef?: Ref<{ confirmClose(): Promise<void> }>;
  readonly onStatus?: (status: TerminalStatus, sessionId?: string) => void;
}

function text(status: TerminalStatus) {
  switch (status.kind) {
    case "opening": return "여는 중";
    case "connecting": return "연결 중";
    case "running": return null;
    case "exited": return `종료 코드 ${status.value}`;
    case "disconnected": return `단절: ${status.value}`;
    case "failed": return `열기 실패: ${status.value}`;
  }
}

export function TerminalView({ active, focusRequested, label, targetId, remote, closing, closeRef, onStatus }: TerminalViewProps) {
  const host = useRef<HTMLDivElement>(null);
  const session = useRef<LocalTerminal | undefined>(undefined);
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

  useLayoutEffect(() => {
    const element = host.current;
    if (!element) return;
    const term = new Terminal({
      allowProposedApi: false,
      fontFamily: '"Cascadia Mono", "DejaVu Sans Mono", monospace',
      fontSize: 14,
      theme: { background: "#12171b", foreground: "#e3e9e6", cursor: "#9cd9bf" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(element);
    let acceptsUserInput = !remote;
    const inputEvents = ["keydown", "keypress", "keyup", "beforeinput", "input", "paste", "compositionstart", "compositionupdate",
      "compositionend", "drop", "mousedown", "mouseup", "mousemove", "wheel", "auxclick", "contextmenu"];
    function blockUserInput(event: Event) {
      if (acceptsUserInput) return;
      event.preventDefault();
      event.stopImmediatePropagation();
    }
    // xterm의 onData에는 입력 출처가 없다. 준비 전 DOM 입력만 차단하고 파서 응답은 전달한다.
    if (remote) for (const type of inputEvents) element.addEventListener(type, blockUserInput, { capture: true, passive: false });
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
        }, undefined, targetId, remote);
      } else {
        session.current.resize(dimensions);
      }
    };
    const observer = new ResizeObserver(layout);
    observer.observe(element);
    refresh.current = (visible, ready, requested) => {
      if (remote) acceptsUserInput = visible && ready;
      if (visible) layout();
      if (!visible || !ready) term.blur();
      else if (requested) term.focus();
    };
    return () => {
      refresh.current = null;
      observer.disconnect();
      subscription.dispose();
      if (remote) for (const type of inputEvents) element.removeEventListener(type, blockUserInput, true);
      session.current?.close();
      session.current = undefined;
      term.dispose();
    };
  }, [targetId, remote]);

  useLayoutEffect(() => {
    refresh.current?.(active, !closing && status.kind === "running", focusRequested);
  }, [active, closing, status.kind, focusRequested]);

  const message = text(status);
  return (
    <section className="terminal-view" aria-label={label} hidden={!active}>
      <div className="terminal-screen" ref={host} inert={closing || status.kind !== "running"} />
      {message && <div className="terminal-status" role="status">{message}</div>}
    </section>
  );
}
