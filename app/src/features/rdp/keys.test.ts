import { expect, it } from "vitest";
import { scanCode } from "./keys";

it("물리 키 코드를 PC/AT set 1 스캔코드로 매핑한다", () => {
  expect(scanCode("KeyX")).toEqual({ code: 0x2d, extended: false });
  expect(scanCode("KeyR")).toEqual({ code: 0x13, extended: false });
  expect(scanCode("KeyD")).toEqual({ code: 0x20, extended: false });
  expect(scanCode("KeyP")).toEqual({ code: 0x19, extended: false });
  expect(scanCode("ShiftLeft")).toEqual({ code: 0x2a, extended: false });
  expect(scanCode("Minus")).toEqual({ code: 0x0c, extended: false });
  expect(scanCode("Enter")).toEqual({ code: 0x1c, extended: false });
  expect(scanCode("Space")).toEqual({ code: 0x39, extended: false });
  expect(scanCode("ArrowLeft")).toEqual({ code: 0x4b, extended: true });
  expect(scanCode("MetaLeft")).toEqual({ code: 0x5b, extended: true });
});
