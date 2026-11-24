import { useRef, useState, type KeyboardEvent, type MouseEvent } from "react";
import { Menu, type MenuItem } from "./controls";

type Opened = Readonly<{ key: number; x: number; y: number; items: readonly MenuItem[]; label: string; restore: HTMLElement | null }>;

/** 메뉴 키 또는 Shift+F10이다. */
export function isMenuKey(event: { key: string; shiftKey: boolean }) {
  return event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey);
}

/**
 * 우클릭한 위치나 키보드 포커스 요소 아래에 메뉴를 연다.
 * Shift 우클릭과 메뉴 키가 아닌 키 입력은 건드리지 않고 false를 돌려준다.
 * 메뉴를 닫으면 메뉴를 열기 전에 포커스가 있던 요소로 돌아간다.
 */
export function useContextMenu() {
  const [menu, setMenu] = useState<Opened>();
  const sequence = useRef(0);

  function open(event: MouseEvent<HTMLElement> | KeyboardEvent<HTMLElement>, items: readonly MenuItem[], label: string): boolean {
    const keyboard = "key" in event;
    if (keyboard ? !isMenuKey(event) : event.shiftKey) return false;
    event.preventDefault();
    event.stopPropagation();
    if (items.length === 0) return false;
    let x: number;
    let y: number;
    let restore: HTMLElement | null;
    if (keyboard) {
      const element = event.target instanceof HTMLElement ? event.target : event.currentTarget;
      const box = element.getBoundingClientRect();
      x = box.left + 24;
      y = box.bottom;
      restore = element;
    } else {
      const active = event.currentTarget.ownerDocument.activeElement;
      x = event.clientX;
      y = event.clientY;
      restore = active instanceof HTMLElement && active !== active.ownerDocument.body ? active : null;
    }
    setMenu({ key: ++sequence.current, x, y, items, label, restore });
    return true;
  }

  const element = menu && <Menu key={menu.key} x={menu.x} y={menu.y} items={menu.items} label={menu.label}
    restoreFocus={menu.restore} onClose={() => setMenu(undefined)} />;
  return { open, element };
}
