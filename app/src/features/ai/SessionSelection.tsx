import { useLayoutEffect, useRef, useState } from "react";
import { Check, ChevronDown, Cloud, PlugZap, X } from "lucide-react";
import { IconButton, type MenuItem } from "../ui/controls";
import { useContextMenu } from "../ui/useContextMenu";
import { AnchoredMenu, useAnchoredMenu } from "./AnchoredMenu";

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

function safeLabel(label: string) {
  return label.replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, "�");
}

function sameSession(a: Omit<SessionSelectionCandidate, "label">, b: Omit<SessionSelectionCandidate, "label">) {
  return a.id === b.id && a.region === b.region && a.node === b.node;
}

export function SessionSelection({ candidates, visible = true, actions }: SessionSelectionProps) {
  const [state, setState] = useState<State>({ kind: "empty" });
  const menu = useAnchoredMenu<HTMLButtonElement>();
  const context = useContextMenu<null>();
  const closeContext = context.close;
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

  // 숨겨지면 우클릭 메뉴를 닫는다. 다시 보일 때 예전 메뉴가 살아나지 않는다.
  useLayoutEffect(() => {
    if (!visible) closeContext();
  }, [visible, closeContext]);

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

  const shown = candidate && !(state.kind === "selecting" && state.closed) ? candidate : undefined;
  const busy = state.kind === "selecting" || state.kind === "clearing";
  const status = state.kind === "selecting" ? "세션 선택 중"
    : state.kind === "clearing" ? "세션 선택 해제 중"
      : !selected ? ""
        : state.kind === "confirming" ? "연결 확인 중"
          : selected.confirmed ? "연결 확인됨" : "연결 확인 전";
  const error = "error" in state ? state.error : undefined;
  const dot = busy || state.kind === "confirming" ? "busy" : error ? "error" : selected?.confirmed ? "running" : undefined;
  const name = shown ? `${safeLabel(shown.label)} (세션 ${shown.id})` : "선택 안 함";
  const items: MenuItem[] = [
    ...(selected ? [{ kind: "label" as const, label: `${selected.region} / ${selected.node}` }] : []),
    ...candidates.map(({ id, label }) => ({
      label: `${safeLabel(label)} (세션 ${id})`,
      icon: id === shown?.id ? Check : undefined,
      onSelect: () => {
        menu.anchor.current?.focus();
        if (id !== shown?.id) void select(id);
      },
    })),
  ];
  const contextItems: MenuItem[] = selected
    ? [...items, { kind: "separator" }, { label: "선택 해제", icon: X, onSelect: () => void select("") }]
    : items;

  return <div className="a01-session" hidden={!visible}>
    {candidates.length > 0
      ? <span className="a01-chip-group">
        <button type="button" ref={menu.anchor} className="a01-chip" data-value={shown?.id ?? ""}
          aria-label={`AI 작업 세션: ${name}`} title={selected ? `${selected.region} / ${selected.node}` : undefined}
          aria-haspopup="menu" aria-expanded={menu.open} disabled={busy} onClick={menu.toggle}
          onContextMenu={(event) => context.open(event, null, "AI 작업 세션")}
          onKeyDown={(event) => {
            if (context.open(event, null, "AI 작업 세션")) return;
            if (event.key === "ArrowDown" || event.key === "ArrowUp") {
              event.preventDefault();
              menu.show();
            }
          }}>
          <Cloud aria-hidden="true" />
          <span className="a01-chip-text">{shown ? safeLabel(shown.label) : "세션 선택"}</span>
          {shown && <span className="status-dot" data-state={dot} aria-hidden="true" />}
          <ChevronDown className="a01-chip-caret" aria-hidden="true" />
        </button>
        {selected && <IconButton small icon={X} label="선택 해제" className="a01-chip-clear"
          onClick={() => void select("")} />}
      </span>
      : <span className="a01-chip muted" role="status"><Cloud aria-hidden="true" />SSM 세션 없음</span>}
    {selected && !selected.confirmed && <button type="button" className="a01-chip action"
      disabled={state.kind === "confirming"} onClick={() => void confirm()}>
      <PlugZap aria-hidden="true" />연결 확인
    </button>}
    {status && <span className="a01-sr-only" role="status">{status}</span>}
    {error && <p className="a01-inline-alert" role="alert">{error}</p>}
    {menu.open && visible && candidates.length > 0 && <AnchoredMenu anchor={menu.anchor} placement="above"
      items={items} label="AI 작업 세션" onClose={menu.close} />}
    {visible && candidates.length > 0 && context.element(() => contextItems)}
  </div>;
}
