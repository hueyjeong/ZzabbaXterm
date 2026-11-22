import { Channel, invoke } from "@tauri-apps/api/core";

export type TerminalStatus =
  | { kind: "opening" | "running" }
  | { kind: "exited"; value: number }
  | { kind: "disconnected" | "failed"; value: string };

type End = Extract<TerminalStatus, { kind: "exited" }> | { kind: "disconnected"; value: string };
export type Dimensions = { rows: number; cols: number };
type Message = ArrayBuffer | End;

export interface LocalTerminalIpc {
  open(size: Dimensions, receive: (message: Message) => void): Promise<string>;
  start(id: string): Promise<void>;
  input(id: string, bytes: Uint8Array): Promise<void>;
  resize(id: string, size: Dimensions): Promise<void>;
  ack(id: string, position: number): Promise<void>;
  close(id: string): Promise<void>;
}

const tauriIpc: LocalTerminalIpc = {
  open: (size, receive) => invoke("terminal_open", { ...size, output: new Channel(receive) }),
  start: (id) => invoke("terminal_start", { id }),
  input: (id, bytes) => invoke("terminal_input", { id, bytes: Array.from(bytes) }),
  resize: (id, size) => invoke("terminal_resize", { id, ...size }),
  ack: (id, position) => invoke("terminal_ack", { id, position }),
  close: (id) => invoke("terminal_close", { id }),
};

export interface LocalTerminal {
  input(data: string): void;
  resize(size: Dimensions): void;
  close(): void;
}

export function connectLocalTerminal(
  initialSize: Dimensions,
  write: (bytes: Uint8Array, parsed: () => void) => void,
  status: (status: TerminalStatus, sessionId?: string) => void,
  ipc = tauriIpc,
): LocalTerminal {
  let id: string | undefined;
  let stopped = false;
  let started = false;
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
    release();
    status({ kind: started ? "disconnected" : "failed", value: String(error) }, sessionId);
  };
  const receive = (message: Message) => {
    if (stopped || id === undefined) return;
    if (message instanceof ArrayBuffer) {
      const handle = id;
      try {
        write(new Uint8Array(message), () => {
          if (stopped) return;
          position += message.byteLength;
          void ipc.ack(handle, position).catch(fail);
        });
      } catch (error) {
        fail(error);
      }
      return;
    }
    const sessionId = id;
    stopped = true;
    id = undefined;
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
  void (async () => {
    // React StrictMode can dispose the first mount before any native session is needed.
    await Promise.resolve();
    if (stopped) return;
    const opened = await ipc.open(initialSize, receive);
    id = opened;
    if (stopped) {
      release();
      return;
    }
    status({ kind: "opening" }, opened);
    await ipc.start(opened);
    if (stopped) return;
    started = true;
    status({ kind: "running" }, opened);
    if (latestSize !== initialSize) flushSize(latestSize);
  })().catch(fail);

  return {
    input(data: string) {
      if (stopped || id === undefined) return;
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
      release();
    },
  };
}
