import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { VersionedFilesStatus } from "../files/FileBrowser";
import { ObjectTable } from "./ObjectTable";
import type { BucketRow, ObjectRow, S3Actions, S3FileSelection, S3Location, S3Session, StorageEditorOpen } from "./s3";
import { StorageProblem, TransferStatus, type Problem, type Transfer } from "./StorageStatus";
import type { CompatibleBucketPage } from "./compatible-settings";
import { bindStorageEditor } from "./storageEditorIpc";
import { countPreviousFiles } from "./storageIpc";
import "./storage.css";

type Listing = { nextToken: string | null } & (
  | { kind: "buckets"; rows: readonly BucketRow[] }
  | { kind: "objects"; location: S3Location; rows: readonly ObjectRow[] }
);
type View = {
  input?: S3Location;
  confirmed: boolean;
  listing?: Listing;
  loading: boolean;
  problem?: Problem;
  editError?: string;
  selectedKey: string | null;
  uploadKey: string;
  transfer?: Transfer;
  invalidUploadPaste?: boolean;
  requestedLocation?: S3Location | null;
  expired?: boolean;
};
const initial: View = { confirmed: false, loading: false, selectedKey: null, uploadKey: "" };

type Props = Readonly<{
  session: S3Session;
  initialBuckets?: CompatibleBucketPage;
  actions: S3Actions;
  onRelogin?: () => void;
  refresh?: number;
  editEnabled?: boolean;
  onEditFile?(opened: StorageEditorOpen): false | void;
  countHistory?: typeof countPreviousFiles;
}>;

