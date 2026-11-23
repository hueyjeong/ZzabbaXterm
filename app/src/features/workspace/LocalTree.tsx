import { useState, useRef, type KeyboardEvent } from "react";
import { ChevronRight, Database, Folder, FolderOpen, KeyRound, Pencil, RefreshCcw, ShieldAlert, SquareTerminal, Waypoints, type LucideIcon } from "lucide-react";
import type { SavedConnections } from "../connections/connectionIpc";
import { Menu, ProtocolIcon, type MenuItem, type ProtocolKind } from "../ui/controls";

type OpenTarget = (target?: SavedConnections["targets"][number]["profile"], options?: { readonly remoteEditor?: true }) => void;
type ElevateTarget = (target?: SavedConnections["targets"][number]["profile"]) => void;
type MenuTarget = {
  method: string; kind: ProtocolKind; open: () => void; sshTargetId?: string; s3TargetId?: string; openTunnel?: () => void;
  openRemoteEditor?: () => void; elevate?: () => void; openStorage?: () => void;
};

export function LocalTree({ profiles, openCounts, onOpen, onElevate, onLocalElevate, onRegisterSshHost, onRetestS3, onOpenTunnel, onOpenStorage }: {
  profiles?: SavedConnections; openCounts?: ReadonlyMap<string, number>; onOpen: OpenTarget; onElevate?: ElevateTarget; onLocalElevate?: () => void;
  onRegisterSshHost: (targetId: string) => void;
  onRetestS3?: (targetId: string) => void;
  onOpenTunnel: (targetId: string, name: string, method: "ssh" | "ssm") => void;
  onOpenStorage?: (targetId: string, name: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState("local");
  const [menu, setMenu] = useState<{ x: number; y: number; row: HTMLLIElement; target: MenuTarget }>();
  const tree = useRef<HTMLUListElement>(null);

  const term = query.trim().toLocaleLowerCase();
  const matches = (name: string) => name.toLocaleLowerCase().includes(term);
  const groups = [
    { key: "local", name: "로컬", id: "", targets: [{ key: "pc", name: "내 PC", id: "", kind: "local" as ProtocolKind, method: "로컬", open: () => onOpen(), elevate: onLocalElevate }] },
    ...(profiles?.groups.map(({ profile }) => ({
      key: `group:${profile.id}`, name: profile.displayName, id: profile.id,
      targets: profiles.targets.filter(({ profile: target }) => target.groupId === profile.id).map(({ profile: target }) => ({
        key: `target:${target.id}`, name: target.displayName, id: target.id, open: () => onOpen(target),
        kind: (target.defaultMethod === "local" ? "local" : target.defaultMethod) as ProtocolKind,
        viaSsm: target.defaultMethod === "ssh" && target.ssh.transport.kind === "ssm",
        method: target.defaultMethod === "ssm" ? "AWS Systems Manager" : target.defaultMethod === "ssh" ? "SSH"
          : target.defaultMethod === "s3" ? "S3" : "로컬",
        sshTargetId: target.defaultMethod === "ssh" ? target.id : undefined,
        openRemoteEditor: target.defaultMethod === "ssh" && target.ssh.transport.kind === "direct" &&
          (target.ssh.auth.kind === "agent" || target.ssh.auth.kind === "keyFile")
          ? () => onOpen(target, { remoteEditor: true }) : undefined,
        s3TargetId: target.defaultMethod === "s3" && onRetestS3 ? target.id : undefined,
        openTunnel: target.defaultMethod === "ssm" || (target.defaultMethod === "ssh" && target.ssh.transport.kind === "direct")
          ? () => onOpenTunnel(target.id, target.displayName, target.defaultMethod) : undefined,
        openStorage: target.defaultMethod === "ssm" && onOpenStorage ? () => onOpenStorage(target.id, target.displayName) : undefined,
        elevate: onElevate && target.defaultMethod === "ssh" ? () => onElevate(target) : undefined,
      })),
    })) ?? []),
  ].flatMap((group) => {
    if (matches(group.name)) return [group];
    const targets = group.targets.filter((target) => matches(target.name));
    return targets.length ? [{ ...group, targets }] : [];
  });
  const expanded = (key: string) => Boolean(term) || !collapsed.has(key);
  const tabStop = groups.some((group) => group.key === selected ||
    (expanded(group.key) && group.targets.some((target) => target.key === selected))) ? selected : groups[0]?.key;

  function toggle(key: string) {
    if (term) return;
    setCollapsed((previous) => {
      const next = new Set(previous);
      if (!next.delete(key)) next.add(key);
      return next;
    });
  }

  function showMenu(row: HTMLLIElement, target: MenuTarget, x: number, y: number) {
    row.focus();
    setMenu({ row, target, x, y });
  }

  function menuItems({ target }: NonNullable<typeof menu>): MenuItem[] {
    const items: MenuItem[] = [{ label: target.method, icon: target.kind === "s3" ? Database : SquareTerminal, onSelect: target.open }];
    const add = (label: string, icon: LucideIcon, action?: () => void) => {
      if (action) items.push({ label, icon, onSelect: action });
    };
    add("S3 버킷 열기", Database, target.openStorage);
    add("터널 열기", Waypoints, target.openTunnel);
    add("nano 연동으로 열기", Pencil, target.openRemoteEditor);
    add("승격 실행…", ShieldAlert, target.elevate);
    const sshTargetId = target.sshTargetId;
    const s3TargetId = target.s3TargetId;
    if (sshTargetId !== undefined || s3TargetId !== undefined) items.push({ kind: "separator" });
    add("호스트 키 등록", KeyRound, sshTargetId !== undefined ? () => onRegisterSshHost(sshTargetId) : undefined);
    add("연결 재시험", RefreshCcw, s3TargetId !== undefined ? () => onRetestS3?.(s3TargetId) : undefined);
    return items;
  }

  function navigate(event: KeyboardEvent<HTMLLIElement>, groupKey?: string, target?: MenuTarget) {
    event.stopPropagation();
    const row = event.currentTarget;
    switch (event.key) {
      case "ArrowDown": case "ArrowUp": case "Home": case "End": {
        const visible = [...(tree.current?.querySelectorAll<HTMLLIElement>('[role="treeitem"]') ?? [])]
          .filter((element) => !element.closest("[hidden]"));
        const index = visible.indexOf(row);
        const next = event.key === "Home" ? 0 : event.key === "End" ? visible.length - 1
          : index + (event.key === "ArrowDown" ? 1 : -1);
        visible[next]?.focus();
        break;
      }
      case "ArrowRight":
        if (groupKey && !expanded(groupKey)) toggle(groupKey);
        else if (groupKey) row.querySelector<HTMLElement>('[role="treeitem"]')?.focus();
        break;
      case "ArrowLeft":
        if (groupKey && expanded(groupKey)) toggle(groupKey);
        else if (!groupKey) row.parentElement?.closest<HTMLElement>('[role="treeitem"]')?.focus();
        break;
      case "Enter":
        if (target) target.open();
        else if (groupKey) toggle(groupKey);
        break;
      case " ":
        if (groupKey) toggle(groupKey);
        break;
      case "ContextMenu": case "F10": {
        if (!target || (event.key === "F10" && !event.shiftKey)) return;
        const rect = row.getBoundingClientRect();
        showMenu(row, target, rect.left + 24, rect.bottom);
        break;
      }
      default: return;
    }
    event.preventDefault();
  }

  return <>
    <div className="connection-search">
      <input className="ui-field" type="search" aria-label="연결 검색" placeholder="검색" value={query}
        onChange={(event) => setQuery(event.target.value)} />
    </div>
    {term && !groups.length && <div className="connection-status" role="status">검색 결과 없음</div>}
    <ul ref={tree} className="local-tree" role="tree" aria-label="연결">
      {groups.map((group) => {
        const open = expanded(group.key);
        const FolderIcon = open ? FolderOpen : Folder;
        return <li key={group.key} role="treeitem" aria-labelledby={group.key} aria-expanded={open}
          aria-selected={selected === group.key} tabIndex={tabStop === group.key ? 0 : -1}
          onFocus={(event) => { if (event.target === event.currentTarget) setSelected(group.key); }}
          onKeyDown={(event) => navigate(event, group.key)}>
          <div id={group.key} className="tree-row tree-group" onClick={(event) => {
            event.currentTarget.parentElement?.focus();
            toggle(group.key);
          }}><ChevronRight className="tree-chevron" data-open={open || undefined} aria-hidden="true" />
            <FolderIcon className="tree-folder" aria-hidden="true" /><span>{group.name}</span>
            {groups.some((other) => other.key !== group.key && other.name === group.name) &&
              <small title={group.id || undefined}>{group.id.slice(0, 8) || "내 PC"}</small>}
          </div>
          <ul role="group" hidden={!open}>
            {group.targets.map((target) => {
              const count = openCounts?.get(target.id) ?? 0;
              return <li key={target.key} role="treeitem" aria-selected={selected === target.key} tabIndex={tabStop === target.key ? 0 : -1}
                onFocus={() => setSelected(target.key)} onClick={(event) => event.currentTarget.focus()}
                onDoubleClick={target.open} onKeyDown={(event) => navigate(event, undefined, target)}
                onContextMenu={(event) => {
                  event.preventDefault();
                  showMenu(event.currentTarget, target, event.clientX, event.clientY);
                }}>
                <div className="tree-row"><ProtocolIcon kind={target.kind} /><span>{target.name}</span>
                  {"viaSsm" in target && target.viaSsm && <span className="tree-via" title="SSM 경유" aria-label="SSM 경유" />}
                  {group.targets.some((other) => other.key !== target.key && other.name === target.name) && <small title={target.id}>{target.id.slice(0, 8)}</small>}
                  {count > 0 && <span className="tree-open" title={`열린 탭 ${count}개`} aria-label={`열린 탭 ${count}개`} />}
                </div>
              </li>;
            })}
          </ul>
        </li>;
      })}
    </ul>
    {menu && <Menu x={menu.x} y={menu.y} label={`${menu.target.method} 메뉴`} items={menuItems(menu)} restoreFocus={menu.row}
      onClose={() => setMenu(undefined)} />}
  </>;
}
