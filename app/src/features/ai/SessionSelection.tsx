import { useLayoutEffect, useRef, useState } from "react";

export type SessionSelectionCandidate = Readonly<{ id: string; label: string; region: string; node: string }>;
export type SessionSelectionSnapshot = Readonly<Omit<SessionSelectionCandidate, "label"> & {
  selectionId: string;
  confirmed: boolean;
}>;
export type SessionSelectionProps = Readonly<{
  candidates: readonly SessionSelectionCandidate[];
  visible?: boolean;
  actions: Readonly<{
    select(id: string): Promise<SessionSelectionSnapshot>;
    confirm(id: string, selectionId: string): Promise<SessionSelectionSnapshot>;
    clear(selectionId: string): Promise<null>;
  }>;
}>;

type State =
  | { kind: "empty"; error?: string }
  | { kind: "selecting"; candidate: SessionSelectionCandidate; closed: boolean }
  | { kind: "selected"; selection: SessionSelectionSnapshot; error?: string }
  | { kind: "confirming"; selection: SessionSelectionSnapshot }
  | { kind: "clearing" };

function sameSession(a: Omit<SessionSelectionCandidate, "label">, b: Omit<SessionSelectionCandidate, "label">) {
  return a.id === b.id && a.region === b.region && a.node === b.node;
}

export function SessionSelection({ candidates, visible = true, actions }: SessionSelectionProps) {
  const [state, setState] = useState<State>({ kind: "empty" });
  const live = useRef<State>(state);
  const mounted = useRef(false);
  const identity = state.kind === "selecting" ? state.candidate : "selection" in state ? state.selection : undefined;
  const candidate = identity && candidates.find((item) => sameSession(item, identity));
  const selected = candidate && "selection" in state ? state.selection : undefined;

  function update(next: State) {
    live.current = next;
    if (mounted.current) setState(next);
  }

  async function clear(selectionId: string, error?: string) {
    update({ kind: "clearing" });
    try {
      await actions.clear(selectionId);
      update({ kind: "empty", error });
    } catch {
      update({ kind: "empty", error: "세션 선택 해제 실패" });
    }
  }

  useLayoutEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  useLayoutEffect(() => {
    const current = live.current;
    const target = current.kind === "selecting" ? current.candidate : "selection" in current ? current.selection : undefined;
    const closed = target && !candidates.some((item) => sameSession(item, target));
    if (closed && current.kind === "selecting" && !current.closed) {
      update({ ...current, closed: true });
    } else if ("selection" in current && (closed || (!visible && current.kind === "confirming"))) {
      // 확인 응답을 버릴 때 서버에 남을 수 있는 같은 선택도 해제한다.
      void clear(current.selection.selectionId);
    }
  });

  async function select(id: string) {
    const previous = live.current;
    if (!visible || previous.kind === "selecting" || previous.kind === "clearing") return;
    if (!id) {
      if ("selection" in previous) await clear(previous.selection.selectionId);
      return;
    }
    const next = candidates.find((item) => item.id === id);
    if (!next) return;
    update({ kind: "selecting", candidate: next, closed: false });
    let result: SessionSelectionSnapshot;
    try {
      result = await actions.select(next.id);
    } catch {
      if ("selection" in previous) await clear(previous.selection.selectionId, "세션 선택 실패");
      else update({ kind: "empty", error: "세션 선택 실패" });
      return;
    }
    const current = live.current;
    const closed = current.kind === "selecting" && current.closed;
    if (result.id !== id) {
      update({ kind: "empty", error: "세션 선택 실패" });
    } else if (!mounted.current || closed || !sameSession(result, next) || result.confirmed) {
      await clear(result.selectionId, closed ? undefined : "세션 선택 실패");
    } else {
      update({ kind: "selected", selection: result });
    }
  }

  async function confirm() {
    const current = live.current;
    if (!visible || !candidate || current.kind !== "selected" || current.selection.confirmed ||
      !sameSession(candidate, current.selection)) return;
    const { selection } = current;
    const request: State = { kind: "confirming", selection };
    update(request);
    try {
      const result = await actions.confirm(selection.id, selection.selectionId);
      if (!mounted.current || live.current !== request) return;
      if (result.selectionId === selection.selectionId && sameSession(result, selection) && result.confirmed) {
        update({ kind: "selected", selection: result });
        return;
      }
    } catch {
      // 원본 오류 대신 현재 확인 동작의 실패만 표시한다.
    }
    if (mounted.current && live.current === request) update({ kind: "selected", selection, error: "세션 연결 확인 실패" });
  }

  return <div hidden={!visible}>
    <label className="a01-field">AI 작업 세션
      <select value={candidate && !(state.kind === "selecting" && state.closed) ? candidate.id : ""}
        disabled={!candidates.length || state.kind === "selecting" || state.kind === "clearing"}
        onChange={(event) => void select(event.currentTarget.value)}>
        <option value="">세션 선택</option>
        {candidates.map(({ id, label }) => <option key={id} value={id}>
          {label.replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, "�")} (세션 {id})
        </option>)}
      </select>
    </label>
    {!candidates.length && <p role="status">선택할 SSM 세션 없음</p>}
    {selected && <>
      <p>{selected.region} / {selected.node}</p>
      <div className="a01-actions">
        {!selected.confirmed && <button type="button" disabled={state.kind === "confirming"}
          onClick={() => void confirm()}>연결 확인</button>}
        <button type="button" onClick={() => void select("")}>선택 해제</button>
      </div>
      {state.kind === "confirming" && <p role="status">연결 확인 중</p>}
      {selected.confirmed && <p role="status">연결 확인됨</p>}
    </>}
    {"error" in state && state.error && <p role="alert">{state.error}</p>}
  </div>;
}
