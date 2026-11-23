import { useLayoutEffect, useRef, useState, type DragEvent } from "react";
import { ArrowLeftToLine, ArrowRightToLine, Columns2, Copy, Merge, X, type LucideIcon } from "lucide-react";
import { IconButton, Menu, ProtocolIcon, type MenuItem, type ProtocolKind } from "../ui/controls";

export const TAB_DRAG_TYPE = "application/x-zzabba-tab";

export type TabView = Readonly<{
  id: number;
  label: string;
  icon: ProtocolKind;
  state: "running" | "busy" | "error" | "idle";
  closeStatus?: "closing" | "unconfirmed";
  locked?: boolean;
  duplicable: boolean;
}>;

type Props = Readonly<{
  tabs: readonly TabView[];
  active: number | null;
  focused: boolean;
  groupIndex: number;
  groupCount: number;
  canAddGroup: boolean;
  reveal?: readonly unknown[];
  onSelect(id: number): void;
  onClose(id: number): void;
  onMove(id: number, index: number): void;
  onSplit(id: number, side: "left" | "right"): void;
  onMoveToNeighbor(id: number, direction: -1 | 1): void;
  onDuplicate(id: number, split: boolean): void;
  onMerge(): void;
  onDragState(id: number | null): void;
}>;

export function dragTabId(event: DragEvent) {
  const value = Number(event.dataTransfer.getData(TAB_DRAG_TYPE));
  return Number.isInteger(value) && value > 0 ? value : undefined;
}

