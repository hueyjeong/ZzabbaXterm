import { useLayoutEffect, useMemo, useRef, useState, type FormEvent } from "react";
import type { ConnectionEditError, GroupDraft, SavedConnections } from "./connectionIpc";
import { Field } from "./fields/Field";
import { groupDepth, groupOptions, MAX_GROUP_DEPTH, subtreeHeight } from "./groupTree";
import { editError } from "./sessionEditorState";
import "./editor.css";

/** parentId만 있으면 그 아래 새 그룹, group이 있으면 저장된 그룹의 이름 바꾸기와 이동이다. */
export type GroupRequest = Readonly<{ parentId?: string; group?: never }>
  | Readonly<{ group: SavedConnections["groups"][number]; parentId?: never }>;

export type GroupDialogProps = Readonly<{
  request: GroupRequest;
  groups: SavedConnections["groups"];
  save(draft: GroupDraft): Promise<unknown>;
  onClose(saved: boolean): void;
  /** 저장 충돌 뒤 목록을 다시 읽어 그룹의 현재 개정을 돌려준다. 없으면 undefined다. */
  latestRevision?(groupId: string): Promise<number | undefined>;
}>;

const DEPTH_TEXT = `최대 ${MAX_GROUP_DEPTH}단계 초과`;

/** 그룹 추가·편집 창이다. 이름과 상위 그룹(최상위 또는 그룹, 자기 자신과 하위 제외)을 고른다. */
export function GroupDialog({ request, groups, save, onClose, latestRevision }: GroupDialogProps) {
  const existing = request.group;
  const [id] = useState(() => existing?.profile.id ?? crypto.randomUUID());
  const [revision, setRevision] = useState(existing?.revision ?? null);
  const [name, setName] = useState(existing?.profile.displayName ?? "");
  const [parentId, setParentId] = useState(existing ? existing.profile.parentId : request.parentId);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<{ field?: "displayName" | "parentId"; error: string }>();
  const options = useMemo(() => groupOptions(groups, existing ? id : undefined), [groups, existing, id]);
  const dialog = useRef<HTMLDialogElement>(null);
  const nameInput = useRef<HTMLInputElement>(null);
  useLayoutEffect(() => {
    const modal = dialog.current;
    modal?.showModal();
    nameInput.current?.focus();
    return () => modal?.close();
  }, []);

  const depth = (parentId ? groupDepth(groups, parentId) : 0) + (existing ? subtreeHeight(groups, id) : 1);
  const parentError = depth > MAX_GROUP_DEPTH ? DEPTH_TEXT : failure?.field === "parentId" ? failure.error : undefined;
  const canSave = !busy && !!name.trim() && depth <= MAX_GROUP_DEPTH;
  const title = existing ? "그룹 편집" : "그룹 추가";

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!canSave) return;
    setBusy(true);
    setFailure(undefined);
    try {
      await save({
        version: 2, revision,
        group: { id, displayName: name.trim(), ...(parentId ? { parentId } : {}) },
      });
    } catch (error) {
      const edit = error as Partial<ConnectionEditError> | null;
      const field = edit?.kind === "invalid" ? edit.field : undefined;
      // 충돌이면 다음 저장이 저장본의 새 개정으로 나가도록 다시 읽는다.
      if (edit?.kind === "conflict" && revision !== null && latestRevision) {
        const latest = await latestRevision(id).catch(() => undefined);
        if (latest !== undefined) setRevision(latest);
      }
      setFailure(field === "displayName" ? { field, error: "이름 형식 오류" }
        : field === "parentId" ? { field, error: "상위 그룹 오류" }
          : field ? { error: "입력 오류" }
            : edit?.kind === "conflict" ? { error: "다른 작업이 그룹을 변경했습니다" } : { error: editError(error).error });
      setBusy(false);
      return;
    }
    onClose(true);
  }

  return <dialog ref={dialog} className="connection-wizard group-dialog" aria-label={title} onCancel={event => {
    event.preventDefault();
    if (!busy) onClose(false);
  }}>
    <form onSubmit={event => void submit(event)}>
      <h2>{title}</h2>
      <fieldset className="group-dialog-body" disabled={busy}>
        <Field label="이름" error={failure?.field === "displayName" ? failure.error : undefined}>
          <input ref={nameInput} value={name} onChange={event => {
            setName(event.target.value);
            if (failure?.field === "displayName") setFailure(undefined);
          }} />
        </Field>
        <Field label="상위 그룹" error={parentError}>
          <select value={parentId ?? ""} onChange={event => {
            setParentId(event.currentTarget.value || undefined);
            if (failure?.field === "parentId") setFailure(undefined);
          }}>
            <option value="">최상위</option>
            {options.map(option => <option key={option.id} value={option.id}>{option.label}</option>)}
          </select>
        </Field>
      </fieldset>
      {failure && !failure.field && <div role="alert">{failure.error}</div>}
      <footer>
        <button type="submit" disabled={!canSave}>저장</button>
        <button type="button" disabled={busy} onClick={() => onClose(false)}>취소</button>
      </footer>
    </form>
  </dialog>;
}
