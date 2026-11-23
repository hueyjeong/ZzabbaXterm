import { useImperativeHandle, useLayoutEffect, useRef, useState, type Ref } from "react";
import { ArrowUp, FilePen, FolderOpen, House, RefreshCw } from "lucide-react";
import type { ApprovedEditorFile, EditorTransport } from "../editor/contract";
import { FileBrowser, sameEntry, type EntryRef, type FileEntry } from "../files/FileBrowser";
import { IconButton } from "../ui/controls";
import { sftpError, sftpSession, type SftpListing, type SftpSession } from "./sftpIpc";
import "./sftp.css";

export interface SftpPanelActions {
  confirmClose(): Promise<void>;
}

type Props = {
  readonly sshSessionId: string;
  readonly closing?: boolean;
  readonly closeRef?: Ref<SftpPanelActions>;
  readonly onEditFile: (file: ApprovedEditorFile, transport: EditorTransport, close: () => Promise<void>) => void;
};

type Crumb = Readonly<{ id: string; name: string }>;

type Attempt = {
  io: SftpSession;
  started: boolean;
  stopped: boolean;
  busy: boolean;
  opened: Promise<SftpListing | undefined>;
  release?: Promise<void>;
};

function stop(attempt: Attempt) {
  attempt.stopped = true;
  return attempt.release ??= attempt.opened.catch(() => undefined).then(async () => {
    if (attempt.started) await attempt.io.detach();
  }).catch((error: unknown) => {
    attempt.release = undefined;
    throw error;
  });
}

export function SftpPanel(props: Props) {
  return <SftpSessionPanel key={props.sshSessionId} {...props} />;
}

