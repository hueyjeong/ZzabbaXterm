import { useLayoutEffect, useRef, type ButtonHTMLAttributes, type KeyboardEvent, type ReactNode, type Ref } from "react";
import {
  AppWindow, Cloud, Database, FileCode2, Monitor, MonitorSmartphone, Server, ShieldAlert, SquareTerminal, Waypoints,
  type LucideIcon,
} from "lucide-react";
import "./tokens.css";

export type ProtocolKind = "local" | "ssh" | "ssm" | "s3" | "rdp" | "vnc" | "tunnel" | "editor" | "sudo";

const PROTOCOL_ICONS: Record<ProtocolKind, LucideIcon> = {
  local: SquareTerminal,
  ssh: Server,
  ssm: Cloud,
  s3: Database,
  rdp: Monitor,
  vnc: MonitorSmartphone,
  tunnel: Waypoints,
  editor: FileCode2,
  sudo: ShieldAlert,
};

export function ProtocolIcon({ kind }: { kind: ProtocolKind }) {
  const Icon = PROTOCOL_ICONS[kind] ?? AppWindow;
  return <Icon className="proto-icon" data-kind={kind} aria-hidden="true" />;
}

/** 아이콘만 보이는 단추다. label은 접근성 이름과 툴팁으로 함께 쓴다. */
export function IconButton({ icon: Icon, label, className = "", small, danger, children, ref, ...rest }:
  Omit<ButtonHTMLAttributes<HTMLButtonElement>, "aria-label" | "title"> & {
    ref?: Ref<HTMLButtonElement>;
    icon: LucideIcon;
    label: string;
    small?: boolean;
    danger?: boolean;
    children?: ReactNode;
  }) {
  return <button type="button" {...rest} ref={ref} aria-label={label} title={label}
    className={`icon-button${small ? " small" : ""}${danger ? " danger" : ""}${className ? ` ${className}` : ""}`}>
    <Icon aria-hidden="true" />{children}
  </button>;
}

export type MenuItem =
  | { kind?: "item"; label: string; icon?: LucideIcon; shortcut?: string; disabled?: boolean; onSelect: () => void }
  | { kind: "separator" }
  | { kind: "label"; label: string };

/** 화면 좌표에 여는 메뉴다. 바깥 클릭·Escape·포커스 이탈로 닫고 화살표로 항목을 이동한다. */
export function Menu({ x, y, items, label, onClose, restoreFocus }: {
  x: number;
  y: number;
  items: readonly MenuItem[];
  label: string;
  onClose: () => void;
  restoreFocus?: HTMLElement | null;
}) {
  const element = useRef<HTMLDivElement>(null);
  const closeRef = useRef(onClose);
  useLayoutEffect(() => { closeRef.current = onClose; });
  useLayoutEffect(() => {
    const onClose = () => closeRef.current();
    const menu = element.current!;
    const { width, height } = menu.getBoundingClientRect();
    menu.style.left = `${Math.max(4, Math.min(x, window.innerWidth - width - 4))}px`;
    menu.style.top = `${Math.max(4, Math.min(y, window.innerHeight - height - 4))}px`;
    menu.querySelector<HTMLButtonElement>('[role="menuitem"]:not(:disabled)')?.focus();
    const outside = (event: PointerEvent) => {
      if (!menu.contains(event.target as Node)) onClose();
    };
    window.addEventListener("pointerdown", outside, true);
    window.addEventListener("blur", onClose);
    return () => {
      window.removeEventListener("pointerdown", outside, true);
      window.removeEventListener("blur", onClose);
    };
  }, [x, y]);

  function close() {
    onClose();
    restoreFocus?.focus();
  }

  function navigate(event: KeyboardEvent<HTMLDivElement>) {
    const enabled = [...event.currentTarget.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not(:disabled)')];
    const index = enabled.indexOf(document.activeElement as HTMLButtonElement);
    let next: HTMLButtonElement | undefined;
    switch (event.key) {
      case "ArrowDown": next = enabled[(index + 1) % enabled.length]; break;
      case "ArrowUp": next = enabled[(index - 1 + enabled.length) % enabled.length]; break;
      case "Home": next = enabled[0]; break;
      case "End": next = enabled[enabled.length - 1]; break;
      case "Escape": case "Tab": event.preventDefault(); close(); return;
      default: return;
    }
    event.preventDefault();
    next?.focus();
  }

  return <div ref={element} className="ui-menu" role="menu" aria-label={label} style={{ left: x, top: y }}
    onKeyDown={navigate} onContextMenu={event => event.preventDefault()}>
    {items.map((item, index) => {
      if (item.kind === "separator") return <div key={index} role="separator" />;
      if (item.kind === "label") return <div key={index} className="ui-menu-label">{item.label}</div>;
      const Icon = item.icon;
      return <button key={index} type="button" role="menuitem" disabled={item.disabled} onClick={() => {
        onClose();
        item.onSelect();
        // 동작이 포커스를 옮기지 않았으면 메뉴를 연 요소로 돌려보낸다.
        queueMicrotask(() => {
          const focused = document.activeElement;
          if ((!focused || focused === document.body) && restoreFocus?.isConnected) restoreFocus.focus();
        });
      }}>
        {Icon ? <Icon aria-hidden="true" /> : <span style={{ width: 15 }} />}
        <span>{item.label}</span>
        {item.shortcut && <kbd>{item.shortcut}</kbd>}
      </button>;
    })}
  </div>;
}
