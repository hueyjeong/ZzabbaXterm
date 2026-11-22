const rows = [
  ["Escape", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8", "Digit9", "Digit0", "Minus", "Equal", "Backspace", "Tab"],
  ["KeyQ", "KeyW", "KeyE", "KeyR", "KeyT", "KeyY", "KeyU", "KeyI", "KeyO", "KeyP", "BracketLeft", "BracketRight", "Enter", "ControlLeft"],
  ["KeyA", "KeyS", "KeyD", "KeyF", "KeyG", "KeyH", "KeyJ", "KeyK", "KeyL", "Semicolon", "Quote", "Backquote", "ShiftLeft", "Backslash"],
  ["KeyZ", "KeyX", "KeyC", "KeyV", "KeyB", "KeyN", "KeyM", "Comma", "Period", "Slash", "ShiftRight", "NumpadMultiply", "AltLeft", "Space", "CapsLock"],
  ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "NumLock", "ScrollLock", "Numpad7", "Numpad8", "Numpad9", "NumpadSubtract", "Numpad4", "Numpad5", "Numpad6", "NumpadAdd", "Numpad1", "Numpad2", "Numpad3", "Numpad0", "NumpadDecimal"],
];
const scan = new Map<string, number>();
let next = 1;
for (const row of rows) for (const code of row) scan.set(code, next++);
for (const [code, value] of Object.entries({
  IntlBackslash: 0x56, F11: 0x57, F12: 0x58,
  NumpadEnter: 0x9c, ControlRight: 0x9d, NumpadDivide: 0xb5, AltRight: 0xb8,
  Home: 0xc7, ArrowUp: 0xc8, PageUp: 0xc9, ArrowLeft: 0xcb, ArrowRight: 0xcd, End: 0xcf,
  ArrowDown: 0xd0, PageDown: 0xd1, Insert: 0xd2, Delete: 0xd3, MetaLeft: 0xdb, MetaRight: 0xdc, ContextMenu: 0xdd,
})) scan.set(code, value);

export function scanCode(code: string) {
  const value = scan.get(code);
  if (value !== undefined) return { code: value & 0x7f, extended: value >= 0x80 };
}
