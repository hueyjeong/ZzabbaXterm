import { useCallback, useEffect, useLayoutEffect, useRef, useState, type RefObject } from "react";
import { Menu, type MenuItem } from "../ui/controls";

/** 단추에 붙은 메뉴의 열림 상태다. 열린 메뉴의 단추를 다시 누르면 닫힌 채로 둔다. */
export function useAnchoredMenu<T extends HTMLElement>() {
  const anchor = useRef<T>(null);
  const [open, setOpenState] = useState(false);
  const openRef = useRef(false);
  const suppress = useRef(false);
  const setOpen = useCallback((value: boolean) => {
    openRef.current = value;
    setOpenState(value);
  }, []);
  const close = useCallback(() => setOpen(false), [setOpen]);

  useEffect(() => {
    // Menu의 바깥 클릭 감시보다 먼저 등록해 같은 단추를 누른 닫기 동작을 구분한다.
    const down = (event: PointerEvent) => {
      suppress.current = openRef.current && !!anchor.current?.contains(event.target as Node);
    };
    window.addEventListener("pointerdown", down, true);
    return () => window.removeEventListener("pointerdown", down, true);
  }, []);

  const toggle = useCallback(() => {
    if (suppress.current) {
      suppress.current = false;
      return;
    }
    setOpen(!openRef.current);
  }, [setOpen]);

  return { anchor, open, close, toggle, show: useCallback(() => setOpen(true), [setOpen]) };
}

function clamp(value: number, min: number, max: number) {
  return Math.max(min, Math.min(value, Math.max(min, max)));
}

/** 기준 단추의 위나 아래에 여는 메뉴다. 위치는 연 순간에 고정한다. */
export function AnchoredMenu({ anchor, placement = "below", align = "start", items, label, onClose }: {
  anchor: RefObject<HTMLElement | null>;
  placement?: "above" | "below";
  align?: "start" | "end";
  items: readonly MenuItem[];
  label: string;
  onClose: () => void;
}) {
  const host = useRef<HTMLSpanElement>(null);
  const [origin] = useState(() => {
    const box = anchor.current?.getBoundingClientRect();
    return box ? { x: box.left, y: placement === "below" ? box.bottom + 4 : box.top } : { x: 0, y: 0 };
  });

  useLayoutEffect(() => {
    const menu = host.current?.querySelector<HTMLElement>(".ui-menu");
    const box = anchor.current?.getBoundingClientRect();
    if (!menu || !box) return;
    const { width, height } = menu.getBoundingClientRect();
    const left = align === "end" ? box.right - width : box.left;
    const above = box.top - height - 4;
    const below = box.bottom + 4;
    const top = placement === "above"
      ? (above >= 4 ? above : below)
      : (below + height <= window.innerHeight - 4 ? below : above);
    menu.style.left = `${clamp(left, 4, window.innerWidth - width - 4)}px`;
    menu.style.top = `${clamp(top, 4, window.innerHeight - height - 4)}px`;
  });

  return <span ref={host} className="a01-menu-host">
    <Menu x={origin.x} y={origin.y} items={items} label={label} onClose={onClose} restoreFocus={anchor.current} />
  </span>;
}
