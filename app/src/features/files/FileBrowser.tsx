import { useCallback, useLayoutEffect, useMemo, useRef, useState, type DragEvent, type KeyboardEvent, type MouseEvent, type ReactNode } from "react";
import {
  ArrowDown, ArrowUp, CloudOff, File, FileArchive, FileAudio, FileCode, FileImage, FileKey, FileSpreadsheet, FileSymlink, FileText, FileVideo, Folder,
  type LucideIcon,
} from "lucide-react";
import { Menu, type MenuItem } from "../ui/controls";
import "./files.css";

export type FileEntry = Readonly<{ key: string; name?: string; title?: string; link?: boolean; offline?: boolean }> & (
  | Readonly<{ kind: "folder" }>
  | Readonly<{ kind: "file"; bytes: number | null; modified?: string | null; storageClass?: string | null }>
);
export type EntryRef = Readonly<{ kind: FileEntry["kind"]; key: string }>;
export type FileColumn = "size" | "modified" | "class";
export type FileView = "list" | "icons";

export type VersionedFilesStatus =
  | Readonly<{ kind: "count"; count: number }>
  | Readonly<{ kind: "loading" | "unsupported" | "denied" | "unavailable" }>;

const versionStatus = { loading: "확인 중", unsupported: "지원 안 함", denied: "권한 거부", unavailable: "확인 실패" };

export function versionedFilesText(status: VersionedFilesStatus) {
  return `이전 버전이 있는 파일: ${status.kind === "count" ? `${status.count}개` : versionStatus[status.kind]}`;
}

export function VersionedFiles({ status }: { status: VersionedFilesStatus }) {
  return <span className="file-versioned" data-kind={status.kind} aria-live="polite">{versionedFilesText(status)}</span>;
}

const units = ["KB", "MB", "GB", "TB", "PB"];
export function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

export function exactBytes(bytes: number) {
  return `${bytes.toLocaleString("ko-KR")} 바이트`;
}

const pad = (value: number) => String(value).padStart(2, "0");
export function formatTime(value: string) {
  const time = new Date(value);
  if (Number.isNaN(time.getTime())) return value;
  return `${time.getFullYear()}-${pad(time.getMonth() + 1)}-${pad(time.getDate())} ${pad(time.getHours())}:${pad(time.getMinutes())}`;
}

export function sameEntry(entry: EntryRef | null | undefined, other: EntryRef | null | undefined) {
  return !!entry && !!other && entry.kind === other.kind && entry.key === other.key;
}

type FileType = "folder" | "text" | "code" | "image" | "archive" | "video" | "audio" | "sheet" | "key" | "file";
const typeIcons: Record<FileType, LucideIcon> = {
  folder: Folder, text: FileText, code: FileCode, image: FileImage, archive: FileArchive,
  video: FileVideo, audio: FileAudio, sheet: FileSpreadsheet, key: FileKey, file: File,
};
const extensions: Record<string, FileType> = {};
for (const [type, list] of [
  ["text", "txt md log ini cfg conf env rst"],
  ["code", "js mjs cjs ts tsx jsx json yaml yml toml xml html htm css scss py rb go rs java kt c h cpp hpp cs php sh bash zsh ps1 sql tf lua swift"],
  ["image", "png jpg jpeg gif svg webp bmp ico tif tiff avif heic"],
  ["archive", "zip gz tgz tar bz2 xz 7z rar zst jar war"],
  ["video", "mp4 mov mkv avi webm m4v"],
  ["audio", "mp3 wav flac ogg m4a aac"],
  ["sheet", "csv tsv xls xlsx ods parquet"],
  ["key", "pem key crt cer pub p12 pfx der"],
] as const) for (const extension of list.split(" ")) extensions[extension] = type;

export function fileType(entry: FileEntry): FileType {
  if (entry.kind === "folder") return "folder";
  const name = entry.name ?? entry.key;
  const dot = name.lastIndexOf(".");
  return dot > 0 && dot < name.length - 1 ? extensions[name.slice(dot + 1).toLowerCase()] ?? "file" : "file";
}

export function FileIcon({ entry }: { entry: FileEntry }) {
  const type = fileType(entry);
  const [Icon, shown] = entry.link ? [FileSymlink, "link"] : entry.offline ? [CloudOff, "offline"] : [typeIcons[type], type];
  return <Icon className="file-icon" data-type={shown} aria-hidden="true" />;
}

type SortKey = "name" | FileColumn;
type Sort = Readonly<{ key: SortKey; descending: boolean }>;
const columnLabels: Record<SortKey, string> = { name: "이름", size: "크기", modified: "수정 시각", class: "스토리지 클래스" };
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

