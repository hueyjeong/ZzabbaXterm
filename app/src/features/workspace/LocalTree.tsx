import { useEffect, useState, useRef, type CSSProperties, type KeyboardEvent, type MouseEvent } from "react";
import {
  ChevronRight, Copy, Database, Monitor, MonitorSmartphone, Folder, FolderOpen, FolderPlus, KeyRound, Pencil, Plus, ShieldAlert,
  SquarePen, SquareTerminal, Trash2, Waypoints, type LucideIcon,
} from "lucide-react";
import type { SavedConnections } from "../connections/connectionIpc";
import { groupChildren } from "../connections/groupTree";
import { Menu, ProtocolIcon, type MenuItem, type ProtocolKind } from "../ui/controls";

type OpenTarget = (target?: SavedConnections["targets"][number]["profile"], options?: { readonly remoteEditor?: true }) => void;
type ElevateTarget = (target?: SavedConnections["targets"][number]["profile"]) => void;
type MenuTarget = {
  method: string; kind: ProtocolKind; open: () => void; sshTargetId?: string; openTunnel?: () => void; edit?: () => void; remove?: () => void;
  duplicate?: () => void; openRemoteEditor?: () => void; elevate?: () => void; openStorage?: () => void;
};
type TreeTarget = MenuTarget & { key: string; name: string; id: string; viaSsm?: boolean };
// local은 "내 PC"를 담는 합성 그룹이다. 추가·이름 바꾸기·삭제 대상이 아니다.
type TreeGroup = { key: string; name: string; id: string; local?: true; groups: readonly TreeGroup[]; targets: readonly TreeTarget[] };

