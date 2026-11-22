import { useId, useLayoutEffect, useRef, useState, type FormEvent } from "react";
import type { AiPanelProps, ChatMessage, Proposal, ProposalTarget, RecordedDecision } from "./contracts";
import "./ai.css";

type ChatState = { kind: "idle" } | { kind: "sending" } | { kind: "failed"; message: string };
type ProposalState = { kind: "idle" } | { kind: "requesting" } | { kind: "failed" };
type DecisionState = RecordedDecision | "reviewing" | "failed";

function targetKey({ targetId, generation }: ProposalTarget | Proposal["target"]) {
  return JSON.stringify([targetId, generation]);
}

function visibleText(value: string) {
  return value.replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, (character) =>
    `\\u{${character.codePointAt(0)?.toString(16)}}`);
}

export function AiPanel(props: AiPanelProps) {
  const [modelName, setModelName] = useState("");
  const [selected, setSelected] = useState("");
  const model = props.models.includes(modelName) ? modelName : "";
  const target = props.targets.find((candidate) => targetKey(candidate) === selected);
  const attempted = useRef(new Set<string>());
  const chatTitle = useId();
  const proposalTitle = useId();

  return <>
    <section className="workspace-chat a01-chat" aria-labelledby={chatTitle}>
      <h2 id={chatTitle}>AI 채팅</h2>
      {props.modelError && <p role="alert">{props.modelError}</p>}
      {props.onRefresh && <button type="button" onClick={props.onRefresh}>모델 및 대상 새로고침</button>}
      <Chat models={props.models} model={model} onModelChange={setModelName} onSendChat={props.onSendChat} />
    </section>
    <section className="workspace-ai-terminal a01-proposals" aria-labelledby={proposalTitle}>
      <h2 id={proposalTitle}>작업 제안</h2>
      {props.targetError && <p role="alert">{props.targetError}</p>}
      {props.policyControl}
      <label className="a01-field">조회 대상
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
      </label>
      {target && props.onOpenCliReview && <button type="button" onClick={props.onOpenCliReview}>CLI 제안 열기</button>}
      {target ? <ProposalReview key={selected} model={model} target={target} proposals={props.proposals}
        attempted={attempted.current} onRequestProposal={props.onRequestProposal} onDecideProposal={props.onDecideProposal} />
        : <p role="status">대기</p>}
    </section>
  </>;
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
    <label className="a01-field">Ollama 모델
      <select value={model} disabled={state.kind === "sending"} onChange={(event) => onModelChange(event.target.value)}>
        <option value="">모델 선택</option>
        {models.map((name) => <option key={name} value={name}>{name}</option>)}
      </select>
    </label>
    <ol className="a01-transcript" role="log" aria-label="대화 기록">
      {messages.map(({ role, content }, index) => <li key={index}>
        <strong>{role === "user" ? "나" : "AI"}</strong>
        <p>{content}</p>
      </li>)}
    </ol>
    <form className="a01-composer" onSubmit={send}>
      <textarea ref={input} aria-label="메시지 입력" placeholder="메시지 입력" value={draft}
        readOnly={state.kind === "sending"} onChange={(event) => setDraft(event.target.value)} />
      {state.kind === "failed" && <p role="alert">전송 실패: {state.message}</p>}
      <button type="submit" disabled={!model || !draft.trim() || state.kind === "sending"}>
        {state.kind === "sending" ? "전송 중" : "전송"}
      </button>
    </form>
  </>;
}

function ProposalReview({ model, target, proposals, attempted, onRequestProposal, onDecideProposal }:
  Pick<AiPanelProps, "proposals" | "onRequestProposal" | "onDecideProposal"> & {
    model: string;
    target: ProposalTarget;
    attempted: Set<string>;
  }) {
  const [request, setRequest] = useState<ProposalState>({ kind: "idle" });
  const [created, setCreated] = useState<Proposal[]>([]);
  const [decisions, setDecisions] = useState<ReadonlyMap<string, DecisionState>>(new Map());
  const pending = useRef<AbortController | null>(null);
  const mounted = useRef(false);
  useLayoutEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; pending.current?.abort(); pending.current = null; };
  }, []);

  const key = targetKey(target);
  const cards = new Map([...created, ...proposals]
    .filter((proposal) => targetKey(proposal.target) === key)
    .map((proposal) => [proposal.id, proposal]));

  async function propose() {
    if (!model || pending.current) return;
    const token = new AbortController();
    pending.current = token;
    setRequest({ kind: "requesting" });
    const { targetId, generation } = target;
    try {
      const result = await onRequestProposal({ model, target: { targetId, generation }, operation: "uptime" }, token.signal);
      if (pending.current !== token) return;
      if (targetKey(result.target) !== key) {
        setRequest({ kind: "failed" });
      } else {
        setCreated((previous) => [...previous.filter((proposal) => proposal.id !== result.id), result]);
        setRequest({ kind: "idle" });
      }
    } catch {
      if (pending.current === token) setRequest({ kind: "failed" });
    } finally {
      if (pending.current === token) pending.current = null;
    }
  }

  async function decide(proposal: Proposal, decision: "approve" | "reject" | "cancel", button: HTMLButtonElement) {
    if (!mounted.current || attempted.has(proposal.id) || targetKey(proposal.target) !== key ||
      (decision === "approve" && proposal.kind !== "reviewable")) return;
    attempted.add(proposal.id);
    if (button.ownerDocument.activeElement === button) button.closest("article")?.focus({ preventScroll: true });
    const mark = (state: DecisionState) => {
      if (mounted.current) setDecisions((previous) => new Map(previous).set(proposal.id, state));
    };
    mark("reviewing");
    try {
      mark(await onDecideProposal({ proposalId: proposal.id, decision }));
    } catch {
      mark("failed");
    }
  }

  return <>
    <div className="a01-actions">
      <button type="button" disabled={!model || request.kind === "requesting"} onClick={propose}>가동 시간 조회 제안</button>
      {request.kind === "requesting" && <button type="button" onClick={() => {
        pending.current?.abort();
        pending.current = null;
        setRequest({ kind: "idle" });
      }}>요청 취소</button>}
    </div>
    {request.kind === "requesting" && <p role="status">제안 요청 중</p>}
    {request.kind === "failed" && <p role="alert">제안 요청 실패</p>}
    {request.kind === "idle" && cards.size === 0 && <p role="status">대기</p>}
    {[...cards.values()].map((proposal) => {
      const decision = decisions.get(proposal.id) ?? (attempted.has(proposal.id) ? "closed" : undefined);
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
      </article>;
    })}
  </>;
}
