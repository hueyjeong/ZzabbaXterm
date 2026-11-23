import { useEffect, useMemo, useState, type DragEvent } from "react";
import { ArrowUp, FolderOpen, HardDrive, HardDriveUpload, RefreshCw, X } from "lucide-react";
import { FileBrowser, sameEntry, type EntryRef, type FileEntry, type FileView } from "../files/FileBrowser";
import { IconButton, type MenuItem } from "../ui/controls";
import type { LocalFailure, LocalFileActions, LocalListing, LocalRoot } from "./s3";
import { localFailureText } from "./StorageStatus";

/** 로컬 창에서 S3 창으로 끄는 파일. 탭 끌기(TAB_DRAG_TYPE)와 OS 파일 놓기("Files")와 겹치지 않는다. */
export const LOCAL_DRAG_TYPE = "application/x-zzabba-local-file";
/** S3 창에서 로컬 창으로 끄는 객체. */
export const S3_DRAG_TYPE = "application/x-zzabba-s3-object";

export type LocalDrag = Readonly<{ sessionId: string; rootId: string; path: string; name: string; bytes: number | null }>;
export type S3Drag = Readonly<{ sessionId: string; key: string }>;

export type LocalView = Readonly<{
  root?: LocalRoot;
  /** 목록을 받은 루트 기준 상대 경로다. 빈 문자열이 루트다. */
  path: string;
  listing?: LocalListing;
  loading: boolean;
  picking: boolean;
  problem?: LocalFailure | Readonly<{ kind: "restricted" }> | "unavailable";
  selected: EntryRef | null;
  stale: boolean;
}>;

export const initialLocal: LocalView = { path: "", loading: false, picking: false, selected: null, stale: false };

export function joinLocal(path: string, name: string) {
  return path ? `${path}/${name}` : name;
}

/** 루트 label과 같은 구분자로 상대 경로를 붙인다. */
export function localDisplay(root: LocalRoot, path: string) {
  const separator = root.label.includes("\\") ? "\\" : "/";
  return path ? `${root.label.replace(/[\\/]+$/, "")}${separator}${path.split("/").join(separator)}` : root.label;
}

/** 끌어 온 항목의 JSON 값을 읽는다. 형식이 다르면 null이다. */
export function readDrag<T>(event: DragEvent, type: string): Partial<T> | null {
  try {
    const raw = event.dataTransfer.getData(type);
    const value: unknown = raw ? JSON.parse(raw) : null;
    return value && typeof value === "object" ? value as Partial<T> : null;
  } catch {
    return null;
  }
}

function describe(problem: NonNullable<LocalView["problem"]>) {
  return problem === "unavailable" ? "네이티브 요청 결과 수신 실패" : localFailureText(problem);
}

type Props = Readonly<{
  sessionId: string;
  view: LocalView;
  actions: LocalFileActions;
  update(change: (current: LocalView) => Partial<LocalView>): void;
  fileView: FileView;
  /** S3 위치가 확인됐고 다른 전송이 없을 때만 참이다. */
  canTransfer: boolean;
  /** 전송 승인·전송 중에는 루트를 바꾸지 않는다. */
  transferring: boolean;
  onUpload(path: string, name: string, bytes: number | null): void;
  onDropObject(key: string): void;
}>;

