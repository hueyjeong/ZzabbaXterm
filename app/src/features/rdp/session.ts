import { Channel, convertFileSrc, invoke, type InvokeArgs } from "@tauri-apps/api/core";
import { approvalSignal, type RemoteDesktopEvent, type RemoteDesktopIdentity, type RemoteDesktopSession } from "../remoteDesktop/session";
import type { DesktopTunnelSource } from "../tunnels/tunnelIpc";
import { scanCode } from "./keys";

interface Draft { readonly host: string; readonly port: number; readonly tlsOnly?: boolean; readonly source?: DesktopTunnelSource }
export interface Capabilities { readonly copy: boolean; readonly paste: boolean; readonly resize: boolean }
export interface Connection extends RemoteDesktopSession {
  clipboard(action: "copy" | "paste"): void;
  resize(width: number, height: number): void;
}

const CHUNK = 256 * 1024;
const MAX_PIXELS = 3840 * 2160;
const MAX_CLIPBOARD_BYTES = 64 * 1024;
const MAX_QUEUE = 512;
const MAX_BATCH = 64;
const BUSY_RETRY_MS = 15;
/** 같은 입력이 이 시간 넘게 busy면 버리고 큐가 빈 뒤 해제한다. */
const BUSY_LIMIT_MS = 3000;
export const DROPPED_NOTICE = "RDP 입력 일부를 버렸습니다";
export const BUSY_NOTICE = "RDP 세션이 작업 중입니다";
/** `transport`는 전송 연결이 끊긴 종료다. 터널 경로는 경로를 다시 열어야 재연결할 수 있다. */
export type EndKind = "transport";
const token = /^[a-f0-9]{32}$/;
function proof(): string | undefined {
  if (window !== top) return;
  const value = (window as Window & { __ZZABBA_RDP_DOCUMENT__?: { proof?: unknown } }).__ZZABBA_RDP_DOCUMENT__?.proof;
  if (typeof value === "string" && token.test(value)) return value;
}
const positive = (value: unknown): value is number => Number.isSafeInteger(value) && Number(value) > 0;
const nonnegative = (value: unknown): value is number => value === 0 || positive(value);
const record = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null;

type Action = { kind: "active"; active: boolean } | { kind: "release" } | { kind: "copy" } |
{ kind: "key"; code: number; extended: boolean; down: boolean } |
{ kind: "pointer"; x: number; y: number; buttons: number } |
{ kind: "paste"; text: string } | { kind: "resize"; width: number; height: number };
type Frame = Extract<RemoteDesktopEvent, { kind: "full" | "patch" }>;
type Queued = { action: Action; epoch: number; displayGeneration: number };
/** 입력 한 건의 결과다. busy는 Rust가 순번을 쓰지 않았으므로 같은 순번으로 다시 보낸다. */
type Reply = "ok" | "stale" | "busy" | { error: string; kind?: string };
const pause = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
/** 사용자 동작이라 묶음에 넣지 않고, busy여도 다시 보내지 않는다. */
const control = (action: Action) => action.kind === "copy" || action.kind === "paste" || action.kind === "resize";
/** Rust 실패 값 `{ kind?, message }`와 문자열 거부를 함께 읽는다. */
function failure(error: unknown): { message: string; kind?: EndKind } {
  if (record(error) && typeof error.message === "string") return { message: error.message, kind: error.kind === "transport" ? "transport" : undefined };
  return { message: String(error) };
}

