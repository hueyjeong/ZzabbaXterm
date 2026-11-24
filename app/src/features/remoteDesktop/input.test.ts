import { expect, it } from "vitest";
import { imeKeyDown, modifierEvents, releasedModifiers, textCharacters, type ModifierState } from "./input";

const none: ModifierState = { shift: false, ctrl: false, alt: false, meta: false };

it("Shift keydown 없이 온 shiftKey=true를 먼저 눌러 보낸다", () => {
  const first = modifierEvents(none, { code: "KeyX", down: true, shiftKey: true, ctrlKey: false, altKey: false, metaKey: false });
  expect(first.events).toEqual([{ code: "ShiftLeft", key: "Shift", location: 1, down: true }]);
  expect(first.state.shift).toBe(true);
  const release = modifierEvents(first.state, { code: "KeyX", down: false, shiftKey: true, ctrlKey: false, altKey: false, metaKey: false });
  expect(release.events).toEqual([]);
});

it("잃어버린 Shift 해제를 다음 키에서 복구한다", () => {
  const state = { ...none, shift: true };
  const next = modifierEvents(state, { code: "KeyA", down: true, shiftKey: false, ctrlKey: false, altKey: false, metaKey: false });
  expect(next.events).toEqual([{ code: "ShiftLeft", key: "Shift", location: 1, down: false }]);
  expect(next.state.shift).toBe(false);
});

it("실제 Shift 키 이벤트는 수식 키 동기화와 중복되지 않는다", () => {
  const down = modifierEvents(none, { code: "ShiftLeft", down: true, shiftKey: true, ctrlKey: false, altKey: false, metaKey: false });
  expect(down.events).toEqual([]);
  expect(down.state.shift).toBe(true);
  const up = modifierEvents(down.state, { code: "ShiftLeft", down: false, shiftKey: false, ctrlKey: false, altKey: false, metaKey: false });
  expect(up.events).toEqual([]);
  expect(up.state.shift).toBe(false);
});

it("확정 문자열을 코드 포인트로 나누고 제어 문자와 짝 없는 서러게이트를 버린다", () => {
  expect(textCharacters("한글 테스트 123")).toEqual(["한", "글", " ", "테", "스", "트", " ", "1", "2", "3"]);
  expect(textCharacters("a😀b")).toEqual(["a", "😀", "b"]);
  expect(textCharacters("\n\t\u007f\u0085\ud83d가")).toEqual(["가"]);
});

it("IME가 처리하는 keydown만 스캔코드 전송에서 뺀다", () => {
  expect(imeKeyDown({ isComposing: false, key: "Process", keyCode: 229 })).toBe(true);
  expect(imeKeyDown({ isComposing: true, key: "ㅏ", keyCode: 75 })).toBe(true);
  expect(imeKeyDown({ isComposing: false, key: "Unidentified", keyCode: 229 })).toBe(true);
  expect(imeKeyDown({ isComposing: false, key: "a", keyCode: 65 })).toBe(false);
  expect(imeKeyDown({ isComposing: false, key: "Control", keyCode: 17 })).toBe(false);
});

it("blur 시 눌린 모든 수식 키를 해제한다", () => {
  const state = { shift: true, ctrl: true, alt: false, meta: false };
  const released = releasedModifiers(state);
  expect(released.events).toEqual([
    { code: "ShiftLeft", key: "Shift", location: 1, down: false },
    { code: "ControlLeft", key: "Control", location: 1, down: false },
  ]);
  expect(released.state).toEqual(none);
});
