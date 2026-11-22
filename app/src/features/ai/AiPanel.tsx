import { useId, useLayoutEffect, useRef, useState, type FormEvent, type KeyboardEvent, type ReactNode, type RefObject } from "react";
import type { AiPanelProps, ChatMessage, Proposal, ProposalTarget, RecordedDecision } from "./contracts";
import { SessionSelection, type SessionSelectionProps } from "./SessionSelection";
import "./ai.css";

type ChatState = { kind: "idle" } | { kind: "sending" } | { kind: "failed"; message: string };
type ProposalState = { kind: "idle" } | { kind: "requesting" } | { kind: "failed" };
type DecisionState = RecordedDecision | "reviewing" | "failed";

export type AiTopTabs = Readonly<{
  item: Readonly<{ id: string; label: string; content: ReactNode; hidden?: boolean }>;
  selectedId: string | null;
  onSelect: (id: string | null) => void;
}>;

function targetKey({ targetId, generation }: ProposalTarget | Proposal["target"]) {
  return JSON.stringify([targetId, generation]);
}

function visibleText(value: string) {
  return value.replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, (character) =>
    `\\u{${character.codePointAt(0)?.toString(16)}}`);
}

function navigateTopTabs(event: KeyboardEvent<HTMLDivElement>) {
  const tabs = event.currentTarget.querySelectorAll<HTMLButtonElement>('[role="tab"]');
  const chat = tabs[0];
  const item = tabs[1];
  let next: HTMLButtonElement;
  switch (event.key) {
    case "Home": next = chat; break;
    case "End": next = item; break;
    case "ArrowLeft":
    case "ArrowRight": next = event.target === chat ? item : chat; break;
    default: return;
  }
  event.preventDefault();
  next.click();
}

export function AiPanel(props: AiPanelProps & { topTabs?: AiTopTabs; sessionSelection?: SessionSelectionProps; policyStatus?: ReactNode }) {
  const [modelName, setModelName] = useState("");
  const [selected, setSelected] = useState("");
  const model = props.models.includes(modelName) ? modelName : "";
  const target = props.targets.find((candidate) => targetKey(candidate) === selected);
  const attempted = useRef(new Set<string>());
  const chatTitle = useId();
  const proposalTitle = useId();
  const proposalSettings = useRef<HTMLDetailsElement>(null);
  const { topTabs } = props;
  const visibleTabs = topTabs && !topTabs.item.hidden ? topTabs : undefined;
  const chatPanel = `${chatTitle}-panel`;
  const itemPanel = `${chatTitle}-item`;
  const itemSelected = !!visibleTabs && visibleTabs.selectedId === visibleTabs.item.id;

  return (
    <div className={visibleTabs ? "a01-top a01-top-tabbed" : "a01-top"}>
      {visibleTabs && <div className="a01-tabs" role="tablist" aria-label="오른쪽 패널" onKeyDown={navigateTopTabs}>
        {[
          { id: null, label: "AI 채팅", panel: chatPanel, selected: !itemSelected },
          { id: visibleTabs.item.id, label: visibleTabs.item.label, panel: itemPanel, selected: itemSelected },
        ].map(({ id, label, panel, selected }) =>
          <button key={panel} type="button" role="tab" id={`${panel}-tab`} aria-controls={panel}
            aria-selected={selected} tabIndex={selected ? 0 : -1} onClick={(event) => {
              event.currentTarget.focus();
              visibleTabs.onSelect(id);
            }}>{label}</button>)}
      </div>}
      <section className="workspace-chat a01-chat" id={visibleTabs ? chatPanel : undefined}
        role={visibleTabs ? "tabpanel" : undefined} aria-labelledby={visibleTabs ? `${chatPanel}-tab` : chatTitle}
        hidden={itemSelected} inert={itemSelected} tabIndex={visibleTabs && !itemSelected ? 0 : undefined}>
        {!visibleTabs && <h2 id={chatTitle}>AI 채팅</h2>}
        {props.sessionSelection && <SessionSelection {...props.sessionSelection}
          visible={props.sessionSelection.visible !== false && !itemSelected} />}
        {(props.modelError || props.onRefresh) && <div className="a01-discovery">
          {props.modelError && <p role="alert">{props.modelError}</p>}
          {props.onRefresh && <button type="button" aria-label="모델 및 대상 새로고침" onClick={props.onRefresh}>새로고침</button>}
        </div>}
        <Chat models={props.models} model={model} onModelChange={setModelName} onSendChat={props.onSendChat} />
        <div className="a01-proposals" role="region" aria-labelledby={proposalTitle}>
          <details className="a01-disclosure" ref={proposalSettings}>
            <summary id={proposalTitle}>작업 제안</summary>
            <div className="a01-proposal-controls">
              {props.policyControl}
              {props.targets.length > 0 && <label className="a01-field">조회 대상
                <select value={target ? selected : ""} onChange={(event) => {
                  const key = event.target.value;
                  setSelected(key);
                  const next = props.targets.find((candidate) => targetKey(candidate) === key);
                  props.onSelectTarget?.(next ? { targetId: next.targetId, generation: next.generation } : null);
                }}>
                  <option value="">대상 선택</option>
                  {props.targets.map((candidate) => <option key={targetKey(candidate)} value={targetKey(candidate)}>
                    {visibleText(candidate.label)} ({visibleText(candidate.targetId)})
                  </option>)}
                </select>
              </label>}
              {target && props.onOpenCliReview && <button type="button" onClick={props.onOpenCliReview}>CLI 제안 열기</button>}
            </div>
          </details>
          {props.targetError && <p role="alert">{props.targetError}</p>}
          {props.policyStatus}
          <ProposalReview key={JSON.stringify([selected, !!target, target && props.policyVersion])} model={model} target={target}
            proposals={props.proposals} history={props.history} attempted={attempted.current} settings={proposalSettings}
            onRequestProposal={props.onRequestProposal} onDecideProposal={props.onDecideProposal} onReadDecision={props.onReadDecision} />
        </div>
      </section>
      {topTabs && <div className="a01-top-item" role={visibleTabs ? "tabpanel" : undefined}
        id={visibleTabs ? itemPanel : undefined} aria-labelledby={visibleTabs ? `${itemPanel}-tab` : undefined}
        hidden={!itemSelected} inert={!itemSelected} tabIndex={itemSelected ? 0 : undefined}>
        {topTabs.item.content}
      </div>}
    </div>
  );
}

