import { useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import type { SavedConnections } from "../connections/connectionIpc";

type OpenTarget = (target?: SavedConnections["targets"][number]["profile"]) => void;

export function LocalTree({ profiles, onOpen }: { profiles?: SavedConnections; onOpen: OpenTarget }) {
  const [query, setQuery] = useState("");
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState("local");
  const [menu, setMenu] = useState<{ x: number; y: number; row: HTMLLIElement; open: () => void; method: string }>();
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
        method: target.defaultMethod === "ssm" ? "AWS Systems Manager" : "로컬",
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

  function showMenu(row: HTMLLIElement, open: () => void, x: number, y: number, method: string) {
    row.focus();
    setMenu({ row, open, x: Math.min(x, window.innerWidth - 170), y: Math.min(y, window.innerHeight - 44), method });
  }

  function navigate(event: KeyboardEvent<HTMLLIElement>, groupKey?: string, target?: { method: string; open: () => void }) {
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
        showMenu(row, target.open, rect.left, rect.bottom, target.method);
        break;
      }
      default: return;
    }
    event.preventDefault();
  }

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
                  showMenu(event.currentTarget, target.open, event.clientX, event.clientY, target.method);
                }}>
                <div className="tree-row"><span>{target.name}</span>
                  {group.targets.some((other) => other.key !== target.key && other.name === target.name) && <small>{target.id}</small>}
                </div>
              </li>
            ))}
          </ul>
        </li>
      ))}
    </ul>
    {menu && <div className="connection-menu" role="menu" style={{ left: menu.x, top: menu.y }}
      onBlur={() => setMenu(undefined)} onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          menu.row.focus();
        }
      }}>
      <button ref={menuButton} type="button" role="menuitem" onClick={() => {
        setMenu(undefined);
        menu.open();
      }}>{menu.method}</button>
    </div>}
  </>;
}
