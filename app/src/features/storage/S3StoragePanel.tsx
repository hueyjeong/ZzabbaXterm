import {
  useEffect, useLayoutEffect, useMemo, useRef, useState, type DragEvent as ReactDragEvent, type KeyboardEvent, type PointerEvent as ReactPointerEvent,
} from "react";
import {
  ArrowLeft, ArrowRight, ArrowUp, Copy, Database, Download, FilePen, FolderOpen, Globe, HardDriveDownload, KeyRound, LayoutGrid, Link, List,
  LogIn, MapPin, RefreshCw, Server, TextCursorInput, Upload, X,
} from "lucide-react";
import {
  FileBrowser, VersionedFiles, exactBytes, formatBytes, sameEntry, type EntryRef, type FileEntry, type FileView, type VersionedFilesStatus,
} from "../files/FileBrowser";
import { IconButton, type MenuItem } from "../ui/controls";
import type { BucketRow, LocalFileActions, ObjectRow, S3Actions, S3FileSelection, S3Location, S3Session, StorageEditorOpen } from "./s3";
import {
  LOCAL_DRAG_TYPE, LocalPane, S3_DRAG_TYPE, initialLocal, localDisplay, readDrag, type LocalDrag, type LocalView, type S3Drag,
} from "./LocalPane";
import { folderLabel, parentPrefix, prefixSegments, relativeName } from "./s3Path";
import { S3Tree, type PrefixTree } from "./S3Tree";
import { StorageProblem, TransferQueue, type Problem, type Transfer } from "./StorageStatus";
import type { CompatibleSession } from "./compatible-settings";
import { bindStorageEditor } from "./storageEditorIpc";
import { countPreviousFiles, localFileActions } from "./storageIpc";
import "./storage.css";

type ObjectListing = Readonly<{ location: S3Location; rows: readonly ObjectRow[]; nextToken: string | null }>;
type BucketListing = Readonly<{ rows: readonly BucketRow[]; nextToken: string | null }>;
type View = {
  input?: S3Location;
  confirmed: boolean;
  listing?: ObjectListing;
  buckets?: BucketListing;
  loading: boolean;
  problem?: Problem;
  editError?: string;
  selected: EntryRef | null;
  transfers: readonly Transfer[];
  uploadKey: string | null;
  invalidUploadPaste?: boolean;
  requestedLocation?: S3Location | null;
  expired?: boolean;
  editing: boolean;
  back: readonly S3Location[];
  forward: readonly S3Location[];
  tree: PrefixTree;
  local: LocalView;
  /** 로컬 창에서 업로드를 마친 위치다. 목록이 같은 위치면 다시 읽는다. */
  remoteStale?: S3Location;
};
const initial: View = {
  confirmed: false, loading: false, selected: null, transfers: [], uploadKey: null, editing: false, back: [], forward: [], tree: new Map(),
  local: initialLocal,
};
const historyLimit = 50;

type Props = Readonly<{
  session: S3Session;
  initialPage?: CompatibleSession["initial"];
  actions: S3Actions;
  onRelogin?: () => void;
  refresh?: number;
  editEnabled?: boolean;
  onEditFile?(opened: StorageEditorOpen): false | void;
  countHistory?: typeof countPreviousFiles;
  localActions?: LocalFileActions;
}>;

function sameLocation(a: S3Location | undefined | null, b: S3Location | undefined | null) {
  return !!a && !!b && a.bucket === b.bucket && a.prefix === b.prefix && a.region === b.region;
}

/** 로컬 파일을 현재 S3 접두어 아래에 올릴 전체 키다. 접두어는 폴더로 본다. */
function uploadKey(prefix: string, name: string) {
  return !prefix || prefix.endsWith("/") ? `${prefix}${name}` : `${prefix}/${name}`;
}

/** 목록을 연 접두어의 조상 경로와 하위 접두어를 트리에 기록한다. */
function remember(tree: PrefixTree, { bucket, prefix }: S3Location, rows: readonly ObjectRow[], append: boolean): PrefixTree {
  const prefixes = rows.flatMap(row => row.kind === "prefix" ? [row.prefix] : []);
  const children = new Map(tree.get(bucket) ?? []);
  let ancestor = "";
  for (const { prefix: segment } of prefixSegments(prefix)) {
    const known = children.get(ancestor) ?? [];
    if (!known.includes(segment)) children.set(ancestor, [...known, segment]);
    ancestor = segment;
  }
  const known = append ? children.get(prefix) ?? [] : [];
  children.set(prefix, [...known, ...prefixes.filter(child => !known.includes(child))]);
  return new Map(tree).set(bucket, children);
}

