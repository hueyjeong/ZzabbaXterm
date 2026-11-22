import { useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import type { SavedConnections } from "../connections/connectionIpc";

type OpenTarget = (target?: SavedConnections["targets"][number]["profile"]) => void;
type MenuTarget = { method: string; open: () => void; sshTargetId?: string; s3TargetId?: string };

export function LocalTree({ profiles, onOpen, onRegisterSshHost, onRetestS3 }: {
  profiles?: SavedConnections; onOpen: OpenTarget; onRegisterSshHost: (targetId: string) => void;
  onRetestS3?: (targetId: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState("local");
  const [menu, setMenu] = useState<{ x: number; y: number; row: HTMLLIElement; target: MenuTarget }>();
  const tree = useRef<HTMLUListElement>(null);
  const menuButton = useRef<HTMLButtonElement>(null);
  useLayoutEffect(() => { if (menu) menuButton.current?.focus(); }, [menu]);

  const term = query.trim().toLocaleLowerCase();
  const matches = (name: string) => name.toLocaleLowerCase().includes(term);
  const groups = [
    { key: "local", name: "로컬", id: "", targets: [{ key: "pc", name: "내 PC", id: "", method: "로컬", open: () => onOpen() }] },
    ...(profiles?.groups.map(({ profile }) => ({
      key: `group:${profile.id}`, name: profile.displayName, id: profile.id,
      targets: profiles.targets.filter(({ profile: target }) => target.groupId === profile.id).map(({ profile: target }) => ({
        key: `target:${target.id}`, name: target.displayName, id: target.id, open: () => onOpen(target),
        method: target.defaultMethod === "ssm" ? "AWS Systems Manager" : target.defaultMethod === "ssh" ? "SSH"
          : target.defaultMethod === "s3" ? "S3" : "로컬",
        sshTargetId: target.defaultMethod === "ssh" ? target.id : undefined,
        s3TargetId: target.defaultMethod === "s3" && onRetestS3 ? target.id : undefined,
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
    const menuHeight = target.sshTargetId !== undefined || target.s3TargetId !== undefined ? 80 : 44;
    setMenu({ row, target, x: Math.min(x, window.innerWidth - 170), y: Math.min(y, window.innerHeight - menuHeight) });
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
        showMenu(row, target, rect.left, rect.bottom);
        break;
      }
      default: return;
    }
    event.preventDefault();
  }

  const sshTargetId = menu?.target.sshTargetId;
  const s3TargetId = menu?.target.s3TargetId;

  return <>
    <input className="connection-search" type="search" aria-label="연결 검색" value={query} onChange={(event) => setQuery(event.target.value)} />
    {term && !groups.length && <div className="connection-status" role="status">검색 결과 없음</div>}
    <ul ref={tree} className="local-tree" role="tree" aria-label="연결">
      {groups.map((group) => (
        <li key={group.key} role="treeitem" aria-labelledby={group.key} aria-expanded={expanded(group.key)}
          aria-selected={selected === group.key} tabIndex={tabStop === group.key ? 0 : -1}
          onFocus={(event) => { if (event.target === event.currentTarget) setSelected(group.key); }}
          onKeyDown={(event) => navigate(event, group.key)}>
          <div id={group.key} className="tree-row" onClick={(event) => {
            event.currentTarget.parentElement?.focus();
            toggle(group.key);
          }}><span aria-hidden="true">{expanded(group.key) ? "▾" : "▸"}</span> <span>{group.name}</span>
            {groups.some((other) => other.key !== group.key && other.name === group.name) &&
              <small>{group.id || "내 PC"}</small>}
          </div>
          <ul role="group" hidden={!expanded(group.key)}>
            {group.targets.map((target) => (
              <li key={target.key} role="treeitem" aria-selected={selected === target.key} tabIndex={tabStop === target.key ? 0 : -1}
                onFocus={() => setSelected(target.key)} onClick={(event) => event.currentTarget.focus()}
                onDoubleClick={target.open} onKeyDown={(event) => navigate(event, undefined, target)}
                onContextMenu={(event) => {
                  event.preventDefault();
                  showMenu(event.currentTarget, target, event.clientX, event.clientY);
                }}>
                <div className="tree-row"><span>{target.name}</span>
                  {target.method === "S3" && <small>S3</small>}
                  {group.targets.some((other) => other.key !== target.key && other.name === target.name) && <small>{target.id}</small>}
                </div>
              </li>
            ))}
          </ul>
        </li>
      ))}
    </ul>
    {menu && <div className="connection-menu" role="menu" style={{ left: menu.x, top: menu.y, display: "grid" }}
      onBlur={(event) => { if (!event.currentTarget.contains(event.relatedTarget)) setMenu(undefined); }} onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          menu.row.focus();
        }
      }}>
      <button ref={menuButton} type="button" role="menuitem" onClick={() => {
        setMenu(undefined);
        menu.target.open();
      }}>{menu.target.method}</button>
      {sshTargetId !== undefined && <button type="button" role="menuitem" onClick={() => {
        menu.row.focus();
        setMenu(undefined);
        onRegisterSshHost(sshTargetId);
      }}>호스트 키 등록</button>}
      {s3TargetId !== undefined && <button type="button" role="menuitem" onClick={() => {
        menu.row.focus();
        setMenu(undefined);
        onRetestS3?.(s3TargetId);
      }}>연결 재시험</button>}
    </div>}
  </>;
}
