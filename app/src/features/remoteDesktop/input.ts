export type Modifier = "shift" | "ctrl" | "alt" | "meta";

export const MODIFIER_KEYS: Record<Modifier, { code: string; key: string; location: number }> = {
  shift: { code: "ShiftLeft", key: "Shift", location: 1 },
  ctrl: { code: "ControlLeft", key: "Control", location: 1 },
  alt: { code: "AltLeft", key: "Alt", location: 1 },
  meta: { code: "MetaLeft", key: "Meta", location: 1 },
};

const PHYSICAL: Record<string, Modifier> = {
  ShiftLeft: "shift", ShiftRight: "shift",
  ControlLeft: "ctrl", ControlRight: "ctrl",
  AltLeft: "alt", AltRight: "alt",
  MetaLeft: "meta", MetaRight: "meta",
};

export type ModifierState = Record<Modifier, boolean>;
export interface ModifierInput {
  readonly code: string;
  readonly down: boolean;
  readonly shiftKey: boolean;
  readonly ctrlKey: boolean;
  readonly altKey: boolean;
  readonly metaKey: boolean;
}
export interface ModifierEvent {
  readonly code: string;
  readonly key: string;
  readonly location: number;
  readonly down: boolean;
}

/// 자동화·IME는 Shift keydown 없이 shiftKey=true만 보낼 수 있으므로,
/// 실제 키 이벤트 전에 서버의 수식 키 상태를 맞추는 이벤트를 만든다.
export function modifierEvents(state: ModifierState, input: ModifierInput): { state: ModifierState; events: ModifierEvent[] } {
  const desired: ModifierState = {
    shift: input.shiftKey, ctrl: input.ctrlKey, alt: input.altKey, meta: input.metaKey,
  };
  const physical = PHYSICAL[input.code];
  const next = { ...state };
  const events: ModifierEvent[] = [];
  for (const name of Object.keys(MODIFIER_KEYS) as Modifier[]) {
    if (name === physical || next[name] === desired[name]) continue;
    next[name] = desired[name];
    events.push({ ...MODIFIER_KEYS[name], down: desired[name] });
  }
  if (physical) next[physical] = input.down;
  return { state: next, events };
}

/// IME 조합이나 insertText로 확정된 문자열을 원격에 보낼 글자(코드 포인트) 단위로 나눈다.
/// BMP 밖 글자는 서러게이트 쌍째로 한 글자다. 제어 문자와 짝 없는 서러게이트는 버린다.
export function textCharacters(text: string): string[] {
  return Array.from(text).filter((character) => {
    const point = character.codePointAt(0)!;
    return !(point < 0x20 || (point >= 0x7f && point < 0xa0) || (point >= 0xd800 && point <= 0xdfff));
  });
}

/// IME가 처리하는 keydown이다. 이 키를 스캔코드로도 보내면 조합 결과와 함께 두 번 입력된다.
export function imeKeyDown(event: { readonly isComposing: boolean; readonly key: string; readonly keyCode: number }): boolean {
  return event.isComposing || event.key === "Process" || event.keyCode === 229;
}

export function releasedModifiers(state: ModifierState): { state: ModifierState; events: ModifierEvent[] } {
  const events = (Object.keys(MODIFIER_KEYS) as Modifier[])
    .filter((name) => state[name])
    .map((name) => ({ ...MODIFIER_KEYS[name], down: false }));
  return { state: { shift: false, ctrl: false, alt: false, meta: false }, events };
}