export function connect(draft: Draft, opened: (connection: Connection) => void,
  caps: (value: Capabilities) => void, ready: (value: boolean) => void, notice: (message: string) => void,
  ended: (message: string, closed: boolean, kind?: EndKind) => void, clipboard: Pick<Clipboard, "readText" | "writeText"> = navigator.clipboard): () => Promise<void> {
  const document = proof();
  const life = new AbortController();
  let output: Channel<string> | undefined;
  const run = <T = void>(name: string, args: InvokeArgs = {}) => {
    if (!document || proof() !== document) return Promise.reject("RDP 문서가 교체되었거나 아직 준비되지 않았습니다");
    return invoke<T>(name, args, { headers: { "x-zzabba-rdp-document": document } });
  };
  let id: RemoteDesktopIdentity | undefined;
  let receive: ((event: RemoteDesktopEvent) => boolean) | undefined;
  let waiting: (() => void) | undefined;
  let pending: { frame: Frame; packet: number; bytes: number } | undefined;
  let reading = false;
  let done = false;
  let active = false;
  let displayGeneration = 0;
  let frameGeneration = 0;
  let epoch = 0;
  let serial = 0;
  let lastPacket = 0;
  let lastFrame = 0;
  const queue: Queued[] = [];
  let sending = false;
  let dropped = false;
  let busySince: { item: Queued; at: number } | undefined;
  let closing: Promise<void> | undefined;
  const stop = () => {
    if (done) return;
    done = true;
    life.abort();
    waiting?.();
    if (output) output.onmessage = () => { };
    queue.length = 0;
    pending = undefined;
  };
  const close = () => {
    stop();
    if (!closing) {
      closing = (async () => {
        await prepared.catch(() => undefined);
        if (id) await run("rdp_close", { id });
      })().catch((error: unknown) => {
        closing = undefined;
        throw error;
      });
    }
    return closing;
  };
  const end = (message: string, kind?: EndKind) => {
    if (done) return;
    const report = (text: string, closed: boolean) => kind ? ended(text, closed, kind) : ended(text, closed);
    void close().then(() => report(message, true), (error: unknown) => report(String(error), false));
  };
  /** 버린 입력이 있으면 한 번 알리고, 큐가 빈 뒤 해제를 보내게 표시한다. */
  function markDropped() {
    if (!dropped) notice(DROPPED_NOTICE);
    dropped = true;
  }

  async function flush() {
    if (sending || done || !id) return;
    sending = true;
    try {
      while (!done) {
        if (!queue.length) {
          // 입력을 버린 뒤 큐가 비면 한 번 해제해 눌린 키나 버튼이 남지 않게 한다. 비활성 전환은 Rust가 따로 해제한다.
          if (!dropped || !active || !displayGeneration) { dropped = false; return; }
          dropped = false;
          queue.push({ action: { kind: "release" }, epoch, displayGeneration });
        }
        // copy·paste·resize는 단건으로만 보내 한 항목의 형식 오류가 묶음 전체를 실패시키지 않게 한다.
        let count = 1;
        if (!control(queue[0].action)) while (count < queue.length && count < MAX_BATCH && !control(queue[count].action)) count++;
        const batch = queue.splice(0, count);
        const inputs = batch.map((next, index) => ({ ...next, sequence: serial + index + 1 }));
        let replies: unknown;
        try {
          replies = inputs.length === 1
            ? [await run<Reply>("rdp_input", { id, input: inputs[0] }).catch((error: unknown) => {
              const { message, kind } = failure(error);
              return { error: message, kind };
            })]
            : await run<Reply[]>("rdp_input_batch", { id, inputs });
        } catch (error) {
          const { message, kind } = failure(error);
          if (!done) end(message, kind);
          return;
        }
        if (done) return;
        let busy = false;
        for (const [index, next] of batch.entries()) {
          const reply: unknown = Array.isArray(replies) ? replies[index] : undefined;
          if (reply === "busy") {
            // 적용되지 않은 항목은 같은 순번으로 다시 보낸다. 그 사이 화면이나 활성 상태가 바뀌었으면 버린다.
            const rest = (from: number) => queue.unshift(...batch.slice(from).filter((item) => item.epoch === epoch));
            if (control(next.action)) {
              notice(BUSY_NOTICE);
              rest(index + 1);
              break;
            }
            const now = Date.now();
            if (busySince?.item !== next) busySince = { item: next, at: now };
            else if (now - busySince.at >= BUSY_LIMIT_MS) {
              busySince = undefined;
              markDropped();
              rest(index + 1);
              break;
            }
            rest(index);
            busy = true;
            break;
          }
          if (reply !== "ok" && reply !== "stale" && !(record(reply) && typeof reply.error === "string")) {
            end("RDP 입력 응답 오류"); return;
          }
          if (busySince?.item === next) busySince = undefined;
          serial++;
          if (reply === "stale") {
            if (next.displayGeneration === displayGeneration) {
              displayGeneration = 0;
              queue.length = 0;
              epoch++;
            }
          } else if (reply !== "ok") {
            const error = String(reply.error);
            if (control(next.action)) notice(error);
            else { end(error, reply.kind === "transport" ? "transport" : undefined); return; }
          }
        }
        if (busy) await pause(BUSY_RETRY_MS);
      }
    } finally { sending = false; }
  }
  /** 버튼 상태가 직전 포인터와 같은 이동은 뒤 이동이 위치를 대신하므로 오래된 것부터 버린다. 마지막 포인터는 최종 위치라 남긴다. */
  function dropMove() {
    let lastPointer = queue.length - 1;
    while (lastPointer >= 0 && queue[lastPointer].action.kind !== "pointer") lastPointer--;
    let buttons: number | undefined;
    for (const [index, { action }] of queue.entries()) {
      if (index >= lastPointer) break;
      if (action.kind !== "pointer") continue;
      if (action.buttons === buttons) { queue.splice(index, 1); return true; }
      buttons = action.buttons;
    }
    return false;
  }
  function send(action: Action) {
    if (done || !displayGeneration || (action.kind !== "active" && !active)) return;
    const last = queue[queue.length - 1];
    if (action.kind === "pointer" && last?.action.kind === "pointer" && last.action.buttons === action.buttons) {
      last.action = action;
    } else {
      if (queue.length >= MAX_QUEUE && action.kind !== "active" && action.kind !== "release" && !dropMove()) {
        markDropped();
        return;
      }
      queue.push({ action, epoch, displayGeneration });
    }
    void flush();
  }
  function setActive(next: boolean) {
    if (active === next) return;
    active = next;
    epoch++;
    queue.length = 0;
    send({ kind: "active", active });
  }
  function clipboardAction(action: "copy" | "paste") {
    if (done || !active || !displayGeneration) return;
    if (!clipboard) { notice("로컬 클립보드를 사용할 수 없습니다"); return; }
    if (action === "copy") { send({ kind: "copy" }); return; }
    const previous = epoch;
    void clipboard.readText().then((text) => {
      if (done || epoch !== previous) return;
      if (text.length > MAX_CLIPBOARD_BYTES || text.includes("\0") || new TextEncoder().encode(text).length > MAX_CLIPBOARD_BYTES) {
        notice("RDP 클립보드 텍스트 범위 오류");
        return;
      }
      send({ kind: "paste", text });
    }).catch(() => { if (!done && epoch === previous) notice("로컬 클립보드 읽기 실패"); });
  }

  async function packet(ticket: unknown) {
    if (done) return;
    if (reading || !id || !document || proof() !== document || typeof ticket !== "string" || !token.test(ticket)) {
      end("RDP 화면 수신 오류"); return;
    }
    reading = true;
    try {
      const response = await fetch(convertFileSrc(`${document}-${ticket}`, "rdp-packet"), { cache: "no-store", signal: life.signal });
      if (!response.ok || done || proof() !== document) throw new Error();
      let sequence: number;
      let offset = 0;
      const type = response.headers.get("content-type");
      if (type === "application/octet-stream") {
        const buffer = await response.arrayBuffer();
        if (done || proof() !== document || !pending || buffer.byteLength <= 12 || buffer.byteLength > CHUNK + 12) throw new Error();
        const view = new DataView(buffer);
        sequence = Number(view.getBigUint64(0, true));
        const start = view.getUint32(8, true);
        const chunk = new Uint8Array(buffer, 12);
        if (sequence !== pending.packet || start !== pending.bytes || chunk.byteLength !== Math.min(CHUNK, pending.frame.rgba.length - start)) throw new Error();
        pending.frame.rgba.set(chunk, start);
        pending.bytes += chunk.length;
        offset = start + 1;
        if (pending.bytes === pending.frame.rgba.length) {
          if (!receive) {
            const gate = Promise.withResolvers<void>();
            waiting = gate.resolve;
            await gate.promise;
          }
          if (done || proof() !== document) return;
          if (!receive?.(pending.frame)) throw new Error();
          lastFrame = pending.frame.sequence;
          pending = undefined;
        }
      } else if (type === "application/json") {
        const value: unknown = await response.json();
        if (done || proof() !== document || pending || !record(value) || !positive(value.sequence) || value.sequence <= lastPacket) throw new Error();
        sequence = lastPacket = value.sequence;
        switch (value.kind) {
          case "frame": {
            const { width, height, x, y, w, h, full, frameSequence, displayGeneration: generation } = value;
            if (!positive(width) || !positive(height) || width > 4096 || height > 4096 || width * height > MAX_PIXELS ||
              !nonnegative(x) || !nonnegative(y) || !positive(w) || !positive(h) || w > width - x || h > height - y ||
              frameSequence !== lastFrame + 1 || typeof full !== "boolean" || !positive(generation) || generation < frameGeneration ||
              (generation !== frameGeneration && !full) ||
              (full && (x !== 0 || y !== 0 || w !== width || h !== height))) throw new Error();
            if (generation !== frameGeneration) {
              frameGeneration = generation;
              displayGeneration = 0;
              queue.length = 0;
              epoch++;
            }
            pending = {
              packet: sequence, bytes: 0, frame: {
                ...id, sequence: frameSequence, kind: full ? "full" : "patch",
                size: { width, height }, rect: { x, y, width: w, height: h }, rgba: new Uint8Array(w * h * 4)
              }
            };
            break;
          }
          case "display":
            if (!lastFrame || value.displayGeneration !== frameGeneration || displayGeneration) throw new Error();
            displayGeneration = frameGeneration;
            send({ kind: "active", active });
            ready(true);
            break;
          case "channels":
            if (typeof value.copy !== "boolean" || typeof value.paste !== "boolean" || typeof value.resize !== "boolean") throw new Error();
            caps({ copy: value.copy, paste: value.paste, resize: value.resize });
            break;
          case "clipboard":
            if (displayGeneration && positive(value.epoch) && value.epoch === epoch && active && clipboard) {
              if (typeof value.text === "string" && value.text.length <= MAX_CLIPBOARD_BYTES &&
                !value.text.includes("\0") && new TextEncoder().encode(value.text).length <= MAX_CLIPBOARD_BYTES) {
                void clipboard.writeText(value.text).catch(() => { if (!done) notice("로컬 클립보드 쓰기 실패"); });
              } else if (typeof value.error === "string") notice(value.error);
              else throw new Error();
            }
            break;
          default: throw new Error();
        }
      } else throw new Error();
      reading = false;
      await run("rdp_ack", { id, sequence, offset });
    } catch { end("RDP 화면 또는 상태를 적용할 수 없습니다"); }
  }

  const prepared = run<unknown>("rdp_prepare").then((assigned) => {
    if (!record(assigned) || typeof assigned.sessionId !== "string" || !token.test(assigned.sessionId) || !positive(assigned.generation)) throw new Error("RDP 세션 ID 오류");
    id = { sessionId: assigned.sessionId, generation: assigned.generation };
    return id;
  });
  void (async () => {
    try {
      const assigned = await prepared;
      if (done) return;
      output = new Channel<string>((ticket: unknown) => {
        if (!approvalSignal(ticket, notice)) void packet(ticket);
      });
      const completion = run("rdp_connect", { id, draft, output });
      opened({
        ...assigned,
        subscribe(listener) {
          receive = listener;
          waiting?.();
          waiting = undefined;
          return () => { receive = undefined; };
        },
        setActive,
        send(input) {
          if (input.sessionId !== id?.sessionId || input.generation !== id.generation) return;
          switch (input.kind) {
            case "release-input": send({ kind: "release" }); break;
            case "pointer": send({ kind: "pointer", x: input.x, y: input.y, buttons: input.buttons & 7 }); break;
            case "key": {
              const scan = scanCode(input.code);
              if (scan) send({ kind: "key", ...scan, down: input.down });
              break;
            }
          }
        },
        clipboard: clipboardAction,
        resize(width, height) { send({ kind: "resize", width, height }); },
      });
      await completion;
      end("RDP 연결이 종료되었습니다");
    } catch (error) {
      const { message, kind } = failure(error);
      end(message, kind);
    }
  })();
  return close;
}