export function LocalPane({ sessionId, view, actions, update, fileView, canTransfer, transferring, onUpload, onDropObject }: Props) {
  const [dropping, setDropping] = useState(false);
  const { root, listing, path } = view;
  const navigable = !!root && !view.loading;

  async function load(target: LocalRoot, next: string) {
    update(() => ({ loading: true, problem: undefined, stale: false }));
    const same = (current: LocalView) => current.root?.rootId === target.rootId;
    try {
      const result = await actions.list({ sessionId, rootId: target.rootId, path: next });
      update(current => {
        if (!same(current)) return {};
        if (result.ok) return { loading: false, path: next, listing: result.value, ...(next !== current.path ? { selected: null } : {}) };
        // 승인이 폐기됐거나 루트가 옮겨졌으면 루트를 다시 골라야 한다.
        return result.error.kind === "denied" || result.error.kind === "rootChanged"
          ? { loading: false, problem: result.error, root: undefined, listing: undefined, path: "", selected: null }
          : { loading: false, problem: result.error };
      });
    } catch {
      update(current => same(current) ? { loading: false, problem: "unavailable" } : {});
    }
  }

  async function openRoot() {
    if (view.picking) return;
    update(() => ({ picking: true, problem: undefined }));
    try {
      const approval = await actions.openRoot({ sessionId });
      if (approval.kind !== "approved") {
        update(() => ({ picking: false, ...(approval.kind === "cancelled" ? {} : { problem: { kind: approval.kind } }) }));
        return;
      }
      const next: LocalRoot = { rootId: approval.rootId, label: approval.label };
      update(() => ({ picking: false, root: next, path: "", listing: undefined, selected: null }));
      await load(next, "");
    } catch {
      update(() => ({ picking: false, problem: "unavailable" }));
    }
  }

  useEffect(() => {
    if (view.stale && root && !view.loading) void load(root, path);
  });

  const go = (next: string) => { if (root && navigable) void load(root, next); };
  const parent = path ? path.slice(0, Math.max(0, path.lastIndexOf("/"))) : null;
  const goUp = () => { if (parent !== null) go(parent); };

  const entries = useMemo<FileEntry[]>(() => listing?.entries.map<FileEntry>(entry => {
    const key = joinLocal(path, entry.name);
    if (entry.kind === "folder") return { kind: "folder", key, name: entry.name, title: entry.name };
    return {
      kind: "file", key, name: entry.name, title: entry.name, link: entry.kind === "link", offline: !!entry.offline,
      bytes: entry.kind === "file" ? entry.bytes : null,
      modified: entry.modified === null ? null : new Date(entry.modified).toISOString(),
    };
  }) ?? [], [listing, path]);

  const selected = entries.find(entry => sameEntry(entry, view.selected)) ?? null;
  // 링크와 클라우드 전용 파일은 올리거나 끌 수 없다.
  const uploadable = (entry: FileEntry | null): entry is Extract<FileEntry, { kind: "file" }> =>
    entry?.kind === "file" && !entry.link && !entry.offline;
  const upload = (entry: FileEntry | null) => {
    if (canTransfer && uploadable(entry)) onUpload(entry.key, entry.name ?? entry.key, entry.bytes);
  };

  function menuItems(entry: FileEntry): MenuItem[] {
    if (entry.kind === "folder") return [{ label: "열기", icon: FolderOpen, disabled: !navigable, onSelect: () => go(entry.key) }];
    return uploadable(entry) ? [{ label: "S3로 업로드", icon: HardDriveUpload, disabled: !canTransfer, onSelect: () => upload(entry) }] : [];
  }

  function dragStart(entry: FileEntry, event: DragEvent<HTMLElement>) {
    if (!root || !uploadable(entry)) {
      event.preventDefault();
      return;
    }
    const data: LocalDrag = { sessionId, rootId: root.rootId, path: entry.key, name: entry.name ?? entry.key, bytes: entry.bytes };
    event.dataTransfer.setData(LOCAL_DRAG_TYPE, JSON.stringify(data));
    event.dataTransfer.effectAllowed = "copy";
  }

  const accepts = (event: DragEvent) => !!root && canTransfer && event.dataTransfer.types.includes(S3_DRAG_TYPE);
  const segments = path ? path.split("/") : [];
  const rootName = root ? root.label.split(/[\\/]/).filter(Boolean).pop() ?? root.label : "";

  return <section className="s3-local" aria-label="로컬 파일" data-drop={dropping ? "" : undefined}
    onDragOver={event => {
      if (!accepts(event)) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "copy";
      setDropping(true);
    }}
    onDragLeave={event => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDropping(false); }}
    onDrop={event => {
      setDropping(false);
      if (!accepts(event)) return;
      event.preventDefault();
      const data = readDrag<S3Drag>(event, S3_DRAG_TYPE);
      if (data?.sessionId === sessionId && typeof data.key === "string") onDropObject(data.key);
    }}>
    <div className="s3-local-head">
      {root && <nav aria-label="로컬 위치" className="s3-crumbs">
        <ol>
          <li><button type="button" className="s3-crumb bucket" title={root.label} aria-current={path === "" ? "location" : undefined}
            disabled={!navigable} onClick={() => go("")}><HardDrive aria-hidden="true" />{rootName}</button></li>
          {segments.map((segment, index) => {
            const target = segments.slice(0, index + 1).join("/");
            return <li key={target}>
              <span className="s3-crumb-sep" aria-hidden="true">/</span>
              <button type="button" className="s3-crumb" title={target} aria-current={target === path ? "location" : undefined}
                disabled={!navigable} onClick={() => go(target)}>{segment}</button>
            </li>;
          })}
        </ol>
      </nav>}
      {!root && <span className="s3-local-title"><HardDrive aria-hidden="true" />로컬</span>}
      {root && <div className="s3-local-tools" role="toolbar" aria-label="로컬 도구">
        <IconButton icon={FolderOpen} label="로컬 폴더 열기" disabled={view.picking || transferring} onClick={() => void openRoot()} />
        <>
          <IconButton icon={ArrowUp} label="로컬 상위 폴더" disabled={parent === null || !navigable} onClick={goUp} />
          <IconButton icon={RefreshCw} label="로컬 새로고침" className={view.loading ? "spinning" : undefined} disabled={!navigable}
            onClick={() => go(path)} />
          <IconButton icon={HardDriveUpload} label="S3로 업로드" disabled={!canTransfer || !uploadable(selected)} onClick={() => upload(selected)} />
        </>
      </div>}
    </div>
    {view.problem && <div className="s3-banner">
      <p role="alert" className="storage-problem">로컬 폴더: {describe(view.problem)}</p>
      <div className="s3-banner-actions">
        <IconButton small icon={X} label="로컬 오류 닫기" onClick={() => update(() => ({ problem: undefined }))} />
      </div>
    </div>}
    {!root && <div className="s3-local-body s3-local-start">
      <button type="button" className="ui-button" disabled={view.picking || transferring} onClick={() => void openRoot()}>
        <FolderOpen aria-hidden="true" />로컬 폴더 열기
      </button>
    </div>}
    {root && <div className="s3-local-body">
      {listing ? <FileBrowser label="로컬 파일 목록" entries={entries} selected={selected} view={fileView} busy={view.loading}
        columns={["size", "modified"]} empty="빈 폴더"
        onSelect={entry => update(() => ({ selected: { kind: entry.kind, key: entry.key } }))}
        onOpen={entry => { if (entry.kind === "folder") go(entry.key); }}
        onParent={parent === null ? undefined : goUp} menuItems={menuItems} onDragStart={dragStart} />
        : <p className="s3-placeholder">{view.loading ? "목록 요청 중" : ""}</p>}
    </div>}
    {listing && <footer className="s3-status"><span>{listing.entries.length}개 항목{listing.truncated ? " 이상" : ""}</span></footer>}
  </section>;
}
