import { useCallback, useRef, useState, type KeyboardEvent, type MouseEvent, type ReactNode } from "react";
import { Menu, type MenuItem } from "./controls";

type Opened<S> = Readonly<{
  key: number; x: number; y: number; source: S; label: string; target: HTMLElement; restore: HTMLElement | null;
}>;

/** 메뉴 키 또는 Shift+F10이다. */
export function isMenuKey(event: { key: string; shiftKey: boolean }) {
  return event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey);
}

/**
 * 우클릭한 위치나 키보드 포커스 요소 아래에 메뉴를 연다.
 * `open`은 이벤트를 처리하면 true, Shift 우클릭이나 메뉴 키가 아닌 키 입력처럼 건드리지 않은 이벤트면 false를 돌려준다.
 * 항목은 메뉴가 열려 있는 동안 렌더링마다 `element`의 build로 다시 만든다. 닫힌 동작은 메뉴에서도 바로 빠진다.
 * 메뉴를 닫으면 메뉴를 열기 전에 포커스가 있던 요소로 돌아간다.
 */
export function useContextMenu<S>() {
  const [menu, setMenu] = useState<Opened<S>>();
  const sequence = useRef(0);

  function open(event: MouseEvent<HTMLElement> | KeyboardEvent<HTMLElement>, source: S, label: string): boolean {
    const keyboard = "key" in event;
    if (keyboard ? !isMenuKey(event) : event.shiftKey) return false;
    event.preventDefault();
    event.stopPropagation();
    // 메뉴 안에서 생긴 이벤트(메뉴 키를 뗄 때 오는 contextmenu 등)는 새 메뉴를 열지 않는다.
    if (event.target instanceof Element && event.target.closest('[role="menu"]')) return true;
    const target = event.currentTarget;
    let x: number;
    let y: number;
    let restore: HTMLElement | null;
    if (keyboard) {
      const focused = event.target instanceof HTMLElement ? event.target : target;
      const box = focused.getBoundingClientRect();
      x = box.left + 24;
      y = box.bottom;
      restore = focused;
    } else {
      const active = target.ownerDocument.activeElement;
      x = event.clientX;
      y = event.clientY;
      restore = active instanceof HTMLElement && active !== active.ownerDocument.body ? active : null;
    }
    setMenu({ key: ++sequence.current, x, y, source, label, target, restore });
    return true;
  }

  const close = useCallback(() => setMenu(undefined), []);

  function element(build: (source: S, target: HTMLElement) => readonly MenuItem[]): ReactNode {
    if (!menu) return null;
    const items = build(menu.source, menu.target);
    return items.length > 0 && <Menu key={menu.key} x={menu.x} y={menu.y} items={items} label={menu.label}
      restoreFocus={menu.restore} onClose={close} />;
  }

  return { open, close, element };
}
