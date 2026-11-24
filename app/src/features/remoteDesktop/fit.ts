import type { RemoteDesktopSize } from "./session";

/** 원격 데스크톱에 요청할 장치 픽셀 크기와 배율(%)이다. */
export interface DesktopFit extends RemoteDesktopSize {
  readonly scale: number;
}

const MIN_SIDE = 200;
const MAX_SIDE = 4096;
const MAX_PIXELS = 3840 * 2160;

/**
 * 화면 영역(CSS 픽셀)을 장치 픽셀로 바꿔 원격 화면이 1:1로 그려지게 한다.
 * Display Control 범위(200 이상, 짝수 폭)와 Rust 프레임 한도(한 변 4096, 3840×2160 화소)에 맞춘다.
 */
export function fitSize(view: RemoteDesktopSize, devicePixelRatio: number): DesktopFit | undefined {
  if (!(view.width > 0 && view.height > 0)) return;
  const ratio = Number.isFinite(devicePixelRatio) && devicePixelRatio > 0 ? devicePixelRatio : 1;
  let width = Math.floor(view.width * ratio);
  let height = Math.floor(view.height * ratio);
  const shrink = Math.min(1, MAX_SIDE / width, MAX_SIDE / height, Math.sqrt(MAX_PIXELS / (width * height)));
  width = Math.floor(width * shrink);
  height = Math.floor(height * shrink);
  return { width: Math.max(MIN_SIDE, width - (width % 2)), height: Math.max(MIN_SIDE, height), scale: Math.round(ratio * 100) };
}

/** 창 크기 변경이 멈춘 뒤 마지막 값으로 한 번만 적용한다. */
export function createFitScheduler(delay: number, apply: () => void) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return {
    schedule() {
      clearTimeout(timer);
      timer = setTimeout(() => {
        timer = undefined;
        apply();
      }, delay);
    },
    cancel() {
      clearTimeout(timer);
      timer = undefined;
    },
  };
}
