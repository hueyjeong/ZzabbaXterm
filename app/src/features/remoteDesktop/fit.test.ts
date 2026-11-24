import { afterEach, expect, it, vi } from "vitest";
import { createFitScheduler, fitSize } from "./fit";

afterEach(() => { vi.useRealTimers(); });

it("화면 영역을 장치 픽셀로 바꾸고 폭은 짝수로 맞춘다", () => {
  expect(fitSize({ width: 645.6, height: 470.2 }, 1.25)).toEqual({ width: 806, height: 587, scale: 125 });
  expect(fitSize({ width: 1280, height: 720 }, 1)).toEqual({ width: 1280, height: 720, scale: 100 });
  expect(fitSize({ width: 641, height: 400 }, 2)).toEqual({ width: 1282, height: 800, scale: 200 });
});

it("Display Control 최솟값과 프레임 한도 안으로 줄인다", () => {
  expect(fitSize({ width: 120, height: 90 }, 1)).toEqual({ width: 200, height: 200, scale: 100 });
  const large = fitSize({ width: 2560, height: 1440 }, 2)!;
  expect(large.width).toBeLessThanOrEqual(4096);
  expect(large.width * large.height).toBeLessThanOrEqual(3840 * 2160);
  expect(large.width % 2).toBe(0);
  expect(fitSize({ width: 0, height: 500 }, 1)).toBeUndefined();
  expect(fitSize({ width: 800, height: 600 }, Number.NaN)).toEqual({ width: 800, height: 600, scale: 100 });
});

it("크기 변경이 멈춘 뒤 한 번만 적용하고 취소하면 적용하지 않는다", () => {
  vi.useFakeTimers();
  const apply = vi.fn();
  const fit = createFitScheduler(400, apply);
  fit.schedule();
  vi.advanceTimersByTime(300);
  fit.schedule();
  vi.advanceTimersByTime(300);
  expect(apply).not.toHaveBeenCalled();
  vi.advanceTimersByTime(100);
  expect(apply).toHaveBeenCalledTimes(1);
  fit.schedule();
  fit.cancel();
  vi.advanceTimersByTime(1000);
  expect(apply).toHaveBeenCalledTimes(1);
});