function compareValues(a: FileEntry, b: FileEntry, key: SortKey) {
  const nameOrder = () => collator.compare(a.name ?? a.key, b.name ?? b.key) || (a.key < b.key ? -1 : a.key > b.key ? 1 : 0);
  if (key === "name" || a.kind === "folder" || b.kind === "folder") return nameOrder();
  const pick = (entry: Extract<FileEntry, { kind: "file" }>) =>
    key === "size" ? entry.bytes : key === "modified" ? (entry.modified ? Date.parse(entry.modified) : null) : entry.storageClass ?? null;
  const left = pick(a);
  const right = pick(b);
  if (left === right || (left !== null && right !== null && Number.isNaN(left) && Number.isNaN(right))) return nameOrder();
  if (left === null) return 1;
  if (right === null) return -1;
  return (typeof left === "number" && typeof right === "number" ? left - right : collator.compare(String(left), String(right))) || nameOrder();
}

function sortEntries(entries: readonly FileEntry[], { key, descending }: Sort) {
  return [...entries].sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === "folder" ? -1 : 1;
    const order = compareValues(a, b, key);
    return descending ? -order : order;
  });
}

type OpenMenu = Readonly<{ entries: readonly FileEntry[]; view: FileView; entry: FileEntry; row: HTMLElement; x: number; y: number }>;
const widths: Record<FileColumn, string> = { size: "6.5em", modified: "9.5rem", class: "9rem" };

/**
 * S3와 SFTP가 함께 쓰는 파일 목록이다. 한 번 누르면 선택, 두 번 누르거나 Enter를 누르면 onOpen을 호출한다.
 * 문맥 메뉴는 선택을 바꾸지 않고 해당 항목의 menuItems만 표시한다.
 */
