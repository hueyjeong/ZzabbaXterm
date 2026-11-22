import { useImperativeHandle, useLayoutEffect, useRef, useState, type Ref } from "react";
import type { ApprovedEditorFile, EditorTransport } from "../editor/contract";
import { FileBrowser } from "../files/FileBrowser";
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
  const [directory, setDirectory] = useState<{ listing: SftpListing; names: readonly string[] }>();
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
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
      if (!current.stopped && listing) setDirectory({ listing, names: [] });
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

  async function navigate(entryId: string | null, names: readonly string[]) {
    const current = attempt.current;
    if (!current || current.stopped || current.busy || closing) return;
    current.busy = true;
    setPending("listing");
    setError("");
    try {
      const listing = await current.io.list(entryId);
      if (current.stopped) return;
      setDirectory({ listing, names });
      setSelectedKey(null);
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

  return <section className="sftp-panel" aria-label="SFTP">
    {error && !closed && <p role="alert">{error}</p>}
    {!closed && pending && <p role="status">{pending === "opening" ? "SFTP 연결 중" : pending === "listing" ? "폴더 불러오는 중" : "파일 여는 중"}</p>}
    {directory && <div inert={closing || closed} aria-busy={pending !== null && !closed}>
      {directory.names.length > 0 && <div className="sftp-location">
        <button type="button" disabled={pending !== null} onClick={() => void navigate(directory.listing.parentEntryId, directory.names.slice(0, -1))}>상위 폴더</button>
        <span>{directory.names.join(" / ")}</span>
      </div>}
      <FileBrowser label="SFTP 파일 목록" entries={directory.listing.entries} selectedKey={selectedKey}
        onOpenFolder={key => {
          const folder = directory.listing.entries.find(entry => entry.kind === "folder" && entry.key === key);
          if (folder) void navigate(key, [...directory.names, folder.name]);
        }} onSelectFile={setSelectedKey} onEditFile={key => void edit(key)} versionedFiles={{ kind: "unsupported" }} />
    </div>}
  </section>;
}
