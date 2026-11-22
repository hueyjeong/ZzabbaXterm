import { Channel, invoke } from "@tauri-apps/api/core";

export type TerminalStatus =
  | { kind: "opening" | "connecting" | "running" }
  | { kind: "exited"; value: number }
  | { kind: "disconnected" | "failed"; value: string };

type End = Extract<TerminalStatus, { kind: "exited" }> | { kind: "disconnected"; value: string };
export type Dimensions = { rows: number; cols: number };
type Message = ArrayBuffer | End | { kind: "connecting" | "ready" };

export interface LocalTerminalIpc {
  open(size: Dimensions, receive: (message: Message) => void, targetId?: string): Promise<string>;
  start(id: string): Promise<void>;
  input(id: string, bytes: Uint8Array): Promise<void>;
  resize(id: string, size: Dimensions): Promise<void>;
  ack(id: string, position: number): Promise<void>;
  close(id: string): Promise<unknown>;
}

const tauriIpc: LocalTerminalIpc = {
  open: (size, receive, targetId) => invoke("terminal_open", {
    ...size, output: new Channel(receive), ...(targetId === undefined ? {} : { targetId }),
  }),
  start: (id) => invoke("terminal_start", { id }),
  input: (id, bytes) => invoke("terminal_input", { id, bytes: Array.from(bytes) }),
  resize: (id, size) => invoke("terminal_resize", { id, ...size }),
  ack: (id, position) => invoke("terminal_ack", { id, position }),
  close: (id) => invoke("terminal_close", { id }),
};

export interface LocalTerminal {
  input(data: string, source?: "user" | "terminal"): void;
  resize(size: Dimensions): void;
  close(): void;
  confirmClose(): Promise<void>;
}

export function connectLocalTerminal(
  initialSize: Dimensions,
  write: (bytes: Uint8Array, parsed: () => void) => void,
  status: (status: TerminalStatus, sessionId?: string) => void,
  ipc = tauriIpc,
  targetId?: string,
  remote = false,
  confirmExit = remote,
): LocalTerminal {
  let id: string | undefined;
  let stopped = false;
  let started = false;
  let receiving = false;
  let ready = false;
  let closing: Promise<void> | undefined;
  // Rust limits unacknowledged output, including bytes received before the start reply.
  const earlyOutput: Message[] = [];
  let latestSize = initialSize;
  let position = 0;
  let inputs = Promise.resolve();
  let resizes = Promise.resolve();
  const encoder = new TextEncoder();

  const release = () => {
    if (id === undefined) return;
    const handle = id;
    id = undefined;
    // A concurrent native exit may already have removed this ID.
    void ipc.close(handle).catch(console.error);
  };
  const failOperation = (error: unknown) => {
    // Native exit can reject input/resize before its ordered final status arrives.
    if (error !== "세션이 닫혔습니다") fail(error);
  };
  const fail = (error: unknown) => {
    if (stopped) return;
    const sessionId = id;
    stopped = true;
    earlyOutput.length = 0;
    if (!confirmExit) release();
    status({ kind: started ? "disconnected" : "failed", value: String(error) }, sessionId);
  };
  const receive = (message: Message) => {
    if (stopped || id === undefined) return;
    if (!receiving) {
      earlyOutput.push(message);
      return;
    }
    if (message instanceof ArrayBuffer) {
      const handle = id;
      try {
        write(new Uint8Array(message), () => {
          if (stopped) return;
          position += message.byteLength;
          void ipc.ack(handle, position).catch(failOperation);
        });
      } catch (error) {
        fail(error);
      }
      return;
    }
    if (message.kind === "connecting" || message.kind === "ready") {
      if (!remote || ready) return;
      ready = message.kind === "ready";
      status({ kind: ready ? "running" : "connecting" }, id);
      return;
    }
    if (message.kind !== "exited" && message.kind !== "disconnected") return;
    const sessionId = id;
    stopped = true;
    if (!confirmExit) id = undefined;
    status(message, sessionId);
  };

  const flushSize = (size: Dimensions) => {
    resizes = resizes.then(async () => {
      if (!stopped && size === latestSize && id !== undefined) await ipc.resize(id, size);
    }).catch((error: unknown) => {
      if (size === latestSize) failOperation(error);
    });
  };

  status({ kind: "opening" });
  // React StrictMode에서 첫 마운트가 해제되면 네이티브 세션을 열지 않는다.
  const opened = Promise.resolve().then(async () => {
    if (stopped) return;
    id = await ipc.open(initialSize, receive, targetId);
    return id;
  });
  void (async () => {
    const sessionId = await opened;
    if (sessionId === undefined) return;
    if (stopped) {
      if (remote) status({ kind: "opening" }, sessionId);
      if (!confirmExit) release();
      return;
    }
    status({ kind: "opening" }, sessionId);
    const starting = ipc.start(sessionId);
    if (remote) {
      receiving = true;
      for (const message of earlyOutput) receive(message);
      earlyOutput.length = 0;
    }
    await starting;
    if (stopped) return;
    started = true;
    if (!remote) {
      receiving = ready = true;
      for (const message of earlyOutput) receive(message);
      earlyOutput.length = 0;
      if (stopped) return;
      status({ kind: "running" }, sessionId);
    }
    if (latestSize !== initialSize) flushSize(latestSize);
  })().catch(fail);

  return {
    input(data: string, source = "user") {
      if (stopped || id === undefined || !(source === "terminal" ? receiving : ready)) return;
      inputs = inputs.then(async () => {
        if (!stopped && id !== undefined) await ipc.input(id, encoder.encode(data));
      }).catch(failOperation);
    },
    resize(size: Dimensions) {
      if (size.rows === latestSize.rows && size.cols === latestSize.cols) return;
      latestSize = size;
      if (started) flushSize(size);
    },
    close() {
      stopped = true;
      earlyOutput.length = 0;
      if (confirmExit) void this.confirmClose().catch(console.error);
      else release();
    },
    confirmClose() {
      stopped = true;
      earlyOutput.length = 0;
      closing ??= opened.then(async (sessionId) => {
        if (sessionId === undefined || id === undefined) return;
        if (await ipc.close(sessionId) !== null) throw new Error("종료 확인 불가");
        id = undefined;
      }, () => { /* 열린 앱 세션 ID가 없으면 시작을 호출하지 않았다. */ })
        .finally(() => { closing = undefined; });
      return closing;
    },
  };
}
