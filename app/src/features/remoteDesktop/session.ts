export const APPROVAL_NOTICE = "승인 창 확인 필요";

/** Rust가 네이티브 승인 창을 띄우고 닫을 때 출력 채널로 보내는 신호를 읽는다. 대기 끝은 승인 대기 알림만 지운다. 신호가 아니면 false다. */
export function approvalSignal(ticket: unknown, notice: (message: string) => void, current: () => string) {
  if (ticket === "approval") notice(APPROVAL_NOTICE);
  else if (ticket === "approval-done") { if (current() === APPROVAL_NOTICE) notice(""); }
  else return false;
  return true;
}

export interface RemoteDesktopIdentity {
  readonly sessionId: string;
  readonly generation: number;
}

type Sequenced = RemoteDesktopIdentity & { readonly sequence: number };

export interface RemoteDesktopSize {
  readonly width: number;
  readonly height: number;
}

export type RemoteDesktopEvent = Sequenced & (
  | {
    readonly kind: "full" | "patch";
    readonly size: RemoteDesktopSize;
    readonly rect: RemoteDesktopSize & { readonly x: number; readonly y: number };
    readonly rgba: Uint8Array<ArrayBuffer> | Uint8ClampedArray<ArrayBuffer>;
  }
  | { readonly kind: "waiting" }
  | { readonly kind: "failed" | "disconnected"; readonly message: string }
);

type Input =
  | { readonly kind: "key"; readonly down: boolean; readonly code: string; readonly key: string; readonly location: number; readonly repeat: boolean }
  | { readonly kind: "pointer"; readonly x: number; readonly y: number; readonly buttons: number }
  /** IME 조합이나 insertText로 확정된 글자 하나(코드 포인트)다. 누름과 뗌을 함께 보낸다. */
  | { readonly kind: "text"; readonly text: string }
  | { readonly kind: "release-input" };

export type RemoteDesktopInput = Sequenced & Input;

export interface RemoteDesktopSession extends RemoteDesktopIdentity {
  // 세션별로 객체를 유지하고, 재연결에서는 새 세대를 사용한다.
  // 화면 순번은 양의 안전한 정수다. 패치는 직전 화면 이벤트에 이어지고 전체 프레임은 건너뛸 수 있다.
  // 프레임에 대한 true는 Canvas 적용 성공을 뜻한다.
  subscribe(receive: (event: RemoteDesktopEvent) => boolean): () => void;
  // 어댑터는 입력을 순번대로 처리하고 폐기한 세대의 대기열과 눌린 상태를 지운다.
  // release-input은 포커스를 잃었을 때 이미 전달한 누름 상태만 해제한다.
  send(input: RemoteDesktopInput): void;
  setActive(active: boolean): void;
  requestClipboard?(request: RemoteDesktopIdentity & { readonly action: "copy" | "paste" }): void;
}

export interface RemoteDesktopViewSession {
  readonly size: RemoteDesktopSize | undefined;
  send(input: Input): void;
  clipboard(action: "copy" | "paste"): void;
  dispose(): void;
}

export function createRemoteDesktopViewSession(
  session: RemoteDesktopSession,
  canvas: HTMLCanvasElement,
  report: (status: RemoteDesktopStatus) => void,
): RemoteDesktopViewSession {
  const identity = { sessionId: session.sessionId, generation: session.generation };
  let closed = false;
  let frameSequence = 0;
  let inputSequence = 0;
  let size: RemoteDesktopSize | undefined;
  const clear = () => {
    size = undefined;
    canvas.width = 0;
    canvas.height = 0;
  };
  clear();
  const receive = (event: RemoteDesktopEvent) => {
    if (closed || event.sessionId !== identity.sessionId || event.generation !== identity.generation ||
      !Number.isSafeInteger(event.sequence) || event.sequence <= frameSequence) return false;

    if (event.kind === "waiting") {
      clear();
      report({ kind: "waiting" });
      return true;
    }
    if (event.kind === "failed" || event.kind === "disconnected") {
      closed = true;
      clear();
      report({ kind: event.kind, message: event.message });
      return true;
    }
    if (event.kind !== "full" && event.kind !== "patch") return false;
    if (!validFrame(event)) return false;
    const { width, height } = event.size;
    if (event.kind === "patch" && (!size || size.width !== width || size.height !== height ||
      event.sequence !== frameSequence + 1)) return false;

    try {
      const context = canvas.getContext("2d");
      if (!context) throw new Error("Canvas 2D unavailable");
      const { x, y, width: rectWidth, height: rectHeight } = event.rect;
      const bytes = new Uint8ClampedArray(event.rgba.buffer, event.rgba.byteOffset, event.rgba.byteLength);
      const image = new ImageData(bytes, rectWidth, rectHeight);
      if (!size || size.width !== width || size.height !== height) {
        canvas.width = width;
        canvas.height = height;
      }
      context.putImageData(image, x, y);
    } catch {
      closed = true;
      clear();
      report({ kind: "failed", message: "원격 화면을 그릴 수 없습니다" });
      return false;
    }

    frameSequence = event.sequence;
    if (!size || size.width !== width || size.height !== height) {
      size = { width, height };
      report({ kind: "connected", size });
    }
    return true;
  };
  report({ kind: "waiting" });
  const unsubscribe = session.subscribe(receive);

  return {
    get size() { return size; },
    send(input: Input) {
      if (size) session.send({ ...input, ...identity, sequence: ++inputSequence });
    },
    clipboard(action: "copy" | "paste") {
      if (size) session.requestClipboard?.({ ...identity, action });
    },
    dispose() {
      closed = true;
      clear();
      unsubscribe();
    },
  };
}

export type RemoteDesktopStatus =
  | { readonly kind: "waiting" }
  | { readonly kind: "connected"; readonly size: RemoteDesktopSize }
  | { readonly kind: "failed" | "disconnected"; readonly message: string };

const MAX_DIMENSION = 8192;
const MAX_PIXELS = 16_777_216;

function validFrame(event: Extract<RemoteDesktopEvent, { kind: "full" | "patch" }>): boolean {
  if (!event.size || !event.rect) return false;
  const { width, height } = event.size;
  const { x, y, width: rectWidth, height: rectHeight } = event.rect;
  return Number.isInteger(width) && width > 0 && width <= MAX_DIMENSION &&
    Number.isInteger(height) && height > 0 && height <= MAX_DIMENSION && width * height <= MAX_PIXELS &&
    Number.isInteger(x) && x >= 0 && Number.isInteger(y) && y >= 0 &&
    Number.isInteger(rectWidth) && rectWidth > 0 && rectWidth <= width - x &&
    Number.isInteger(rectHeight) && rectHeight > 0 && rectHeight <= height - y &&
    (event.kind !== "full" || (x === 0 && y === 0 && rectWidth === width && rectHeight === height)) &&
    (event.rgba instanceof Uint8Array || event.rgba instanceof Uint8ClampedArray) &&
    event.rgba.buffer instanceof ArrayBuffer && event.rgba.byteLength === rectWidth * rectHeight * 4;
}