function Chat({ models, model, onModelChange, onSendChat }: Pick<AiPanelProps, "models" | "onSendChat"> & {
  model: string;
  onModelChange: (model: string) => void;
}) {
  const [draft, setDraft] = useState("");
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [state, setState] = useState<ChatState>({ kind: "idle" });
  const sending = useRef(false);
  const input = useRef<HTMLTextAreaElement>(null);

  async function send(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!model || !draft.trim() || sending.current) return;
    sending.current = true;
    input.current?.focus();
    setState({ kind: "sending" });
    const next: ChatMessage[] = [...messages, { role: "user", content: draft }];
    try {
      const reply = await onSendChat({ model, messages: next });
      if (reply.kind === "reply") {
        setMessages([...next, { role: "assistant", content: reply.content }]);
        setDraft("");
        setState({ kind: "idle" });
      } else {
        setState({ kind: "failed", message: reply.message });
      }
    } catch {
      setState({ kind: "failed", message: "로컬 모델 연결 오류" });
    } finally {
      sending.current = false;
    }
  }

  return <>
    {models.length > 0 && <label className="a01-field">Ollama 모델
      <select value={model} disabled={state.kind === "sending"} onChange={(event) => onModelChange(event.target.value)}>
        <option value="">모델 선택</option>
        {models.map((name) => <option key={name} value={name}>{name}</option>)}
      </select>
    </label>}
    <ol className="a01-transcript" role="log" aria-label="대화 기록">
      {messages.map(({ role, content }, index) => <li key={index}>
        <strong>{role === "user" ? "나" : "AI"}</strong>
        <p>{content}</p>
      </li>)}
    </ol>
    {state.kind === "failed" && <p role="alert">전송 실패: {state.message}</p>}
    {models.length === 0 && state.kind === "sending" && <p role="status">전송 중</p>}
    {models.length > 0 && <form className="a01-composer" onSubmit={send}>
      <textarea ref={input} aria-label="메시지 입력" placeholder="메시지 입력" value={draft}
        readOnly={state.kind === "sending"} onChange={(event) => setDraft(event.target.value)} />
      <button type="submit" disabled={!model || !draft.trim() || state.kind === "sending"}>
        {state.kind === "sending" ? "전송 중" : "전송"}
      </button>
    </form>}
  </>;
}

