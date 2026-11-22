import type { DesktopTunnelSource } from "../tunnels/tunnelIpc";
import { Channel } from "@tauri-apps/api/core";
import { documentProof, invokeVnc, readPacket } from "./document";
import type { RemoteDesktopEvent, RemoteDesktopIdentity, RemoteDesktopSession } from "../remoteDesktop/session";
import { keysym } from "./keys";

export interface Owner { readonly tab: string; readonly document: number }
export type Draft = Readonly<{ password: boolean } & (
  | { host: string; port: number; source?: never }
  | { source: DesktopTunnelSource; host?: never; port?: never }
)>;

type Frame = Extract<RemoteDesktopEvent, { kind: "full" | "patch" }>;
const HEADER = 33;
const CHUNK = 256 * 1024;
const MAX_PIXELS = 8_294_400;

type Action = { kind: "release" } | { kind: "key"; keysym: number; down: boolean } |
{ kind: "pointer"; x: number; y: number; buttons: number } |
{ kind: "cutText"; text: string } | { kind: "resize"; width: number; height: number };

export const createOwner = () => invokeVnc<Owner>("vnc_create");
export const closeOwner = (owner: Owner) => invokeVnc("vnc_close", { owner, generation: null });

export interface Connection extends RemoteDesktopSession {
  resize(width: number, height: number): void;
}