function SftpSessionPanel({ sshSessionId, closing, closeRef, onEditFile }: Props) {
  const attempt = useRef<Attempt | null>(null);
  const editListener = useRef(onEditFile);
  useLayoutEffect(() => { editListener.current = onEditFile; }, [onEditFile]);
  const [directory, setDirectory] = useState<{ listing: SftpListing; path: readonly Crumb[] }>();
  const [selected, setSelected] = useState<EntryRef | null>(null);
  const [pending, setPending] = useState<"opening" | "listing" | "editing" | null>("opening");
  const [closed, setClosed] = useState(false);
  const [error, setError] = useState("");

  useLayoutEffect(() => {
    const io = sftpSession(sshSessionId);
    const current: Attempt = {
      io, started: false, stopped: false, busy: false,
      opened: Promise.resolve().then(() => {
        // StrictMode에서 즉시 해제된 첫 마운트는 연결을 열지 않는다.
        if (current.stopped) return;
        current.started = true;
        return io.attach();
      }),
    };
    attempt.current = current;
    void current.opened.then((listing) => {
      if (!current.stopped && listing) setDirectory({ listing, path: [] });
    }, (error: unknown) => {
      if (!current.stopped) setError(`SFTP 연결 실패: ${sftpError(error).message}`);
    }).finally(() => {
      if (!current.stopped) setPending(null);
    });
    return () => {
      attempt.current = null;
      void stop(current).catch(() => { /* SSH 종료 시 백엔드가 세션을 정리한다. */ });
    };
  }, [sshSessionId]);

  useImperativeHandle(closeRef, () => ({
    async confirmClose() {
      const current = attempt.current;
      if (!current) return;
      setClosed(true);
      try {
        await stop(current);
      } catch (error) {
        throw new Error(sftpError(error).message);
      }
    },
  }), []);

  async function navigate(entryId: string | null, path: readonly Crumb[]) {
    const current = attempt.current;
    if (!current || current.stopped || current.busy || closing) return;
    current.busy = true;
    setPending("listing");
    setError("");
    try {
      const listing = await current.io.list(entryId);
      if (current.stopped) return;
      // 목록마다 항목 ID가 새로 발급되므로 이전 선택은 유지하지 않는다.
      setDirectory({ listing, path });
      setSelected(null);
    } catch (error) {
      if (!current.stopped) setError(`폴더 열기 실패: ${sftpError(error).message}`);
    } finally {
      current.busy = false;
      if (!current.stopped) setPending(null);
    }
  }

  async function edit(entryId: string) {
    const current = attempt.current;
    if (!current || current.stopped || current.busy || closing) return;
    current.busy = true;
    setPending("editing");
    setError("");
    try {
      const file = await current.io.open(entryId);
      if (!current.stopped) editListener.current(file, current.io.transport, () => current.io.close(file.handle));
    } catch (error) {
      if (!current.stopped) setError(`파일 열기 실패: ${sftpError(error).message}`);
    } finally {
      current.busy = false;
      if (!current.stopped) setPending(null);
    }
  }

  function open(entry: FileEntry) {
    if (!directory) return;
    if (entry.kind === "file") {
      void edit(entry.key);
      return;
    }
    const folder = directory.listing.entries.find(item => item.kind === "folder" && item.key === entry.key);
    if (folder) void navigate(folder.key, [...directory.path, { id: folder.key, name: folder.name }]);
  }

  const busy = pending !== null;
  const path = directory?.path ?? [];
  const current = selected && directory?.listing.entries.some(entry => sameEntry(entry, selected)) ? selected : null;
  return <section className="sftp-panel" aria-label="SFTP">
    <header className="sftp-bar">
      <nav className="sftp-path" aria-label="SFTP 경로" ref={node => { if (node) node.scrollLeft = node.scrollWidth; }}>
        <button type="button" className="sftp-crumb home" title="시작 폴더" aria-label="시작 폴더"
          aria-current={directory && !path.length ? "location" : undefined} disabled={busy || !directory || !path.length}
          onClick={() => void navigate(null, [])}><House aria-hidden="true" /></button>
        {path.map((crumb, index) => <span key={crumb.id} className="sftp-crumb-item">
          <span className="sftp-crumb-sep" aria-hidden="true">/</span>
          <button type="button" className="sftp-crumb" title={crumb.name} aria-current={index === path.length - 1 ? "location" : undefined}
            disabled={busy || index === path.length - 1} onClick={() => void navigate(crumb.id, path.slice(0, index + 1))}>{crumb.name}</button>
        </span>)}
      </nav>
      <div className="sftp-actions">
        <IconButton small icon={ArrowUp} label="상위 폴더" disabled={busy || !directory || !path.length}
          onClick={() => directory && void navigate(directory.listing.parentEntryId, path.slice(0, -1))} />
        <IconButton small icon={RefreshCw} label="새로고침" className={pending === "listing" ? "spinning" : undefined} disabled={busy || !directory}
          onClick={() => void navigate(path[path.length - 1]?.id ?? null, path)} />
      </div>
    </header>
    {error && !closed && <p role="alert" className="sftp-line error">{error}</p>}
    {!closed && pending && <p role="status" className="sftp-line">{pending === "opening" ? "SFTP 연결 중" : pending === "listing" ? "폴더 불러오는 중" : "파일 여는 중"}</p>}
    {directory && <div className="sftp-files" inert={closing || closed} aria-busy={busy && !closed}>
      <FileBrowser compact label="SFTP 파일 목록" entries={directory.listing.entries} selected={current} busy={busy}
        onSelect={entry => setSelected({ kind: entry.kind, key: entry.key })} onOpen={open}
        onParent={path.length ? () => void navigate(directory.listing.parentEntryId, path.slice(0, -1)) : undefined}
        menuItems={entry => entry.kind === "folder"
          ? [{ label: "열기", icon: FolderOpen, disabled: busy, onSelect: () => open(entry) }]
          : [{ label: "편집", icon: FilePen, disabled: busy, onSelect: () => open(entry) }]} empty="빈 폴더" />
    </div>}
  </section>;
}