export function LocalTree({
  profiles, openCounts, onOpen, onElevate, onLocalElevate, onRegisterSshHost, onTrustSshHost, onEdit, onDelete, onDuplicate,
  onAddSession, onAddGroup, onEditGroup, onDeleteGroup, onOpenTunnel, onOpenStorage, focusRequest,
}: {
  profiles?: SavedConnections; openCounts?: ReadonlyMap<string, number>; onOpen: OpenTarget; onElevate?: ElevateTarget; onLocalElevate?: () => void;
  onRegisterSshHost: (targetId: string) => void;
  onTrustSshHost?: (targetId: string) => void;
  onEdit?: (targetId: string) => void;
  onDelete?: (targetId: string) => void;
  onDuplicate?: (targetId: string) => void;
  /** groupId가 없으면 최상위에 추가한다. */
  onAddSession?: (groupId?: string) => void;
  /** parentId가 없으면 최상위에 추가한다. */
  onAddGroup?: (parentId?: string) => void;
  onEditGroup?: (groupId: string) => void;
  onDeleteGroup?: (groupId: string) => void;
  onOpenTunnel: (targetId: string, name: string, method: "ssh" | "ssm") => void;
  onOpenStorage?: (targetId: string, name: string) => void;
  /** 값이 바뀌면 트리의 현재 행으로 포커스를 돌린다. 대화상자를 닫은 뒤 쓴다. */
  focusRequest?: number;
}) {
  const [query, setQuery] = useState("");
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState("local");
  const [menu, setMenu] = useState<{ x: number; y: number; row: HTMLElement | null; label: string; items: MenuItem[] }>();
  const tree = useRef<HTMLUListElement>(null);

  useEffect(() => {
    if (!focusRequest) return;
    // 그 사이 다른 곳으로 옮긴 포커스는 그대로 둔다.
    const active = document.activeElement;
    if (active && active !== document.body && !tree.current?.contains(active)) return;
    tree.current?.querySelector<HTMLElement>('[role="treeitem"][tabindex="0"]')?.focus();
  }, [focusRequest]);

  const term = query.trim().toLocaleLowerCase();
  const matches = (name: string) => name.toLocaleLowerCase().includes(term);
  const item = ({ profile: target }: SavedConnections["targets"][number]): TreeTarget => ({
    key: `target:${target.id}`, name: target.displayName, id: target.id, open: () => onOpen(target),
    kind: (target.defaultMethod === "local" ? "local" : target.defaultMethod) as ProtocolKind,
    viaSsm: (target.defaultMethod === "ssh" && target.ssh.transport.kind === "ssm")
      || (target.defaultMethod === "rdp" && target.rdp.route.kind === "ssm")
      || (target.defaultMethod === "vnc" && target.vnc.route.kind === "ssm"),
    method: target.defaultMethod === "ssm" ? "AWS Systems Manager" : target.defaultMethod === "ssh" ? "SSH"
      : target.defaultMethod === "s3" ? "S3" : target.defaultMethod === "rdp" ? "RDP"
        : target.defaultMethod === "vnc" ? "VNC" : "로컬",
    sshTargetId: target.defaultMethod === "ssh" ? target.id : undefined,
    openRemoteEditor: target.defaultMethod === "ssh" && target.ssh.transport.kind === "direct" &&
      (target.ssh.auth.kind === "agent" || target.ssh.auth.kind === "keyFile")
      ? () => onOpen(target, { remoteEditor: true }) : undefined,
    edit: onEdit ? () => onEdit(target.id) : undefined,
    duplicate: onDuplicate ? () => onDuplicate(target.id) : undefined,
    remove: onDelete ? () => onDelete(target.id) : undefined,
    openTunnel: target.defaultMethod === "ssm" || (target.defaultMethod === "ssh" && target.ssh.transport.kind === "direct")
      ? () => onOpenTunnel(target.id, target.displayName, target.defaultMethod) : undefined,
    openStorage: target.defaultMethod === "ssm" && onOpenStorage ? () => onOpenStorage(target.id, target.displayName) : undefined,
    elevate: onElevate && target.defaultMethod === "ssh" ? () => onElevate(target) : undefined,
  });

  // 그룹은 parentId로 중첩한다. 하위 그룹을 세션보다 먼저 두고, 그룹 없는 세션은 최상위 그룹 뒤에 둔다.
  const targetsByGroup = new Map<string | undefined, TreeTarget[]>();
  for (const entry of profiles?.targets ?? []) {
    targetsByGroup.set(entry.profile.groupId, [...targetsByGroup.get(entry.profile.groupId) ?? [], item(entry)]);
  }
  const children = groupChildren(profiles?.groups ?? []);
  const built = new Set<string>();
  const build = (id: string | undefined): TreeGroup[] => (children.get(id) ?? []).flatMap((group) => {
    if (built.has(group.id)) return [];
    built.add(group.id);
    return [{ key: `group:${group.id}`, name: group.displayName, id: group.id, groups: build(group.id), targets: targetsByGroup.get(group.id) ?? [] }];
  });
  // 검색하면 일치하는 항목과 그 조상 그룹을 남긴다. 이름이 일치하는 그룹은 내용을 모두 보인다.
  const filter = (group: TreeGroup): TreeGroup[] => {
    if (matches(group.name)) return [group];
    const groups = group.groups.flatMap(filter);
    const targets = group.targets.filter((target) => matches(target.name));
    return groups.length || targets.length ? [{ ...group, groups, targets }] : [];
  };
  const groups = [
    { key: "local", name: "로컬", id: "", local: true as const, groups: [],
      targets: [{ key: "pc", name: "내 PC", id: "", kind: "local" as ProtocolKind, method: "로컬", open: () => onOpen(), elevate: onLocalElevate }] },
    ...build(undefined),
  ].flatMap(filter);
  const topLevel = (targetsByGroup.get(undefined) ?? []).filter((target) => matches(target.name));
  const expanded = (key: string) => Boolean(term) || !collapsed.has(key);
  const visible: string[] = [];
  const collect = (group: TreeGroup) => {
    visible.push(group.key);
    if (!expanded(group.key)) return;
    group.groups.forEach(collect);
    visible.push(...group.targets.map((target) => target.key));
  };
  groups.forEach(collect);
  visible.push(...topLevel.map((target) => target.key));
  const tabStop = visible.includes(selected) ? selected : visible[0];

  function toggle(key: string) {
    if (term) return;
    setCollapsed((previous) => {
      const next = new Set(previous);
      if (!next.delete(key)) next.add(key);
      return next;
    });
  }

  /** row는 메뉴를 닫으면 포커스를 돌릴 요소다. 행 메뉴는 그 행에 먼저 포커스를 둔다. */
  function showMenu(row: HTMLElement | null, label: string, items: MenuItem[], x: number, y: number, focusRow = true) {
    if (!items.length) return;
    if (focusRow) row?.focus();
    setMenu({ row, label, items, x, y });
  }

  function showMenuAtRow(row: HTMLElement, label: string, items: MenuItem[]) {
    const rect = row.getBoundingClientRect();
    showMenu(row, label, items, rect.left + 24, rect.bottom);
  }

  function blankMenu(): MenuItem[] {
    const items: MenuItem[] = [];
    if (onAddSession) items.push({ label: "세션 추가…", icon: Plus, onSelect: () => onAddSession() });
    if (onAddGroup) items.push({ label: "그룹 추가…", icon: FolderPlus, onSelect: () => onAddGroup() });
    return items;
  }

  function groupMenu(group: TreeGroup): MenuItem[] {
    // 합성 "로컬" 그룹은 빈 곳과 같은 최상위 추가 메뉴를 띄운다.
    if (group.local) return blankMenu();
    const items: MenuItem[] = [];
    if (onAddSession) items.push({ label: "세션 추가…", icon: Plus, onSelect: () => onAddSession(group.id) });
    if (onAddGroup) items.push({ label: "그룹 추가…", icon: FolderPlus, onSelect: () => onAddGroup(group.id) });
    if (onEditGroup) {
      if (items.length) items.push({ kind: "separator" });
      items.push({ label: "이름 바꾸기…", icon: SquarePen, onSelect: () => onEditGroup(group.id) });
    }
    if (onDeleteGroup) {
      if (items.length) items.push({ kind: "separator" });
      items.push({ label: "삭제", icon: Trash2, onSelect: () => onDeleteGroup(group.id) });
    }
    return items;
  }

  function targetMenu(target: MenuTarget): MenuItem[] {
    const items: MenuItem[] = [{ label: target.method, icon: target.kind === "s3" ? Database : target.kind === "rdp" ? Monitor
      : target.kind === "vnc" ? MonitorSmartphone : SquareTerminal, onSelect: target.open }];
    const add = (label: string, icon: LucideIcon, action?: () => void) => {
      if (action) items.push({ label, icon, onSelect: action });
    };
    add("S3 버킷 열기", Database, target.openStorage);
    add("터널 열기", Waypoints, target.openTunnel);
    add("nano 연동으로 열기", Pencil, target.openRemoteEditor);
    add("승격 실행…", ShieldAlert, target.elevate);
    const sshTargetId = target.sshTargetId;
    if (sshTargetId !== undefined) items.push({ kind: "separator" });
    add("호스트 키 확인…", KeyRound, sshTargetId !== undefined && onTrustSshHost ? () => onTrustSshHost(sshTargetId) : undefined);
    add("호스트 키 파일로 등록…", KeyRound, sshTargetId !== undefined ? () => onRegisterSshHost(sshTargetId) : undefined);
    if (target.edit || target.duplicate || target.remove) items.push({ kind: "separator" });
    add("편집…", SquarePen, target.edit);
    add("복제", Copy, target.duplicate);
    add("삭제", Trash2, target.remove);
    return items;
  }

  function navigate(event: KeyboardEvent<HTMLLIElement>, group?: TreeGroup, target?: MenuTarget) {
    event.stopPropagation();
    const row = event.currentTarget;
    const groupKey = group?.key;
    switch (event.key) {
      case "ArrowDown": case "ArrowUp": case "Home": case "End": {
        const rows = [...(tree.current?.querySelectorAll<HTMLLIElement>('[role="treeitem"]') ?? [])]
          .filter((element) => !element.closest("[hidden]"));
        const index = rows.indexOf(row);
        const next = event.key === "Home" ? 0 : event.key === "End" ? rows.length - 1
          : index + (event.key === "ArrowDown" ? 1 : -1);
        rows[next]?.focus();
        break;
      }
      case "ArrowRight":
        if (groupKey && !expanded(groupKey)) toggle(groupKey);
        else if (groupKey) row.querySelector<HTMLElement>('[role="treeitem"]')?.focus();
        break;
      case "ArrowLeft":
        if (groupKey && expanded(groupKey) && !term) toggle(groupKey);
        else row.parentElement?.closest<HTMLElement>('[role="treeitem"]')?.focus();
        break;
      case "Enter":
        if (target) target.open();
        else if (groupKey) toggle(groupKey);
        break;
      case " ":
        if (groupKey) toggle(groupKey);
        break;
      case "F2":
        if (target?.edit) target.edit();
        else if (group && !group.local && onEditGroup) onEditGroup(group.id);
        else return;
        break;
      case "Delete":
        if (target?.remove) target.remove();
        else if (group && !group.local && onDeleteGroup) onDeleteGroup(group.id);
        else return;
        break;
      case "ContextMenu": case "F10":
        if (event.key === "F10" && !event.shiftKey) return;
        if (target) showMenuAtRow(row, `${target.method} 메뉴`, targetMenu(target));
        else if (group) showMenuAtRow(row, group.local ? "연결 메뉴" : `${group.name} 그룹 메뉴`, groupMenu(group));
        break;
      default: return;
    }
    event.preventDefault();
  }

  function rowMenu(event: MouseEvent<HTMLLIElement>, label: string, items: MenuItem[]) {
    event.preventDefault();
    event.stopPropagation();
    showMenu(event.currentTarget, label, items, event.clientX, event.clientY);
  }

  const level = (value: number) => ({ "--tree-level": value }) as CSSProperties;

  function targetRow(target: TreeTarget, siblings: readonly TreeTarget[], depth: number) {
    const count = openCounts?.get(target.id) ?? 0;
    return <li key={target.key} role="treeitem" aria-level={depth} aria-selected={selected === target.key} tabIndex={tabStop === target.key ? 0 : -1}
      style={level(depth)} onFocus={() => setSelected(target.key)} onClick={(event) => event.currentTarget.focus()}
      onDoubleClick={target.open} onKeyDown={(event) => navigate(event, undefined, target)}
      onContextMenu={(event) => rowMenu(event, `${target.method} 메뉴`, targetMenu(target))}>
      <div className="tree-row"><ProtocolIcon kind={target.kind} /><span>{target.name}</span>
        {target.viaSsm && <span className="tree-via" role="img" title="SSM 경유" aria-label="SSM 경유" />}
        {siblings.some((other) => other.key !== target.key && other.name === target.name) && <small title={target.id}>{target.id.slice(0, 8)}</small>}
        {count > 0 && <span className="tree-open" role="img" title={`열린 탭 ${count}개`} aria-label={`열린 탭 ${count}개`} />}
      </div>
    </li>;
  }

  function groupRow(group: TreeGroup, siblings: readonly TreeGroup[], depth: number) {
    const open = expanded(group.key);
    const FolderIcon = open ? FolderOpen : Folder;
    return <li key={group.key} role="treeitem" aria-level={depth} aria-labelledby={group.key} aria-expanded={open}
      aria-selected={selected === group.key} tabIndex={tabStop === group.key ? 0 : -1} style={level(depth)}
      onFocus={(event) => { if (event.target === event.currentTarget) setSelected(group.key); }}
      onKeyDown={(event) => navigate(event, group)}
      onContextMenu={(event) => rowMenu(event, group.local ? "연결 메뉴" : `${group.name} 그룹 메뉴`, groupMenu(group))}>
      <div id={group.key} className="tree-row tree-group" onClick={(event) => {
        event.currentTarget.parentElement?.focus();
        toggle(group.key);
      }}><ChevronRight className="tree-chevron" data-open={open || undefined} aria-hidden="true" />
        <FolderIcon className="tree-folder" aria-hidden="true" /><span>{group.name}</span>
        {siblings.some((other) => other.key !== group.key && other.name === group.name) &&
          <small title={group.id || undefined}>{group.id.slice(0, 8) || "내 PC"}</small>}
      </div>
      <ul role="group" hidden={!open}>
        {group.groups.map((child) => groupRow(child, group.groups, depth + 1))}
        {group.targets.map((target) => targetRow(target, group.targets, depth + 1))}
      </ul>
    </li>;
  }

  return <>
    <div className="connection-search">
      <input className="ui-field" type="search" aria-label="연결 검색" placeholder="검색" value={query}
        onChange={(event) => setQuery(event.target.value)} />
    </div>
    {term && !groups.length && !topLevel.length && <div className="connection-status" role="status">검색 결과 없음</div>}
    <ul ref={tree} className="local-tree" role="tree" aria-label="연결" onContextMenu={(event) => {
      // 행 밖의 빈 곳이다. 행의 메뉴는 행에서 전파를 멈춘다.
      event.preventDefault();
      const active = document.activeElement instanceof HTMLElement && document.activeElement !== document.body ? document.activeElement : null;
      showMenu(active, "연결 메뉴", blankMenu(), event.clientX, event.clientY, false);
    }}>
      {groups.map((group) => groupRow(group, groups, 1))}
      {topLevel.map((target) => targetRow(target, topLevel, 1))}
    </ul>
    {menu && <Menu x={menu.x} y={menu.y} label={menu.label} items={menu.items} restoreFocus={menu.row}
      onClose={() => setMenu(undefined)} />}
  </>;
}