export function FileBrowser({
  entries, selected, onSelect, onOpen, onParent, menuItems, label = "파일 목록", columns = ["size"], compact = false,
  view = "list", footer, empty = "항목 없음", busy = false, onDragStart,
}: {
  entries: readonly FileEntry[];
  selected: EntryRef | null;
  onSelect(entry: FileEntry): void;
  onOpen(entry: FileEntry): void;
  onParent?(): void;
  menuItems?(entry: FileEntry): readonly MenuItem[];
  label?: string;
  columns?: readonly FileColumn[];
  compact?: boolean;
  view?: FileView;
  footer?: ReactNode;
  empty?: string;
  busy?: boolean;
  /** 지정하면 항목을 끌 수 있다. 끌지 않을 항목은 event.preventDefault()로 막는다. */
  onDragStart?(entry: FileEntry, event: DragEvent<HTMLElement>): void;
}) {
  const [sort, setSort] = useState<Sort>({ key: "name", descending: false });
  const [menu, setMenu] = useState<OpenMenu | null>(null);
  const menuRef = useRef<OpenMenu | null>(null);
  const rows = useRef(new Map<string, HTMLElement>());
  const sorted = useMemo(() => sortEntries(entries, sort), [entries, sort]);
  const icons = view === "icons" && !compact;
  const shown = (compact ? columns.filter(column => column === "size") : columns)
    .filter(column => column !== "size" || entries.some(entry => entry.kind === "file" && entry.bytes !== null));
  const cursor = Math.max(0, sorted.findIndex(entry => sameEntry(entry, selected)));
  const id = (entry: FileEntry) => `${entry.kind}:${entry.key}`;

  const closeMenu = useCallback(() => {
    const current = menuRef.current;
    if (!current) return;
    menuRef.current = null;
    setMenu(null);
    const focused = document.activeElement;
    // 보기 전환으로 행이 다시 만들어졌으면 같은 항목의 새 행으로 돌아간다.
    const row = current.row.isConnected ? current.row : rows.current.get(`${current.entry.kind}:${current.entry.key}`);
    if (row && (!focused || focused === document.body || focused.closest(".ui-menu"))) row.focus({ preventScroll: true });
  }, []);

  useLayoutEffect(() => {
    // 목록이나 보기가 바뀌면 이전 항목의 메뉴를 닫는다.
    if (menu && (menu.entries !== entries || menu.view !== view)) closeMenu();
  }, [entries, view, menu, closeMenu]);

  function showMenu(entry: FileEntry, row: HTMLElement, x: number, y: number) {
    if (!menuItems?.(entry).length) return;
    const next = { entries, view, entry, row, x, y };
    menuRef.current = next;
    setMenu(next);
  }

  function contextMenu(event: MouseEvent<HTMLElement>, entry: FileEntry) {
    event.preventDefault();
    event.stopPropagation();
    showMenu(entry, event.currentTarget, event.clientX, event.clientY);
  }

  function move(index: number) {
    const entry = sorted[Math.max(0, Math.min(sorted.length - 1, index))];
    if (!entry) return;
    onSelect(entry);
    const row = rows.current.get(id(entry));
    row?.focus({ preventScroll: true });
    row?.scrollIntoView?.({ block: "nearest" });
  }

  function keyDown(event: KeyboardEvent<HTMLElement>, index: number, entry: FileEntry) {
    if (event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey)) {
      event.preventDefault();
      event.stopPropagation();
      const rect = event.currentTarget.getBoundingClientRect();
      showMenu(entry, event.currentTarget, rect.left + 24, rect.bottom);
      return;
    }
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const page = Math.max(1, Math.floor((event.currentTarget.parentElement?.clientHeight ?? 0) / Math.max(1, event.currentTarget.offsetHeight)));
    let next: number | undefined;
    switch (event.key) {
      case "ArrowDown": next = index + 1; break;
      case "ArrowUp": next = index - 1; break;
      case "ArrowRight": if (icons) next = index + 1; break;
      case "ArrowLeft": if (icons) next = index - 1; break;
      case "Home": next = 0; break;
      case "End": next = sorted.length - 1; break;
      case "PageDown": next = index + page; break;
      case "PageUp": next = index - page; break;
      case "Enter": event.preventDefault(); onOpen(entry); return;
      case "Backspace": if (onParent) { event.preventDefault(); onParent(); } return;
      default: return;
    }
    if (next === undefined) return;
    event.preventDefault();
    move(next);
  }

  function header(key: SortKey) {
    const active = sort.key === key;
    const Arrow = sort.descending ? ArrowDown : ArrowUp;
    return <div key={key} role="columnheader" className="file-cell" data-column={key}
      aria-sort={active ? (sort.descending ? "descending" : "ascending") : "none"}>
      <button type="button" className="file-sort" onClick={() => setSort(current => ({
        key, descending: current.key === key ? !current.descending : false,
      }))}>
        <span>{columnLabels[key]}</span>{active && <Arrow aria-hidden="true" />}
      </button>
    </div>;
  }

  function cells(entry: FileEntry) {
    return shown.map(column => {
      let text = "";
      let title: string | undefined;
      if (entry.kind === "file") {
        if (column === "size" && entry.bytes !== null) {
          text = formatBytes(entry.bytes);
          title = exactBytes(entry.bytes);
        } else if (column === "modified" && entry.modified) {
          text = formatTime(entry.modified);
          title = entry.modified;
        } else if (column === "class" && entry.storageClass) text = entry.storageClass;
      }
      return <div key={column} role="gridcell" className="file-cell" data-column={column} title={title}>{text}</div>;
    });
  }

  const template = ["minmax(0, 1fr)", ...shown.map(column => widths[column])].join(" ");
  const rowProps = (entry: FileEntry, index: number) => {
    const isSelected = sameEntry(entry, selected);
    const name = entry.name ?? entry.key;
    return {
      key: id(entry),
      ref: (node: HTMLElement | null) => {
        if (node) rows.current.set(id(entry), node);
        else rows.current.delete(id(entry));
      },
      tabIndex: index === cursor ? 0 : -1,
      "aria-selected": isSelected,
      "aria-keyshortcuts": "Shift+F10",
      "data-kind": entry.kind,
      "data-menu": menu?.entry === entry ? "" : undefined,
      title: entry.title ?? name,
      onClick: () => onSelect(entry),
      onDoubleClick: () => onOpen(entry),
      onContextMenu: (event: MouseEvent<HTMLElement>) => contextMenu(event, entry),
      onKeyDown: (event: KeyboardEvent<HTMLElement>) => keyDown(event, index, entry),
      draggable: onDragStart ? true : undefined,
      onDragStart: onDragStart && ((event: DragEvent<HTMLElement>) => onDragStart(entry, event)),
    };
  };

  return <div className="file-browser" data-compact={compact ? "" : undefined} data-view={icons ? "icons" : "list"} aria-busy={busy}>
    {icons ? <div className="file-scroll">
      <div role="listbox" aria-label={label} aria-orientation="horizontal" className="file-tiles">
        {sorted.map((entry, index) => {
          const { key, ...props } = rowProps(entry, index);
          return <div key={key} role="option" className="file-tile" {...props}>
            <FileIcon entry={entry} />
            <span className="file-name">{entry.name ?? entry.key}</span>
          </div>;
        })}
      </div>
      {!entries.length && <p className="file-empty" role="status">{empty}</p>}
      {footer && <div className="file-more">{footer}</div>}
    </div> : <div role="grid" aria-label={label} aria-rowcount={entries.length + 1} className="file-grid file-scroll"
      style={{ ["--file-columns" as string]: template }}>
      <div role="rowgroup" className="file-head">
        <div role="row" className="file-row">{header("name")}{shown.map(header)}</div>
      </div>
      <div role="rowgroup">
        {sorted.map((entry, index) => {
          const { key, ...props } = rowProps(entry, index);
          return <div key={key} role="row" className="file-row" {...props}>
            <div role="gridcell" className="file-cell file-name-cell">
              <FileIcon entry={entry} />
              <span className="file-name">{entry.name ?? entry.key}</span>
            </div>
            {cells(entry)}
          </div>;
        })}
        {footer && <div role="row" className="file-row file-more"><div role="gridcell" className="file-cell">{footer}</div></div>}
      </div>
    </div>}
    {!icons && !entries.length && <p className="file-empty" role="status">{empty}</p>}
    {menu && <Menu x={menu.x} y={menu.y} items={menuItems?.(menu.entry) ?? []} onClose={closeMenu} restoreFocus={menu.row}
      label={menu.entry.kind === "folder" ? "폴더 작업" : "파일 작업"} />}
  </div>;
}