function ProposalReview({ model, target, proposals, history, attempted, settings, onRequestProposal, onDecideProposal, onReadDecision }:
  Pick<AiPanelProps, "proposals" | "history" | "onRequestProposal" | "onDecideProposal" | "onReadDecision"> & {
    model: string;
    target: ProposalTarget | undefined;
    attempted: Set<string>;
    settings: RefObject<HTMLDetailsElement | null>;
  }) {
  const [request, setRequest] = useState<ProposalState>({ kind: "idle" });
  const [created, setCreated] = useState<Proposal[]>([]);
  const [decisions, setDecisions] = useState<ReadonlyMap<string, DecisionState>>(new Map());
  const pending = useRef<AbortController | null>(null);
  const mounted = useRef(false);
  const proposeButton = useRef<HTMLButtonElement>(null);
  const cancelButton = useRef<HTMLButtonElement>(null);
  useLayoutEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; pending.current?.abort(); pending.current = null; };
  }, []);

  const key = target && targetKey(target);
  const cards = new Map([...created, ...proposals]
    .filter((proposal) => targetKey(proposal.target) === key)
    .map((proposal) => [proposal.id, proposal]));
  const recorded = new Map(history?.map(({ proposal, decision }) => [proposal.id, decision]));
  for (const { proposal } of history ?? []) cards.set(proposal.id, proposal);

  function keepRequestFocus() {
    const cancel = cancelButton.current;
    if (cancel && cancel.ownerDocument.activeElement === cancel) {
      const next = settings.current?.open && model ? proposeButton.current : settings.current?.querySelector("summary");
      next?.focus({ preventScroll: true });
    }
  }

  async function propose() {
    if (!model || !target || pending.current) return;
    const token = new AbortController();
    pending.current = token;
    setRequest({ kind: "requesting" });
    const { targetId, generation } = target;
    try {
      const result = await onRequestProposal({ model, target: { targetId, generation }, operation: "uptime" }, token.signal);
      if (pending.current !== token) return;
      keepRequestFocus();
      if (targetKey(result.target) !== key) {
        setRequest({ kind: "failed" });
      } else {
        setCreated((previous) => [...previous.filter((proposal) => proposal.id !== result.id), result]);
        setRequest({ kind: "idle" });
      }
    } catch {
      if (pending.current === token) {
        keepRequestFocus();
        setRequest({ kind: "failed" });
      }
    } finally {
      if (pending.current === token) pending.current = null;
    }
  }

  async function recordDecision(proposalId: string, button: HTMLButtonElement, read: () => Promise<RecordedDecision | undefined>) {
    if (!mounted.current) return;
    if (button.ownerDocument.activeElement === button) button.closest("article")?.focus({ preventScroll: true });
    const mark = (state: DecisionState) => {
      if (mounted.current) setDecisions((previous) => new Map(previous).set(proposalId, state));
    };
    mark("reviewing");
    try {
      mark(await read() ?? "failed");
    } catch {
      mark("failed");
    }
  }

  function decide(proposal: Proposal, decision: "approve" | "reject" | "cancel", button: HTMLButtonElement) {
    if (!mounted.current || attempted.has(proposal.id) || targetKey(proposal.target) !== key ||
      (decision === "approve" && proposal.kind !== "reviewable")) return;
    attempted.add(proposal.id);
    void recordDecision(proposal.id, button, () => onDecideProposal({ proposalId: proposal.id, decision }));
  }

  return <>
    {target && (model || request.kind === "requesting") && <div className="a01-actions a01-request">
      <button type="button" className="a01-request-start" ref={proposeButton} disabled={!model} aria-disabled={request.kind === "requesting" || undefined}
        onClick={propose}>가동 시간 조회 제안</button>
      {request.kind === "requesting" && <button type="button" className="a01-request-cancel" ref={cancelButton} onClick={() => {
        keepRequestFocus();
        pending.current?.abort();
        pending.current = null;
        setRequest({ kind: "idle" });
      }}>요청 취소</button>}
    </div>}
    {request.kind === "requesting" && <p role="status">제안 요청 중</p>}
    {request.kind === "failed" && <p role="alert">제안 요청 실패</p>}
    {[...cards.values()].map((proposal) => {
      const decision = recorded.get(proposal.id) ?? decisions.get(proposal.id) ?? (attempted.has(proposal.id) ? "closed" : undefined);
      return <article key={proposal.id} className="a01-proposal" aria-label="조회 제안" tabIndex={-1}>
        <dl>
          {proposal.ssm && <>
            <dt>SSM 프로필</dt><dd>{visibleText(proposal.ssm.profile)}</dd>
            <dt>리전</dt><dd>{visibleText(proposal.ssm.region)}</dd>
          </>}
          <dt>대상</dt><dd>{visibleText(proposal.node)}</dd>
          {proposal.kind === "reviewable" && <>
            <dt>명령</dt><dd><code>{visibleText(proposal.displayCommand)}</code></dd>
            <dt>해석</dt><dd>{visibleText(proposal.interpretation)}</dd>
          </>}
        </dl>
        {proposal.kind !== "reviewable" && <p>{proposal.kind === "blocked" ? "차단된 조회" : "미등록 조회"}</p>}
        {decision ? <p role="status" tabIndex={-1} ref={(status) => {
          if (status && status.ownerDocument.activeElement === status.closest("article")) status.focus({ preventScroll: true });
        }}>{{
          reviewing: "결정 기록 중", approved: "승인 기록됨", cancelled: "취소됨",
          rejected: "거절 기록됨", failed: "결정 기록 실패", closed: "검토 종료"
        }[decision]}</p> : <div className="a01-actions">
          {proposal.kind === "reviewable" && <button type="button" onClick={(event) => decide(proposal, "approve", event.currentTarget)}>승인</button>}
          <button type="button" onClick={(event) => decide(proposal, "reject", event.currentTarget)}>거절</button>
          <button type="button" onClick={(event) => decide(proposal, "cancel", event.currentTarget)}>취소</button>
        </div>}
        {decision === "failed" && onReadDecision && <button type="button" onClick={(event) => {
          if (decisions.get(proposal.id) === "failed") {
            void recordDecision(proposal.id, event.currentTarget, () => onReadDecision(proposal.id));
          }
        }}>결정 다시 조회</button>}
      </article>;
    })}
  </>;
}