export function S3StoragePanel({ session, initialBuckets, actions, onRelogin, refresh = 0, editEnabled = true, onEditFile, countHistory = countPreviousFiles }: Props) {
  const [views, setViews] = useState(() => {
    const initialViews = new Map<string, View>();
    if (initialBuckets) initialViews.set(session.sessionId, { ...initial, listing: { kind: "buckets", ...initialBuckets } });
    return initialViews;
  });
  const [history, setHistory] = useState<{ request: Parameters<typeof countPreviousFiles>[0]; count: typeof countPreviousFiles; status: VersionedFilesStatus }>();
  const [menuKey, setMenuKey] = useState<string>();
  const editAttempt = useRef<{ sessionId: string } | null>(null);
  const editListener = useRef(onEditFile);
  useLayoutEffect(() => { editListener.current = onEditFile; }, [onEditFile]);
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

  async function load(location: S3Location | null, token: string | null = null, afterRelogin = false) {
    if (view.loading) return;
    editAttempt.current = null;
    update(current => ({
      loading: true, requestedLocation: location, problem: undefined, editError: undefined,
      ...(afterRelogin ? {
        expired: false, confirmed: false,
        ...(current.transfer?.state === "failed" && current.transfer.problem !== "unavailable" &&
          current.transfer.problem.failure.kind === "ssoLoginRequired" ? { transfer: undefined } : {}),
      } : {}),
    }));
    try {
      let listing: Listing;
      if (location) {
        const result = await actions.listObjects({ sessionId, ...location, token });
        if (!result.ok) {
          update(() => ({ loading: false, problem: result.error, ...(result.error.failure.kind === "ssoLoginRequired" ? { expired: true } : {}) }));
          return;
        }
        const previous = view.listing;
        const rows = token !== null && previous?.kind === "objects" ? [...previous.rows, ...result.value.rows] : result.value.rows;
        listing = { kind: "objects", location, rows, nextToken: result.value.nextToken };
      } else {
        const result = await actions.listBuckets({ sessionId, token });
        if (!result.ok) {
          update(() => ({ loading: false, problem: result.error, ...(result.error.failure.kind === "ssoLoginRequired" ? { expired: true } : {}) }));
          return;
        }
        const previous = view.listing;
        const rows = token !== null && previous?.kind === "buckets" ? [...previous.rows, ...result.value.rows] : result.value.rows;
        listing = { kind: "buckets", rows, nextToken: result.value.nextToken };
      }
      update(current => {
        const previous = current.listing;
        const changed = listing.kind === "objects" && (previous?.kind !== "objects" ||
          previous.location.bucket !== listing.location.bucket || previous.location.prefix !== listing.location.prefix ||
          previous.location.region !== listing.location.region);
        return {
          loading: false,
          listing,
          ...(token === null ? { confirmed: listing.kind === "objects" } : {}),
          ...(changed ? { selectedKey: null } : {}),
        };
      });
    } catch {
      update(() => ({ loading: false, problem: "unavailable" }));
    }
  }

  function open(location: S3Location) {
    update(() => ({ input: location, confirmed: false }));
    void load(location);
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
    void load(view.requestedLocation ?? null, null, true);
  });

  const listing = view.listing;
  const location = listing?.kind === "objects" ? listing.location : null;
  const transferring = view.transfer?.state === "selecting" || view.transfer?.state === "running";
  const expired = !compatible && (view.expired || (view.transfer?.state === "failed" && view.transfer.problem !== "unavailable" && view.transfer.problem.failure.kind === "ssoLoginRequired"));
  const detectedRegion = !compatible && view.problem && view.problem !== "unavailable" && view.problem.failure.kind === "bucketRegionMismatch" ? view.problem.failure.region : null;
  const canTransfer = view.confirmed && !view.loading && !transferring && !expired;
  const selectedKey = listing?.kind === "objects" && listing.rows.some(row => row.kind === "object" && row.key === view.selectedKey)
    ? view.selectedKey : null;
  const canEdit = !compatible && editEnabled && !!onEditFile && view.confirmed && !view.loading && !expired;
  useLayoutEffect(() => () => { editAttempt.current = null; }, [sessionId, canEdit]);

  function approvedRowId(key: string | undefined) {
    const row = listing?.kind === "objects" ? listing.rows.find(row => row.kind === "object" && row.key === key) : undefined;
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
  const historyListing = !compatible && view.confirmed && !view.loading && !view.problem && !expired && listing?.kind === "objects" ? listing : undefined;
  const historyRequest = useMemo(() => historyListing ? { sessionId, ...historyListing.location } : undefined,
    [historyListing, sessionId]);

  useEffect(() => {
    if (!historyRequest) return;
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
  }, [countHistory, historyRequest]);

  const versionedFiles: VersionedFilesStatus | undefined = historyRequest
    ? history?.request === historyRequest && history.count === countHistory ? history.status : { kind: "loading" }
    : undefined;

  async function transfer(direction: S3FileSelection["direction"], key: string | null) {
    if (!canTransfer || !location || key === null || key === "") return;
    const target = { sessionId, bucket: location.bucket, region: location.region, key };
    const display = { direction, bucket: location.bucket, region: location.region, key };
    update(() => ({ transfer: { ...display, state: "selecting" } }));
    try {
      const selection = await actions.selectLocalFile({ ...target, direction });
      if (selection.kind !== "approved") {
        update(() => ({ transfer: { ...display, state: selection.kind } }));
        return;
      }
      const request = { ...target, approvalId: selection.approvalId };
      update(() => ({ transfer: { ...display, state: "running" } }));
      if (direction === "upload") {
        const result = await actions.upload(request);
        if (!result.ok && result.error.failure.kind === "ssoLoginRequired" && editAttempt.current?.sessionId === sessionId) {
          editAttempt.current = null;
        }
        update(() => ({
          transfer: result.ok
            ? { ...display, state: "uploaded", bytes: result.value }
            : { ...display, state: "failed", problem: result.error }
        }));
      } else {
        const result = await actions.download(request);
        if (!result.ok && result.error.failure.kind === "ssoLoginRequired" && editAttempt.current?.sessionId === sessionId) {
          editAttempt.current = null;
        }
        update(() => ({
          transfer: result.ok
            ? { ...display, state: "downloaded", result: result.value }
            : { ...display, state: "failed", problem: result.error }
        }));
      }
    } catch {
      update(() => ({ transfer: { ...display, state: "failed", problem: "unavailable" } }));
    }
  }

  const title = compatible ? "S3 호환 HTTPS" : "AWS S3";
  return <section className="s3-storage" aria-label={title}>
    <header className="storage-heading">
      <h2>{title}</h2><span>{session.profile} · {session.region}{compatible &&
        ` · ${compatible.serviceId} · ${session.endpoint} · ${compatible.addressing === "path" ? "경로" : "가상 호스트"}`}</span>
    </header>
    <form className="storage-controls" onSubmit={event => { event.preventDefault(); if (input.bucket && input.region && !view.loading && !transferring) open(input); }}>
      {([["버킷", "bucket"], ["접두어", "prefix"], ["리전", "region"]] as const).map(([label, field]) =>
        <label key={field}>{field === "region" && compatible ? "서명 리전" : label}<input value={input[field]} readOnly={field === "region" && !!compatible} disabled={view.loading || transferring} onChange={event => {
          if (field === "region" && compatible) return;
          const value = event.target.value;
          editAttempt.current = null;
          update(() => ({ input: { ...input, [field]: value }, confirmed: false }));
        }} /></label>)}
      <button type="submit" disabled={!input.bucket || !input.region || view.loading || transferring}>버킷 열기</button>
      <button type="button" disabled={view.loading || transferring} onClick={() => void load(null)}>버킷 목록</button>
    </form>
    {view.problem && <StorageProblem problem={view.problem} session={session} />}
    {view.editError && <p role="alert">파일 열기 실패: {view.editError}</p>}
    {detectedRegion && <button type="button" disabled={view.loading || transferring} onClick={() => {
      editAttempt.current = null;
      update(() => ({ input: { ...input, region: detectedRegion }, confirmed: false, problem: undefined }));
    }}>{detectedRegion} 선택</button>}
    {view.loading && <p role="status">목록 요청 중</p>}
    {listing?.kind === "buckets" && <ul className="storage-buckets" aria-label="버킷 목록">
      {listing.rows.map(({ name, region }) => <li key={name}>
        <button type="button" disabled={view.loading || transferring} onClick={() => open({ bucket: name, prefix: "", region: compatible ? session.region : region })}>{name}</button> <span>{region}</span>
      </li>)}
    </ul>}
    {listing?.kind === "objects" && <>
      <div className="storage-location"><strong>{listing.location.bucket}</strong> / <code>{listing.location.prefix}</code> · {listing.location.region}</div>
      {/* ObjectTable의 공통 메뉴는 항목별 콜백 대신 현재 메뉴의 편집 가능 여부를 받는다. */}
      <div onContextMenuCapture={event => {
        const target = event.target;
        if (!(target instanceof Element)) return;
        const row = target.closest("tr, li");
        if (row) setMenuKey(row.querySelector<HTMLButtonElement>(".file-browser-entry")?.title);
      }} onKeyDownCapture={event => {
        if (event.key !== "ContextMenu" && !(event.key === "F10" && event.shiftKey)) return;
        const target = event.target;
        if (target instanceof HTMLButtonElement && target.matches(".file-browser-entry")) setMenuKey(target.title);
      }}>
        <ObjectTable rows={listing.rows} selectedKey={selectedKey} versionedFiles={versionedFiles} onOpenPrefix={prefix => {
          if (!view.loading && !transferring) open({ ...listing.location, prefix });
        }} onSelectObject={key => update(() => ({ selectedKey: key }))}
          onEditObject={canEdit && approvedRowId(menuKey) ? key => void edit(key) : undefined} />
      </div>
    </>}
    {listing?.rows.length === 0 && <p>목록이 비어 있습니다.</p>}
    {listing && listing.nextToken !== null && <button type="button" disabled={view.loading || transferring || (!!location && !view.confirmed)} onClick={() => void load(location, listing.nextToken)}>다음 페이지</button>}
    {location && <form className="storage-controls" onSubmit={event => { event.preventDefault(); void transfer("upload", view.uploadKey); }}>
      <label>업로드 전체 키<input value={view.uploadKey} onChange={event => {
        const uploadKey = event.target.value;
        update(() => ({ uploadKey, invalidUploadPaste: false }));
      }} onPaste={event => {
        if (!/[\r\n]/.test(event.clipboardData.getData("text/plain"))) return;
        event.preventDefault();
        update(() => ({ invalidUploadPaste: true }));
      }} /></label>
      <button type="submit" disabled={!view.uploadKey || !canTransfer}>업로드</button>
      <button type="button" disabled={selectedKey === null || !canTransfer} onClick={() => void transfer("download", selectedKey)}>다운로드</button>
    </form>}
    {view.invalidUploadPaste && <p role="alert">줄바꿈이 포함된 키는 붙여넣을 수 없습니다</p>}
    {view.transfer && <TransferStatus session={session} transfer={view.transfer} />}
    {expired && onRelogin && <button type="button" disabled={view.loading || transferring} onClick={onRelogin}>SSO 재로그인</button>}
  </section>;
}