export function TabStrip(props: Props) {
  const { tabs, active, focused } = props;
  const [menu, setMenu] = useState<{ x: number; y: number; tab: TabView; anchor: HTMLElement }>();
  const [dropIndex, setDropIndex] = useState<number>();
  const current = tabs.find(tab => tab.id === active);
  const list = useRef<HTMLElement>(null);

  // 너비가 바뀌거나 선택이 바뀌면 선택한 탭을 목록 안으로 드러낸다.
  useLayoutEffect(() => {
    const nav = list.current;
    const button = nav?.querySelector<HTMLButtonElement>('.workspace-tab-select[aria-pressed="true"]');
    if (!nav || !button) return;
    const show = () => {
      const bounds = nav.getBoundingClientRect();
      const tab = button.parentElement!.getBoundingClientRect();
      const selected = tab.width <= bounds.width ? tab : button.getBoundingClientRect();
      if (selected.left < bounds.left) nav.scrollLeft += selected.left - bounds.left;
      else if (selected.right > bounds.right) nav.scrollLeft += selected.right - bounds.right;
    };
    show();
    const observer = new ResizeObserver(show);
    observer.observe(nav);
    return () => observer.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, tabs.length, ...(props.reveal ?? [])]);
  const canSplitActive = !!current && !current.locked && props.canAddGroup && (tabs.length > 1 || current.duplicable);

  function indexAt(event: DragEvent<HTMLElement>) {
    const items = [...event.currentTarget.querySelectorAll<HTMLElement>(".workspace-tab")];
    const index = items.findIndex(item => {
      const rect = item.getBoundingClientRect();
      return event.clientX < rect.left + rect.width / 2;
    });
    return index < 0 ? items.length : index;
  }

  function menuItems(tab: TabView): MenuItem[] {
    const items: MenuItem[] = [];
    const item = (label: string, icon: LucideIcon, onSelect: () => void, disabled = false) =>
      items.push({ label, icon, onSelect, disabled });
    item("오른쪽으로 분리", Columns2, () => props.onSplit(tab.id, "right"), !props.canAddGroup || tabs.length < 2 || !!tab.locked);
    if (props.groupIndex > 0) item("왼쪽 그룹으로 이동", ArrowLeftToLine, () => props.onMoveToNeighbor(tab.id, -1), !!tab.locked);
    if (props.groupIndex < props.groupCount - 1) item("오른쪽 그룹으로 이동", ArrowRightToLine, () => props.onMoveToNeighbor(tab.id, 1), !!tab.locked);
    if (tab.duplicable) {
      items.push({ kind: "separator" });
      item("같은 연결 새 탭", Copy, () => props.onDuplicate(tab.id, false));
      item("같은 연결 오른쪽에 열기", Columns2, () => props.onDuplicate(tab.id, true), !props.canAddGroup);
    }
    if (props.groupCount > 1) {
      items.push({ kind: "separator" });
      item("모든 그룹 합치기", Merge, props.onMerge);
    }
    items.push({ kind: "separator" });
    item(tab.closeStatus === "unconfirmed" ? "종료 다시 확인" : "닫기", X, () => props.onClose(tab.id), tab.closeStatus === "closing" || !!tab.locked);
    return items;
  }

  return <div className="workspace-tabstrip" data-focused={focused || undefined}>
    <nav ref={list} className="workspace-tabs" aria-label={props.groupCount > 1 ? `작업 탭 그룹 ${props.groupIndex + 1}` : "작업 탭"}
      onDragOver={event => {
        if (!event.dataTransfer.types.includes(TAB_DRAG_TYPE)) return;
        event.preventDefault();
        event.dataTransfer.dropEffect = "move";
        setDropIndex(indexAt(event));
      }}
      onDragLeave={event => {
        if (!event.currentTarget.contains(event.relatedTarget as Node)) setDropIndex(undefined);
      }}
      onDrop={event => {
        const id = dragTabId(event);
        setDropIndex(undefined);
        if (id === undefined) return;
        event.preventDefault();
        props.onMove(id, indexAt(event));
      }}>
      {tabs.map((tab, index) => {
        const selected = tab.id === active;
        return <div key={tab.id} className="workspace-tab" data-active={selected || undefined}
          data-drop={dropIndex === index || undefined} draggable={!tab.locked}
          onDragStart={event => {
            event.dataTransfer.setData(TAB_DRAG_TYPE, String(tab.id));
            event.dataTransfer.effectAllowed = "move";
            props.onDragState(tab.id);
          }}
          onDragEnd={() => {
            setDropIndex(undefined);
            props.onDragState(null);
          }}
          onAuxClick={event => {
            if (event.button !== 1 || tab.locked || tab.closeStatus === "closing") return;
            event.preventDefault();
            props.onClose(tab.id);
          }}
          onContextMenu={event => {
            event.preventDefault();
            setMenu({ x: event.clientX, y: event.clientY, tab, anchor: event.currentTarget.querySelector("button")! });
          }}>
          <button type="button" className="workspace-tab-select"
            aria-pressed={selected} disabled={tab.locked} title={tab.label} onClick={() => props.onSelect(tab.id)}
            onKeyDown={event => {
              if (event.key !== "ContextMenu" && !(event.key === "F10" && event.shiftKey)) return;
              event.preventDefault();
              const rect = event.currentTarget.getBoundingClientRect();
              setMenu({ x: rect.left, y: rect.bottom, tab, anchor: event.currentTarget });
            }}>
            <ProtocolIcon kind={tab.icon} />
            <span className="workspace-tab-label">{tab.label}</span>
            {tab.state !== "idle" && tab.state !== "running" && <span className="status-dot" data-state={tab.state} aria-hidden="true" />}
          </button>
          <IconButton small icon={X} className="workspace-tab-close"
            label={`${tab.label} ${tab.closeStatus === "unconfirmed" ? "종료 다시 확인" : "닫기"}`}
            disabled={tab.closeStatus === "closing" || tab.locked} onClick={() => props.onClose(tab.id)} />
          {tab.closeStatus && <span className="workspace-tab-close-status" role={tab.closeStatus === "closing" ? "status" : "alert"}>
            {tab.closeStatus === "closing" ? "종료 중" : "종료 확인 불가"}
          </span>}
        </div>;
      })}
      <div className="workspace-tabs-tail" data-drop={dropIndex === tabs.length || undefined} />
    </nav>
    <div className="workspace-tabstrip-actions">
      <IconButton icon={Columns2} label="오른쪽으로 분할" disabled={!canSplitActive}
        onClick={() => {
          if (!current) return;
          if (tabs.length > 1) props.onSplit(current.id, "right");
          else props.onDuplicate(current.id, true);
        }} />
    </div>
    {menu && <Menu x={menu.x} y={menu.y} label={`${menu.tab.label} 탭 메뉴`} items={menuItems(menu.tab)}
      restoreFocus={menu.anchor} onClose={() => setMenu(undefined)} />}
  </div>;
}
