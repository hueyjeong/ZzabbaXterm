const FIXED: Readonly<Record<string, number>> = {
  Backspace: 0xff08, Tab: 0xff09, Enter: 0xff0d, Escape: 0xff1b,
  Home: 0xff50, ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53,
  ArrowDown: 0xff54, PageUp: 0xff55, PageDown: 0xff56, End: 0xff57,
  Insert: 0xff63, Delete: 0xffff, Pause: 0xff13, ScrollLock: 0xff14,
  PrintScreen: 0xff61, ContextMenu: 0xff67, NumLock: 0xff7f, CapsLock: 0xffe5,
  AltGraph: 0xfe03, Shift: 0xffe1, Control: 0xffe3, Alt: 0xffe9, Meta: 0xffeb,
};

export function keysym(key: string, location: number, code: string): number | undefined {
  if (location === 3 && code.startsWith("Numpad")) {
    if (/^Numpad[0-9]$/.test(code) && /^[0-9]$/.test(key)) return 0xffb0 + Number(key);
    if (key === "Enter") return 0xff8d;
    const keypad = { "*": 0xffaa, "+": 0xffab, "-": 0xffad, ".": 0xffae, "/": 0xffaf };
    if (Object.prototype.hasOwnProperty.call(keypad, key)) return keypad[key as keyof typeof keypad];
  }
  if (Object.prototype.hasOwnProperty.call(FIXED, key)) {
    return FIXED[key] + (location === 2 && ["Shift", "Control", "Alt", "Meta"].includes(key) ? 1 : 0);
  }
  if (/^F(?:[1-9]|1[0-9]|2[0-4])$/.test(key)) return 0xffbd + Number(key.slice(1));
  const point = key.codePointAt(0);
  if (point === undefined || key.length !== (point > 0xffff ? 2 : 1) || point < 0x20 ||
    (point >= 0x7f && point < 0xa0) || (point >= 0xd800 && point <= 0xdfff)) return;
  return point <= 0xff ? point : 0x01000000 + point;
}
