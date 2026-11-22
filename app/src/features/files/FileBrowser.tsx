import { useLayoutEffect, useRef, useState, type MouseEvent } from "react";
import "./files.css";

export type FileEntry = Readonly<{ key: string; name?: string }> & (
  | Readonly<{ kind: "folder" }>
  | Readonly<{ kind: "file"; bytes: number | null }>
);

export type VersionedFilesStatus =
  | Readonly<{ kind: "count"; count: number }>
  | Readonly<{ kind: "loading" | "unsupported" | "denied" | "unavailable" }>;

const versionStatus = { loading: "확인 중", unsupported: "지원 안 함", denied: "권한 거부", unavailable: "확인 실패" };
const modes = [{ id: "list", label: "목록" }, { id: "icons", label: "아이콘" }] as const;

type Menu = {
  entries: readonly FileEntry[];
  entry: FileEntry;
  button: HTMLButtonElement;
  x: number;
  y: number;
};

export function FileBrowser({ entries, selectedKey, onOpenFolder, onSelectFile, onEditFile, versionedFiles, label = "파일 목록", nameColumn = "이름" }: {
  entries: readonly FileEntry[];
  selectedKey: string | null;
  onOpenFolder(key: string): void;
  onSelectFile(key: string): void;
  onEditFile?(key: string): void;
  versionedFiles?: VersionedFilesStatus;
  label?: string;
  nameColumn?: string;
}) {
  const [mode, setMode] = useState<(typeof modes)[number]["id"]>("list");
  const [menu, setMenu] = useState<Menu | null>(null);
  const popup = useRef<HTMLDivElement>(null);
  const viewControls = useRef<HTMLDivElement>(null);
  const canEdit = menu?.entry.kind === "file" && !!onEditFile;

  useLayoutEffect(() => {
    const node = popup.current;
    if (!menu || !node) return;
    if (menu.entries !== entries) {
      const target = menu.button.isConnected ? menu.button : viewControls.current?.querySelector<HTMLButtonElement>('[aria-pressed="true"]');
      target?.focus({ preventScroll: true });
      setMenu(null);
      return;
    }

    const { width, height } = node.getBoundingClientRect();
    node.style.left = `${Math.max(0, Math.min(menu.x, window.innerWidth - width))}px`;
    node.style.top = `${Math.max(0, Math.min(menu.y, window.innerHeight - height))}px`;
    node.querySelector("button")?.focus({ preventScroll: true });

    const dismiss = (event: Event) => {
      if (event.target instanceof Node && node.contains(event.target)) return;
      const focused = document.activeElement;
      if (focused === document.body || node.contains(focused)) menu.button.focus({ preventScroll: true });
      setMenu(null);
    };
    document.addEventListener("click", dismiss, true);
    document.addEventListener("contextmenu", dismiss, true);
    return () => {
      document.removeEventListener("click", dismiss, true);
      document.removeEventListener("contextmenu", dismiss, true);
    };
  }, [menu, entries, canEdit]);

  function activate(entry: FileEntry) {
    if (entry.kind === "folder") onOpenFolder(entry.key);
    else onSelectFile(entry.key);
  }

  function showMenu(event: MouseEvent<HTMLElement>, entry: FileEntry) {
    const button = event.currentTarget.querySelector("button");
    if (!button) return;
    event.preventDefault();
    event.stopPropagation();
    setMenu({ entries, entry, button, x: event.clientX, y: event.clientY });
  }

  function entryButton(entry: FileEntry) {
    const name = entry.name ?? entry.key;
    return <button type="button" className="file-browser-entry" title={name}
      aria-pressed={entry.kind === "file" ? selectedKey === entry.key : undefined}
      aria-keyshortcuts="Shift+F10"
      aria-description={mode === "icons" ? (entry.kind === "folder" ? "폴더" : "파일") : undefined}
      onClick={() => activate(entry)} onKeyDown={event => {
        if (event.key !== "ContextMenu" && !(event.key === "F10" && event.shiftKey)) return;
        event.preventDefault();
        event.stopPropagation();
        const button = event.currentTarget;
        const rect = button.getBoundingClientRect();
        setMenu({ entries, entry, button, x: rect.left, y: rect.bottom });
      }}>
      {mode === "icons" && <svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.5">
        {entry.kind === "folder"
          ? <path d="M2.75 5.75h6l2 2h10.5v12.5H2.75z" />
          : <path d="M5.75 2.75h8l4.5 4.5v14H5.75zM13.75 2.75v4.5h4.5" />}
      </svg>}
      <span>{name}</span>
    </button>;
  }

  function closeMenu() {
    menu?.button.focus({ preventScroll: true });
    setMenu(null);
  }

  return <div className="file-browser">
    <div className="file-browser-toolbar">
      <div role="group" aria-label="파일 보기" ref={viewControls}>
        {modes.map(({ id, label }) => <button key={id} type="button" aria-pressed={mode === id} onClick={event => {
          event.currentTarget.focus({ preventScroll: true });
          setMode(id);
        }}>{label}</button>)}
      </div>
      {versionedFiles && <span className="file-browser-history" aria-live="polite">이전 버전이 있는 파일: {versionedFiles.kind === "count"
        ? `${versionedFiles.count}개` : versionStatus[versionedFiles.kind]}</span>}
    </div>
    {mode === "list" ? <table className="file-browser-list" aria-label={label}>
      <thead><tr><th scope="col">유형</th><th scope="col">{nameColumn}</th><th scope="col">크기 (바이트)</th></tr></thead>
      <tbody>{entries.map(entry => <tr key={`${entry.kind}:${entry.key}`} onContextMenu={event => showMenu(event, entry)}>
        <td>{entry.kind === "folder" ? "폴더" : "파일"}</td>
        <td>{entryButton(entry)}</td>
        <td>{entry.kind === "file" ? entry.bytes ?? "—" : "—"}</td>
      </tr>)}</tbody>
    </table> : <ul className="file-browser-icons" aria-label={label}>
      {entries.map(entry => <li key={`${entry.kind}:${entry.key}`} onContextMenu={event => showMenu(event, entry)}>{entryButton(entry)}</li>)}
    </ul>}
    {menu && <div ref={popup} className="file-browser-menu" role="menu" aria-label={menu.entry.kind === "folder" ? "폴더 작업" : "파일 작업"}
      onContextMenu={event => event.preventDefault()}
      onBlur={event => {
        if (event.relatedTarget && !event.currentTarget.contains(event.relatedTarget)) setMenu(null);
      }} onKeyDown={event => {
        if (event.key === "Escape") {
          event.preventDefault();
          event.stopPropagation();
          closeMenu();
        } else if (event.key === "Tab") {
          closeMenu();
        } else if (["ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) {
          event.preventDefault();
          event.stopPropagation();
          const items = event.currentTarget.querySelectorAll("button");
          const first = items[0];
          const last = items[items.length - 1];
          (event.key === "Home" ? first : event.key === "End" ? last : event.target === first ? last : first)?.focus();
        }
      }}>
      {canEdit && <button type="button" role="menuitem" tabIndex={-1} onClick={() => {
        closeMenu();
        onEditFile?.(menu.entry.key);
      }}>편집</button>}
      <button type="button" role="menuitem" tabIndex={-1} onClick={() => {
        closeMenu();
        activate(menu.entry);
      }}>{menu.entry.kind === "folder" ? "열기" : "선택"}</button>
    </div>}
  </div>;
}
