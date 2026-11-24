import { useLayoutEffect, useRef, useState } from "react";
import { editError } from "./sessionEditorState";
import "./editor.css";

export type DeleteDialogProps = Readonly<{
  name: string;
  onDelete(): Promise<void>;
  onClose(): void;
  /** 참조 오류의 세션 ID를 이름으로 바꾼다. */
  nameOf?(id: string): string;
}>;

/** 삭제 확인 창이다. 대상 이름과 "삭제", "취소"만 둔다. */
export function DeleteDialog({ name, onDelete, onClose, nameOf = id => id }: DeleteDialogProps) {
  const dialog = useRef<HTMLDialogElement>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  useLayoutEffect(() => {
    const modal = dialog.current;
    modal?.showModal();
    // 실수로 지우지 않게 첫 포커스를 "취소"에 둔다.
    cancel.current?.focus();
    return () => modal?.close();
  }, []);

  async function remove() {
    setBusy(true);
    setError("");
    try {
      await onDelete();
    } catch (failure) {
      const referenced = failure as { kind?: string; targets?: readonly string[] } | null;
      setError(referenced?.kind === "referenced" && referenced.targets
        ? `참조 중인 세션: ${referenced.targets.map(nameOf).join(", ")}` : editError(failure).error);
      setBusy(false);
      return;
    }
    onClose();
  }

  return <dialog ref={dialog} className="connection-wizard delete-dialog" aria-label="삭제" onCancel={event => {
    event.preventDefault();
    if (!busy) onClose();
  }}>
    <h2>{name}</h2>
    {error && <div role="alert">{error}</div>}
    <footer>
      <button type="button" className="danger" disabled={busy} onClick={() => void remove()}>삭제</button>
      <button ref={cancel} type="button" disabled={busy} onClick={onClose}>취소</button>
    </footer>
  </dialog>;
}
