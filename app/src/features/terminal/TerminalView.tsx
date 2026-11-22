import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useLayoutEffect, useRef, useState } from "react";
import { connectLocalTerminal, type LocalTerminal, type TerminalStatus } from "./session";
import "./terminal.css";

export interface TerminalViewProps {
  readonly active: boolean;
  readonly label: string;
  readonly onStatus?: (status: TerminalStatus, sessionId?: string) => void;
}

function text(status: TerminalStatus) {
  switch (status.kind) {
    case "opening": return "여는 중";
    case "running": return null;
    case "exited": return `종료 코드 ${status.value}`;
    case "disconnected": return `단절: ${status.value}`;
    case "failed": return `열기 실패: ${status.value}`;
  }
}

// Keep the component mounted with a stable tab key; only unmount closes the session.
export function TerminalView({ active, label, onStatus }: TerminalViewProps) {
  const host = useRef<HTMLDivElement>(null);
  const refresh = useRef<(() => void) | null>(null);
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
    let session: LocalTerminal | undefined;
    const subscription = term.onData((data) => session?.input(data));
    const layout = () => {
      if (!element.clientWidth || !element.clientHeight) return;
      fit.fit();
      const dimensions = { cols: term.cols, rows: term.rows };
      if (!session) {
        session = connectLocalTerminal(dimensions, (bytes, parsed) => term.write(bytes, parsed), (next, id) => {
          setStatus(next);
          statusListener.current?.(next, id);
        });
      } else {
        session.resize(dimensions);
      }
    };
    const observer = new ResizeObserver(layout);
    observer.observe(element);
    refresh.current = () => {
      layout();
      term.focus();
    };
    return () => {
      refresh.current = null;
      observer.disconnect();
      subscription.dispose();
      session?.close();
      term.dispose();
    };
  }, []);

  useLayoutEffect(() => {
    if (active) refresh.current?.();
  }, [active]);

  const message = text(status);
  return (
    <section className="terminal-view" aria-label={label} hidden={!active}>
      <div className="terminal-screen" ref={host} />
      {message && <div className="terminal-status" role="status">{message}</div>}
    </section>
  );
}