export function connect(
  owner: Owner, draft: Draft, started: (session: Connection) => void,
  notice: (message: string) => void, failed: (message: string) => void,
  clipboard: Pick<Clipboard, "readText" | "writeText"> = navigator.clipboard,
): () => void {
  const proof = documentProof();
  const lifetime = new AbortController();
  const invoke = <T = void>(name: string, args: Record<string, unknown>) => invokeVnc<T>(name, args, proof);
  let identity: RemoteDesktopIdentity | undefined;
  let receive: ((event: RemoteDesktopEvent) => boolean) | undefined;
  let pending: RemoteDesktopEvent | undefined;
  let done = false;
  let epoch = 0;
  let ready = false;
  let active = false;
  let inputLifetime = 0;
  let clipboardText: string | undefined;
  let lastFrame = 0;
  let serial = 0;
  let assembling: { frame: Frame; epoch: number; length: number } | undefined;
  let sending = false;
  const queued: { action: Action; epoch: number }[] = [];
  let pendingFrame: { frame: Frame; ack: () => void } | undefined;

  function emit(event: RemoteDesktopEvent) {
    if (receive) receive(event);
    else pending = event;
  }

  function terminate(message: string, close: boolean) {
    if (done) return;
    done = true;
    lifetime.abort();
    ready = false;
    assembling = undefined;
    pendingFrame = undefined;
    if (close) void invoke("vnc_close", { owner, generation: identity?.generation ?? null }).catch(() => { });
    queued.length = 0;
    clipboardText = undefined;
    if (identity) emit({ ...identity, sequence: lastFrame + 1, kind: "disconnected", message });
    else failed(message);
  }

  async function flush() {
    if (sending || done || !identity) return;
    sending = true;
    try {
      for (let next = queued.shift(); !done && next; next = queued.shift()) {
        await invoke("vnc_input", { owner, input: { ...identity, ...next, sequence: ++serial } });
      }
    } catch (error) {
      terminate(String(error), true);
    } finally {
      sending = false;
    }
  }

  function send(action: Action) {
    if (done || !ready || (!active && action.kind !== "release")) return;
    const previous = queued[queued.length - 1];
    if (action.kind === "pointer" && previous?.action.kind === "pointer" && previous.action.buttons === action.buttons) {
      previous.action = action;
    } else if (queued.length < 32) {
      queued.push({ action, epoch });
    } else {
      terminate("VNC 입력 큐가 가득 찼습니다", true);
      return;
    }
    void flush();
  }

  function releaseInput() {
    ++inputLifetime;
    queued.length = 0;
    send({ kind: "release" });
  }

  function applyFrame(frame: Frame, ack: () => void) {
    if (!receive) { pendingFrame = { frame, ack }; return; }
    if (!receive(frame)) {
      terminate("원격 화면을 그릴 수 없습니다", true);
      return;
    }
    lastFrame = frame.sequence;
    ready = true;
    ack();
  }

  function establish(id: RemoteDesktopIdentity) {
    identity = id;
    started({
      ...id,
      subscribe(listener) {
        receive = listener;
        if (pending) { const event = pending; pending = undefined; listener(event); }
        if (pendingFrame) {
          const { frame, ack } = pendingFrame;
          pendingFrame = undefined;
          applyFrame(frame, ack);
        }
        return () => { receive = undefined; };
      },
      setActive(value) {
        if (active === value) return;
        active = value;
        if (!active) releaseInput();
      },
      send(input) {
        if (input.sessionId !== id.sessionId || input.generation !== id.generation) return;
        switch (input.kind) {
          case "release-input": if (active) releaseInput(); break;
          case "key": {
            const symbol = keysym(input.key, input.location, input.code);
            if (symbol !== undefined) send({ kind: "key", keysym: symbol, down: input.down });
            break;
          }
          case "pointer": send({ kind: "pointer", x: input.x, y: input.y, buttons: (input.buttons & 1) | ((input.buttons & 2) << 1) | ((input.buttons & 4) >> 1) }); break;
        }
      },
      resize(width, height) { send({ kind: "resize", width, height }); },
      requestClipboard(request) {
        if (done || !ready || !active || request.sessionId !== id.sessionId || request.generation !== id.generation) return;
        const at = inputLifetime;
        if (request.action === "copy") {
          if (clipboardText === undefined) { notice("서버 클립보드가 비어 있습니다"); return; }
          void clipboard.writeText(clipboardText).catch(() => { if (!done) notice("클립보드 쓰기 실패"); });
        } else {
          void clipboard.readText().then((text) => {
            if (done || inputLifetime !== at || !ready || !active) return;
            if (text.length > 1024 * 1024 || /[^\x00-\xff]/u.test(text)) { notice("VNC 클립보드는 1 MiB 이하의 Latin-1 텍스트만 지원합니다"); return; }
            send({ kind: "cutText", text });
          }).catch(() => { if (!done && inputLifetime === at) notice("클립보드 읽기 실패"); });
        }
      },
    });
  }

  function binary(buffer: ArrayBuffer) {
    if (!identity || buffer.byteLength <= HEADER || buffer.byteLength > HEADER + CHUNK) throw new Error();
    const data = new DataView(buffer);
    const kind = data.getUint8(0);
    const sequence = Number(data.getBigUint64(1, true));
    const screen = Number(data.getBigUint64(9, true));
    const [width, height, x, y, w, h] = [17, 19, 21, 23, 25, 27].map((pos) => data.getUint16(pos, true));
    const offset = data.getUint32(29, true);
    const total = w * h * 4;
    if (!Number.isSafeInteger(sequence) || sequence !== lastFrame + 1 || screen !== epoch ||
      !width || !height || width > 8192 || height > 8192 || width * height > MAX_PIXELS ||
      !w || !h || x + w > width || y + h > height || (kind !== 1 && kind !== 2) ||
      (kind === 1 ? x !== 0 || y !== 0 || w !== width || h !== height : !ready) ||
      offset >= total || buffer.byteLength !== HEADER + Math.min(total - offset, CHUNK)) throw new Error();
    const bytes = new Uint8Array(buffer, HEADER);
    if (offset === 0) {
      if (assembling) throw new Error();
      const frame: Frame = {
        ...identity, sequence, kind: kind === 1 ? "full" : "patch", size: { width, height },
        rect: { x, y, width: w, height: h }, rgba: total === bytes.length ? bytes : new Uint8Array(total)
      };
      assembling = { frame, epoch: screen, length: 0 };
    }
    const current = assembling;
    if (!current || current.length !== offset || current.epoch !== screen || (current.frame.kind === "full") !== (kind === 1) ||
      current.frame.size.width !== width || current.frame.size.height !== height || current.frame.rect.x !== x || current.frame.rect.y !== y ||
      current.frame.rect.width !== w || current.frame.rect.height !== h) throw new Error();
    if (current.frame.rgba !== bytes) current.frame.rgba.set(bytes, offset);
    current.length += bytes.length;
    const ack = () => void invoke("vnc_ack", { owner, generation: identity?.generation, sequence, offset })
      .catch((error) => terminate(String(error), true));
    if (current.length === total) {
      assembling = undefined;
      applyFrame(current.frame, ack);
    } else ack();
  }

  function dispose() {
    done = true;
    lifetime.abort();
    ready = false;
    receive = undefined;
    pending = undefined;
    pendingFrame = undefined;
    assembling = undefined;
    clipboardText = undefined;
    queued.length = 0;
  }

  const output = new Channel<unknown>((ticket) => { void handle(ticket); });
  async function handle(ticket: unknown) {
    if (done) return;
    try {
      const message = await readPacket(proof, ticket, lifetime.signal);
      if (done) return;
      if (message instanceof ArrayBuffer) { binary(message); return; }
      if (!record(message) || !record(message.identity)) throw new Error();
      const { sessionId, generation } = message.identity;
      if (typeof sessionId !== "string" || typeof generation !== "number" || !Number.isSafeInteger(generation) || generation < 1) throw new Error();
      if (message.kind === "started" && !identity && message.serial === 0) establish({ sessionId, generation });
      if (identity?.sessionId !== sessionId || identity.generation !== generation) return;
      switch (message.kind) {
        case "waiting": {
          const next = message.value;
          if (typeof next !== "number" || !Number.isSafeInteger(next) || next !== (epoch === 0 ? 1 : epoch + 2)) throw new Error();
          epoch = next;
          ready = false;
          ++inputLifetime;
          queued.length = 0;
          emit({ ...identity, kind: "waiting", sequence: lastFrame + 1 });
          break;
        }
        case "clipboard": if (typeof message.value === "string") clipboardText = message.value; break;
        case "notice": if (typeof message.value === "string") notice(message.value); break;
        case "closed":
          if (message.serial === 0xffffffff && typeof message.value === "string") {
            await invoke("vnc_ack", { owner, generation, sequence: 0, offset: message.serial });
            terminate(message.value, false);
          }
          return;
      }
      if (typeof message.serial === "number" && Number.isInteger(message.serial) && message.serial >= 0 && message.serial < 0xffffffff) {
        void invoke("vnc_ack", { owner, generation, sequence: 0, offset: message.serial })
          .catch((error) => terminate(String(error), true));
      }
    } catch {
      terminate("VNC 화면 데이터가 올바르지 않습니다", true);
    }
  }
  void invoke("vnc_connect", { owner, draft, output }).catch((error) => terminate(String(error), false));
  return dispose;
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