export function S3StoragePanel({
  session, initialPage, actions, onRelogin, refresh = 0, editEnabled = true, onEditFile, countHistory = countPreviousFiles,
  localActions = localFileActions,
}: Props) {
  const fixedLocation = initialPage?.kind === "objects" ? initialPage.location : undefined;
  const [views, setViews] = useState(() => {
    const initialViews = new Map<string, View>();
    if (initialPage?.kind === "buckets") {
      initialViews.set(session.sessionId, { ...initial, buckets: initialPage.page });
    } else if (initialPage?.kind === "objects") {
      const { location, page } = initialPage;
      initialViews.set(session.sessionId, {
        ...initial, input: location, requestedLocation: location, confirmed: true, listing: { location, ...page },
        tree: remember(initial.tree, location, page.rows, false),
      });
    }
    return initialViews;
  });
  const [history, setHistory] = useState<{ request: Parameters<typeof countPreviousFiles>[0]; count: typeof countPreviousFiles; status: VersionedFilesStatus }>();
  const [fileView, setFileView] = useState<FileView>("list");
  const [queueOpen, setQueueOpen] = useState(false);
  const [treeWidth, setTreeWidth] = useState(240);
  const [localWidth, setLocalWidth] = useState(360);
  const [dropping, setDropping] = useState(false);
  const [stacked, setStacked] = useState(false);
  const split = useRef<HTMLDivElement>(null);
  // 좁은 폭에서는 두 창을 위아래로 쌓는다. 고정 위치 메뉴가 어긋나지 않게 컨테이너 쿼리 대신 크기를 관찰한다.
  useLayoutEffect(() => {
    const node = split.current;
    if (!node || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(([entry]) => setStacked(entry.contentRect.width < 760));
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  const editAttempt = useRef<{ sessionId: string } | null>(null);
  const editListener = useRef(onEditFile);
  useLayoutEffect(() => { editListener.current = onEditFile; }, [onEditFile]);
  const uploadAnchor = useRef<HTMLSpanElement>(null);
  const { sessionId } = session;
  const compatible = session.compatible;
  const view = views.get(sessionId) ?? initial;
  const input = view.input ?? session.location ?? { bucket: "", prefix: "", region: session.region };

  function update(change: (current: View) => Partial<View>) {
    setViews(current => {
      const previous = current.get(sessionId) ?? initial;
      return new Map(current).set(sessionId, { ...previous, ...change(previous) });
    });
  }

  function updateLocal(change: (current: LocalView) => Partial<LocalView>) {
    update(current => ({ local: { ...current.local, ...change(current.local) } }));
  }

  async function load(location: S3Location | null, { token = null, afterRelogin = false, step }: {
    token?: string | null; afterRelogin?: boolean; step?: "back" | "forward";
  } = {}) {
    if (view.loading || (fixedLocation && (location?.bucket !== fixedLocation.bucket || location.region !== fixedLocation.region))) return;
    editAttempt.current = null;
    update(() => ({
      loading: true, requestedLocation: location, problem: undefined, editError: undefined,
      ...(afterRelogin ? { expired: false, confirmed: false } : {}),
    }));
    const failed = (error: Problem) => update(() => ({
      loading: false, problem: error,
      ...(error !== "unavailable" && error.failure.kind === "ssoLoginRequired" ? { expired: true } : {}),
      ...(location && token === null ? { editing: true } : {}),
    }));
    try {
      if (!location) {
        const result = await actions.listBuckets({ sessionId, token });
        if (!result.ok) return failed(result.error);
        update(current => ({
          loading: false,
          buckets: { rows: token !== null && current.buckets ? [...current.buckets.rows, ...result.value.rows] : result.value.rows, nextToken: result.value.nextToken },
        }));
        return;
      }
      const result = await actions.listObjects({ sessionId, ...location, token });
      if (!result.ok) return failed(result.error);
      update(current => {
        const previous = current.listing;
        const rows = token !== null && previous ? [...previous.rows, ...result.value.rows] : result.value.rows;
        const changed = !previous || !sameLocation(previous.location, location);
        let { back, forward } = current;
        if (changed && previous && token === null) {
          if (step === "back") {
            back = back.slice(0, -1);
            forward = [...forward, previous.location];
          } else if (step === "forward") {
            forward = forward.slice(0, -1);
            back = [...back, previous.location];
          } else {
            back = [...back, previous.location].slice(-historyLimit);
            forward = [];
          }
        }
        return {
          loading: false,
          listing: { location, rows, nextToken: result.value.nextToken },
          tree: remember(current.tree, location, result.value.rows, token !== null),
          back, forward,
          ...(token === null ? { confirmed: true, editing: false, input: location } : {}),
          ...(changed ? { selected: null } : {}),
        };
      });
    } catch {
      update(() => ({ loading: false, problem: "unavailable" }));
    }
  }

  function open(location: S3Location, step?: "back" | "forward") {
    update(() => ({ input: location, confirmed: false }));
    void load(location, { step });
  }

  const booted = useRef<string | undefined>(undefined);
  useEffect(() => {
    if (!session.location || booted.current === sessionId || views.has(sessionId)) return;
    booted.current = sessionId;
    open(session.location);
  });

  const refreshed = useRef(refresh);
  useLayoutEffect(() => {
    if (refreshed.current === refresh || view.loading) return;
    refreshed.current = refresh;
    void load(view.requestedLocation ?? null, { afterRelogin: true });
  });

  const listing = view.listing;
  const location = listing?.location ?? null;
  const transferring = view.transfers.some(({ state }) => state === "selecting" || state === "running");
  const expired = !compatible && !!view.expired;
  const detectedRegion = !compatible && view.problem && view.problem !== "unavailable" && view.problem.failure.kind === "bucketRegionMismatch" ? view.problem.failure.region : null;
  const canTransfer = view.confirmed && !view.loading && !transferring && !expired;
  const navigable = !view.loading && !transferring;
  const selected = listing && view.selected && listing.rows.some(row => row.kind === "object"
    ? view.selected?.kind === "file" && row.key === view.selected.key
    : view.selected?.kind === "folder" && row.prefix === view.selected.key) ? view.selected : null;
  const selectedKey = selected?.kind === "file" ? selected.key : null;
  const canEdit = !compatible && editEnabled && !!onEditFile && view.confirmed && !view.loading && !expired;
  const editing = view.editing || !listing;
  useLayoutEffect(() => () => { editAttempt.current = null; }, [sessionId, canEdit]);

  function approvedRowId(key: string | null | undefined) {
    const row = listing?.rows.find(row => row.kind === "object" && row.key === key);
    return row?.kind === "object" && typeof row.rowId === "string" && row.rowId.length ? row.rowId : undefined;
  }

  async function edit(key: string) {
    if (!canEdit || editAttempt.current) return;
    const rowId = approvedRowId(key);
    if (!rowId) return;
    const current = { sessionId };
    editAttempt.current = current;
    const editor = bindStorageEditor(sessionId);
    update(() => ({ editError: undefined }));
    try {
      const file = await editor.open(rowId);
      const close = () => editor.close(file.handle);
      let handedOff = false;
      try {
        const listener = editListener.current;
        if (editAttempt.current === current && listener) {
          handedOff = listener({ file, transport: editor.transport, close }) !== false;
        }
      } finally {
        if (!handedOff) await close();
      }
    } catch (error) {
      if (editAttempt.current === current) {
        update(() => ({ editError: error instanceof Error ? error.message : "네이티브 요청 결과 수신 실패" }));
      }
    } finally {
      if (editAttempt.current === current) editAttempt.current = null;
    }
  }

  // 같은 목록으로 돌아오면 이미 받은 수를 다시 요청하지 않는다.
  const listingRequest = useMemo(() => listing ? { sessionId, ...listing.location } : undefined, [listing, sessionId]);
  const historyRequest = !compatible && view.confirmed && !view.loading && !view.problem && !expired ? listingRequest : undefined;
  const historyDone = !!historyRequest && history?.request === historyRequest && history.count === countHistory;

  useEffect(() => {
    if (!historyRequest || historyDone) return;
    let active = true;
    async function loadHistory(request: NonNullable<typeof historyRequest>) {
      let status: VersionedFilesStatus = { kind: "unavailable" };
      try {
        const result = await countHistory(request);
        if (result.ok) status = { kind: "count", count: result.value };
        else if (result.error.failure.kind === "service") {
          const { status: httpStatus, code } = result.error.failure;
          if (httpStatus === 403) status = { kind: "denied" };
          else if (httpStatus === 501 || code === "NotImplemented" || code === "UnsupportedOperation") status = { kind: "unsupported" };
        }
      } catch {
        // IPC 오류도 객체 목록과 별도로 표시한다.
      }
      if (active) setHistory({ request, count: countHistory, status });
    }
    void loadHistory(historyRequest);
    return () => { active = false; };
  }, [countHistory, historyRequest, historyDone]);

  const versionedFiles: VersionedFilesStatus | undefined = historyRequest
    ? historyDone && history ? history.status : { kind: "loading" }
    : undefined;

  /** local이 있으면 대화상자 대신 승인된 로컬 루트의 경로로 전송한다. */
  async function transfer(direction: S3FileSelection["direction"], key: string | null,
    local?: Readonly<{ rootId: string; path: string; display: string; size?: number | null }>) {
    if (!canTransfer || !location || key === null || key === "") return;
    const target = { sessionId, bucket: location.bucket, region: location.region, key };
    const row = direction === "download" ? listing?.rows.find(row => row.kind === "object" && row.key === key) : undefined;
    const id = crypto.randomUUID();
    const display = {
      id, direction, bucket: location.bucket, region: location.region, key,
      size: local?.size ?? (row?.kind === "object" ? row.bytes : null), ...(local ? { local: local.display } : {}),
    };
    const set = (next: Transfer, extra: Partial<View> = {}) =>
      update(current => ({ transfers: current.transfers.map(item => item.id === id ? next : item), ...extra }));
    update(current => ({ transfers: [{ ...display, state: "selecting" } as Transfer, ...current.transfers] }));
    setQueueOpen(true);
    try {
      const selection = local
        ? await localActions.select({ ...target, rootId: local.rootId, path: local.path, direction })
        : await actions.selectLocalFile({ ...target, direction });
      if (selection.kind !== "approved") {
        set({ ...display, state: selection.kind });
        return;
      }
      const request = { ...target, approvalId: selection.approvalId };
      set({ ...display, state: "running" });
      const result = direction === "upload" ? await actions.upload(request) : await actions.download(request);
      if (!result.ok) {
        const loginRequired = result.error.failure.kind === "ssoLoginRequired";
        if (loginRequired && editAttempt.current?.sessionId === sessionId) editAttempt.current = null;
        set({ ...display, state: "failed", problem: result.error }, loginRequired ? { expired: true } : {});
      } else if (typeof result.value === "number") set({ ...display, state: "uploaded", bytes: result.value }, local ? { remoteStale: location } : {});
      else {
        set({ ...display, state: "downloaded", result: result.value });
        if (local) updateLocal(current => current.root?.rootId === local.rootId ? { stale: true } : {});
      }
    } catch {
      set({ ...display, state: "failed", problem: "unavailable" });
    }
  }

  const localRoot = view.local.root;
  const canLocalTransfer = canTransfer && !!localRoot && !view.local.loading;

  function downloadLocal(key: string | null) {
    if (!canLocalTransfer || !localRoot || !listing?.rows.some(row => row.kind === "object" && row.key === key)) return;
    const { path } = view.local;
    void transfer("download", key, { rootId: localRoot.rootId, path, display: localDisplay(localRoot, path) });
  }

  function uploadLocal(path: string, name: string, size: number | null) {
    if (!canLocalTransfer || !localRoot || !location) return;
    void transfer("upload", uploadKey(location.prefix, name), { rootId: localRoot.rootId, path, display: localDisplay(localRoot, path), size });
  }

  useEffect(() => {
    const stale = view.remoteStale;
    if (!stale || view.loading || transferring) return;
    update(() => ({ remoteStale: undefined }));
    if (listing && view.confirmed && sameLocation(listing.location, stale)) void load(listing.location);
  });

  function closeUpload(focus = false) {
    update(() => ({ uploadKey: null, invalidUploadPaste: false }));
    if (focus) uploadAnchor.current?.querySelector("button")?.focus();
  }

  useEffect(() => {
    if (view.uploadKey === null) return;
    const outside = (event: PointerEvent) => {
      if (!uploadAnchor.current?.contains(event.target as Node)) closeUpload();
    };
    window.addEventListener("pointerdown", outside, true);
    return () => window.removeEventListener("pointerdown", outside, true);
  });

  const entries = useMemo<FileEntry[]>(() => {
    if (!listing) return [];
    const base = listing.location.prefix;
    return listing.rows.map<FileEntry>(row => row.kind === "prefix"
      ? { kind: "folder", key: row.prefix, name: folderLabel(relativeName(row.prefix, base)), title: row.prefix }
      : {
        kind: "file", key: row.key, name: relativeName(row.key, base), title: row.key, bytes: row.bytes,
        modified: row.lastModified ?? null, storageClass: row.storageClass ?? null,
      });
  }, [listing]);

  const parent = location ? parentPrefix(location.prefix) : null;
  const goUp = () => { if (location && parent !== null && navigable) open({ ...location, prefix: parent }); };
  const goBack = () => { const target = view.back[view.back.length - 1]; if (target && navigable) open(target, "back"); };
  const goForward = () => { const target = view.forward[view.forward.length - 1]; if (target && navigable) open(target, "forward"); };
  const reload = () => { if (location && navigable) open(location); };

  function openEntry(entry: FileEntry) {
    if (entry.kind === "folder") {
      if (location && navigable) open({ ...location, prefix: entry.key });
    } else if (canEdit && approvedRowId(entry.key)) void edit(entry.key);
  }

  function menuItems(entry: FileEntry): MenuItem[] {
    const copy: MenuItem = { label: "전체 키 복사", icon: Copy, onSelect: () => { void navigator.clipboard?.writeText(entry.key).catch(() => undefined); } };
    if (entry.kind === "folder") return [{ label: "열기", icon: FolderOpen, disabled: !navigable, onSelect: () => openEntry(entry) }, { kind: "separator" }, copy];
    return [
      ...(canEdit && approvedRowId(entry.key) ? [{ label: "편집", icon: FilePen, onSelect: () => void edit(entry.key) }] : []),
      { label: "다운로드", icon: Download, disabled: !canTransfer, onSelect: () => void transfer("download", entry.key) },
      ...(localRoot ? [{ label: "로컬 폴더로 다운로드", icon: HardDriveDownload, disabled: !canLocalTransfer, onSelect: () => downloadLocal(entry.key) }] : []),
      { kind: "separator" }, copy,
    ];
  }

  function shortcuts(event: KeyboardEvent<HTMLElement>) {
    // 로컬 창의 키 입력은 S3 위치를 바꾸지 않는다.
    if (!event.altKey || event.ctrlKey || event.metaKey || (event.target as Element).closest?.(".s3-local")) return;
    const action = { ArrowLeft: goBack, ArrowRight: goForward, ArrowUp: goUp }[event.key];
    if (!action) return;
    event.preventDefault();
    action();
  }

  function resize(event: ReactPointerEvent<HTMLDivElement>, width = treeWidth, setWidth = setTreeWidth, min = 160, max = 480) {
    const handle = event.currentTarget;
    const start = event.clientX - width;
    handle.setPointerCapture?.(event.pointerId);
    const moveTo = (move: PointerEvent) => setWidth(Math.max(min, Math.min(max, move.clientX - start)));
    const stop = () => {
      handle.removeEventListener("pointermove", moveTo);
      handle.removeEventListener("pointerup", stop);
      handle.removeEventListener("pointercancel", stop);
    };
    handle.addEventListener("pointermove", moveTo);
    handle.addEventListener("pointerup", stop);
    handle.addEventListener("pointercancel", stop);
  }

  const treeBuckets: BucketRow[] = fixedLocation ? [{ name: fixedLocation.bucket, region: fixedLocation.region }] : [
    ...(location && !view.buckets?.rows.some(({ name }) => name === location.bucket) ? [{ name: location.bucket, region: location.region }] : []),
    ...(view.buckets?.rows ?? []),
  ];
  const selectedRow = selected && entries.find(entry => sameEntry(entry, selected));
  const title = compatible ? "S3 호환 HTTPS" : "AWS S3";
  const fixedBucket = fixedLocation?.bucket;
  const showBanner = view.problem || view.editError || (expired && onRelogin);

  const acceptsLocal = (event: ReactDragEvent) => canLocalTransfer && !!location && event.dataTransfer.types.includes(LOCAL_DRAG_TYPE);

  return <section className="s3-explorer" aria-label={title} onKeyDown={shortcuts}>
    <div className="s3-split" ref={split} data-stacked={stacked ? "" : undefined} style={{ ["--local-width" as string]: `${localWidth}px` }}>
      <LocalPane sessionId={sessionId} view={view.local} actions={localActions} update={updateLocal} fileView={fileView}
        canTransfer={canTransfer && !!location} onUpload={uploadLocal} onDropObject={downloadLocal} />
      {!stacked && <div className="s3-resizer s3-split-resizer" role="separator" aria-orientation="vertical" aria-label="로컬 창 크기" tabIndex={0}
        aria-valuemin={220} aria-valuemax={960} aria-valuenow={localWidth} onPointerDown={event => resize(event, localWidth, setLocalWidth, 220, 960)}
        onKeyDown={event => {
          const delta = event.key === "ArrowLeft" ? -16 : event.key === "ArrowRight" ? 16 : 0;
          if (!delta) return;
          event.preventDefault();
          setLocalWidth(width => Math.max(220, Math.min(960, width + delta)));
        }} />}
      <div className="s3-remote">
        <div className="s3-toolbar" role="toolbar" aria-label="탐색 도구">
          <IconButton icon={ArrowLeft} label="뒤로" disabled={!view.back.length || !navigable} onClick={goBack} />
          <IconButton icon={ArrowRight} label="앞으로" disabled={!view.forward.length || !navigable} onClick={goForward} />
          <IconButton icon={ArrowUp} label="상위 폴더" disabled={parent === null || !navigable} onClick={goUp} />
          <IconButton icon={RefreshCw} label="새로고침" className={view.loading ? "spinning" : undefined} disabled={!location || !navigable} onClick={reload} />
          <span className="s3-toolbar-sep" role="separator" aria-orientation="vertical" />
          <span className="s3-popover-anchor" ref={uploadAnchor}>
            <IconButton icon={Upload} label="업로드" aria-expanded={view.uploadKey !== null} aria-haspopup="dialog" disabled={!canTransfer || !location}
              onClick={() => view.uploadKey === null ? update(() => ({ uploadKey: location?.prefix ?? "", invalidUploadPaste: false })) : closeUpload()} />
            {view.uploadKey !== null && location && <form className="s3-popover" role="dialog" aria-label="업로드" onKeyDown={event => {
              if (event.key !== "Escape") return;
              event.preventDefault();
              event.stopPropagation();
              closeUpload(true);
            }} onSubmit={event => {
              event.preventDefault();
              const key = view.uploadKey;
              if (!key || key.endsWith("/")) return;
              closeUpload();
              void transfer("upload", key);
            }}>
              <label className="ui-label">업로드 전체 키<input className="ui-field mono" autoFocus value={view.uploadKey} spellCheck={false}
                onFocus={event => event.currentTarget.setSelectionRange(event.currentTarget.value.length, event.currentTarget.value.length)}
                onChange={event => {
                  const uploadKey = event.target.value;
                  update(() => ({ uploadKey, invalidUploadPaste: false }));
                }} onPaste={event => {
                  if (!/[\r\n]/.test(event.clipboardData.getData("text/plain"))) return;
                  event.preventDefault();
                  update(() => ({ invalidUploadPaste: true }));
                }} onDrop={event => {
                  // 끌어 놓은 텍스트는 줄바꿈이 조용히 지워지므로 받지 않는다.
                  event.preventDefault();
                }} /></label>
              <div className="s3-popover-target"><Database aria-hidden="true" />{location.bucket}<span>{location.region}</span></div>
              {view.invalidUploadPaste && <p role="alert" className="s3-inline-error">줄바꿈이 포함된 키는 붙여넣을 수 없습니다</p>}
              <div className="s3-popover-actions">
                <button type="button" className="ui-button ghost" onClick={() => closeUpload(true)}>취소</button>
                <button type="submit" className="ui-button primary" disabled={!view.uploadKey || view.uploadKey.endsWith("/") || !canTransfer}><Upload aria-hidden="true" />파일 선택</button>
              </div>
            </form>}
          </span>
          <IconButton icon={Download} label="다운로드" disabled={selectedKey === null || !canTransfer} onClick={() => void transfer("download", selectedKey)} />
          <IconButton icon={HardDriveDownload} label="로컬 폴더로 다운로드" disabled={selectedKey === null || !canLocalTransfer} onClick={() => downloadLocal(selectedKey)} />
          <IconButton icon={FilePen} label="편집" disabled={!canEdit || !approvedRowId(selectedKey)} onClick={() => { if (selectedKey !== null) void edit(selectedKey); }} />
          <span className="s3-toolbar-sep" role="separator" aria-orientation="vertical" />
          <IconButton icon={List} label="목록 보기" aria-pressed={fileView === "list"} onClick={() => setFileView("list")} />
          <IconButton icon={LayoutGrid} label="아이콘 보기" aria-pressed={fileView === "icons"} onClick={() => setFileView("icons")} />
          <div className="s3-badges">
            <span className="s3-badge" title={compatible ? "별칭" : "SSO 프로필"}><KeyRound aria-hidden="true" />{session.profile}</span>
            {compatible && <span className="s3-badge" title="서비스 ID"><Server aria-hidden="true" />{compatible.serviceId}</span>}
            {compatible && <span className="s3-badge wide" title="HTTPS 출처"><Link aria-hidden="true" />{session.endpoint}</span>}
            {compatible && <span className="s3-badge" title="주소 방식">{compatible.addressing === "path" ? "경로" : "가상 호스트"}</span>}
            <span className="s3-badge" title={compatible ? "서명 리전" : "리전"}><Globe aria-hidden="true" />{session.region}</span>
          </div>
        </div>

        {editing ? <form className="s3-address" data-editing="" onSubmit={event => {
          event.preventDefault();
          if (input.bucket && input.region && navigable) open(input);
        }} onKeyDown={event => {
          if (event.key !== "Escape" || !listing) return;
          event.preventDefault();
          update(() => ({ editing: false, input: listing.location }));
        }}>
          <Database className="s3-address-icon" aria-hidden="true" />
          {fixedBucket !== undefined ? <span className="s3-address-fixed">{fixedBucket}</span> :
            <label className="s3-address-field bucket"><span className="sr-only">버킷</span><input className="ui-field mono" placeholder="버킷" value={input.bucket}
              spellCheck={false} disabled={!navigable} onChange={event => {
                const value = event.target.value;
                editAttempt.current = null;
                update(() => ({ input: { ...input, bucket: value }, confirmed: false }));
              }} /></label>}
          <span className="s3-address-sep" aria-hidden="true">/</span>
          <label className="s3-address-field"><span className="sr-only">접두어</span><input className="ui-field mono" placeholder="접두어" value={input.prefix}
            spellCheck={false} disabled={!navigable} onChange={event => {
              const value = event.target.value;
              editAttempt.current = null;
              update(() => ({ input: { ...input, prefix: value }, confirmed: false }));
            }} /></label>
          <span className="s3-chip" title={compatible ? "서명 리전" : "리전"}><MapPin aria-hidden="true" />{input.region}</span>
          <button type="submit" className="ui-button primary" disabled={!input.bucket || !input.region || !navigable}>{fixedLocation ? "접두어 열기" : "버킷 열기"}</button>
          {listing && <IconButton icon={X} label="편집 취소" onClick={() => update(() => ({ editing: false, input: listing.location }))} />}
        </form> : <div className="s3-address" onClick={event => {
          if (event.target === event.currentTarget) update(() => ({ editing: true }));
        }}>
          <nav aria-label="위치" className="s3-crumbs">
            <ol>
              <li><button type="button" className="s3-crumb bucket" title={input.bucket} aria-current={input.prefix === "" ? "location" : undefined}
                disabled={!navigable} onClick={() => open({ ...input, prefix: "" })}><Database aria-hidden="true" />{input.bucket}</button></li>
              {prefixSegments(input.prefix).map(segment => <li key={segment.prefix}>
                <span className="s3-crumb-sep" aria-hidden="true">/</span>
                <button type="button" className="s3-crumb" title={segment.prefix} aria-current={segment.prefix === input.prefix ? "location" : undefined}
                  disabled={!navigable} onClick={() => open({ ...input, prefix: segment.prefix })}>{segment.label || "/"}</button>
              </li>)}
            </ol>
          </nav>
          <span className="s3-chip" title={compatible ? "서명 리전" : "리전"}><MapPin aria-hidden="true" />{input.region}</span>
          <IconButton icon={TextCursorInput} label="주소 편집" disabled={!navigable} onClick={() => update(() => ({ editing: true }))} />
        </div>}

        {showBanner && <div className="s3-banner">
          {view.problem && <StorageProblem problem={view.problem} session={session} />}
          {view.editError && <p role="alert" className="storage-problem">파일 열기 실패: {view.editError}</p>}
          {!view.problem && !view.editError && expired && <p className="storage-problem">SSO 로그인 만료</p>}
          <div className="s3-banner-actions">
            {detectedRegion && <button type="button" className="ui-button" disabled={!navigable} onClick={() => {
              editAttempt.current = null;
              update(() => ({ input: { ...input, region: detectedRegion }, confirmed: false, problem: undefined, editing: true }));
            }}><MapPin aria-hidden="true" />{detectedRegion} 선택</button>}
            {expired && onRelogin && <button type="button" className="ui-button primary" disabled={!navigable} onClick={onRelogin}>
              <LogIn aria-hidden="true" />SSO 재로그인</button>}
            {(view.problem || view.editError) && <IconButton small icon={X} label="오류 닫기"
              onClick={() => update(() => ({ problem: undefined, editError: undefined }))} />}
          </div>
        </div>}

        <div className="s3-body">
          <div className="s3-tree-pane" style={{ width: treeWidth }}>
            <S3Tree buckets={treeBuckets} current={location ?? undefined} tree={view.tree} disabled={!navigable}
              canList={!fixedLocation} loadMore={!fixedLocation && !!view.buckets && view.buckets.nextToken !== null}
              onOpen={target => open(target)} onListBuckets={() => void load(null)}
              onLoadMore={() => void load(null, { token: view.buckets?.nextToken ?? null })} />
          </div>
          <div className="s3-resizer" role="separator" aria-orientation="vertical" aria-label="폴더 창 크기" tabIndex={0}
            aria-valuemin={160} aria-valuemax={480} aria-valuenow={treeWidth} onPointerDown={resize}
            onKeyDown={event => {
              const delta = event.key === "ArrowLeft" ? -16 : event.key === "ArrowRight" ? 16 : 0;
              if (!delta) return;
              event.preventDefault();
              setTreeWidth(width => Math.max(160, Math.min(480, width + delta)));
            }} />
          <div className="s3-files" data-drop={dropping ? "" : undefined}
            onDragOver={event => {
              if (!acceptsLocal(event)) return;
              event.preventDefault();
              event.dataTransfer.dropEffect = "copy";
              setDropping(true);
            }}
            onDragLeave={event => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDropping(false); }}
            onDrop={event => {
              setDropping(false);
              if (!acceptsLocal(event)) return;
              event.preventDefault();
              const data = readDrag<LocalDrag>(event, LOCAL_DRAG_TYPE);
              if (data?.sessionId !== sessionId || data.rootId !== localRoot?.rootId || typeof data.path !== "string" || typeof data.name !== "string") return;
              uploadLocal(data.path, data.name, typeof data.bytes === "number" ? data.bytes : null);
            }}>
            {listing ? <FileBrowser label="객체 목록" entries={entries} selected={selected} view={fileView} busy={view.loading}
              columns={["size", "modified", "class"]} empty="빈 폴더"
              onSelect={entry => update(() => ({ selected: { kind: entry.kind, key: entry.key } }))}
              onOpen={openEntry} onParent={parent === null ? undefined : goUp} menuItems={menuItems}
              onDragStart={(entry, event) => {
                if (entry.kind !== "file") {
                  event.preventDefault();
                  return;
                }
                const data: S3Drag = { sessionId, key: entry.key };
                event.dataTransfer.setData(S3_DRAG_TYPE, JSON.stringify(data));
                event.dataTransfer.effectAllowed = "copy";
              }}
              footer={listing.nextToken !== null && <button type="button" className="ui-button ghost"
                disabled={view.loading || transferring || !view.confirmed} onClick={() => void load(listing.location, { token: listing.nextToken })}>더 불러오기</button>} />
              : <p className="s3-placeholder">{view.loading ? "목록 요청 중" : "열린 버킷 없음"}</p>}
          </div>
        </div>
      </div>
    </div>

    <TransferQueue transfers={view.transfers} session={session} open={queueOpen} onToggle={() => setQueueOpen(value => !value)} />

    <footer className="s3-status">
      {listing && <span>{listing.rows.length}개 항목{listing.nextToken !== null ? " 이상" : ""}</span>}
      {selectedRow && <span title={selectedRow.title}>선택 1개{selectedRow.kind === "file" && selectedRow.bytes !== null &&
        <> · <span title={exactBytes(selectedRow.bytes)}>{formatBytes(selectedRow.bytes)}</span></>}</span>}
      {versionedFiles && <VersionedFiles status={versionedFiles} />}
      {view.loading && <span role="status" className="s3-status-busy">목록 요청 중</span>}
    </footer>
  </section>;
}
