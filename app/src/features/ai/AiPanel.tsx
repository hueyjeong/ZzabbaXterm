import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode, type RefObject } from "react";
import {
  Activity, ArrowUp, Ban, Check, ChevronDown, CircleAlert, CircleCheck, CircleHelp, Cloud, Crosshair, Ellipsis, Globe, Inbox,
  LoaderCircle, Play, RefreshCw, Server, ShieldAlert, ShieldCheck, ShieldOff, Sparkles, Square, SquarePen, SquareTerminal, X,
} from "lucide-react";
import { IconButton, type MenuItem } from "../ui/controls";
import { AnchoredMenu, useAnchoredMenu } from "./AnchoredMenu";
import {
  executionActive, type AiPanelProps, type ChatMessage, type ExecutionPhase, type ExecutionStatus, type Proposal,
  type ProposalTarget, type RecordedDecision,
} from "./contracts";
import { SessionSelection, type SessionSelectionProps } from "./SessionSelection";
import "./ai.css";

type ChatState = { kind: "idle" } | { kind: "sending"; content: string } | { kind: "failed"; message: string };
type ProposalState = { kind: "idle" } | { kind: "requesting" } | { kind: "failed" };
type DecisionState = RecordedDecision | "reviewing" | "failed";
type Entry = Readonly<{ seq: number; message: ChatMessage }>;

const EXECUTION_LABEL: Record<ExecutionPhase, string> = {
  starting: "채널 연결 중",
  pairing: "연결 코드 대기",
  waitingClaim: "중앙 셸 예약 키 대기",
  awaitingStart: "작업 시작 대기",
  running: "실행 중",
  stopRequested: "중지 요청됨",
  exited: "종료됨",
  unconfirmed: "종료 확인 불가",
  failed: "실행 실패",
};

// 채널 연결 → 연결 코드 → 예약 키 → 실행 → 종료
const EXECUTION_STEPS = 5;
const EXECUTION_STEP: Record<ExecutionPhase, number> = {
  starting: 0, pairing: 1, waitingClaim: 2, awaitingStart: 3, running: 3, stopRequested: 3,
  exited: 4, unconfirmed: 4, failed: 4,
};

const DECISION_LABEL: Record<DecisionState | "closed", string> = {
  reviewing: "기록 중", approved: "승인됨", rejected: "거절됨", cancelled: "취소됨", failed: "기록 실패", closed: "검토 종료",
};

function blockText(value: string) {
  return value.replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, (character) =>
    character === "\n" || character === "\t" ? character
      : `\\u{${character.codePointAt(0)?.toString(16)}}`);
}

function targetKey({ targetId, generation }: ProposalTarget | Proposal["target"]) {
  return JSON.stringify([targetId, generation]);
}

function visibleText(value: string) {
  return value.replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, (character) =>
    `\\u{${character.codePointAt(0)?.toString(16)}}`);
}

function menuKeys(open: () => void) {
  return (event: KeyboardEvent<HTMLButtonElement>) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      open();
    }
  };
}

export function AiPanel(props: AiPanelProps & { sessionSelection?: SessionSelectionProps; policyStatus?: ReactNode }) {
  const [modelName, setModelName] = useState("");
  const [selected, setSelected] = useState("");
  const [entries, setEntries] = useState<readonly Entry[]>([]);
  const [chat, setChat] = useState<ChatState>({ kind: "idle" });
  // 사용자가 고르기 전에는 첫 모델을 쓴다.
  // 고르기 전에는 첫 모델을 쓴다. 고른 모델이 목록에서 빠지면 다른 모델로 조용히 바꾸지 않고 다시 고르게 한다.
  const model = props.models.includes(modelName) ? modelName : modelName ? "" : props.models[0] ?? "";
  const target = props.targets.find((candidate) => targetKey(candidate) === selected);
  const attempted = useRef(new Set<string>());
  const order = useRef(new Map<string, number>());
  const sequence = useRef(0);
  const input = useRef<HTMLTextAreaElement>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const title = useId();
  const more = useAnchoredMenu<HTMLButtonElement>();
  const targetMenu = useAnchoredMenu<HTMLButtonElement>();
  const { policy } = props;

  useEffect(() => {
    // 맨 아래를 보고 있을 때만 새 내용으로 따라 내려간다.
    const element = scroller.current;
    if (!element) return;
    const follow = () => {
      if (stick.current) element.scrollTop = element.scrollHeight;
    };
    const observer = new MutationObserver(follow);
    observer.observe(element, { childList: true, subtree: true, characterData: true });
    return () => observer.disconnect();
  }, []);

  function chooseTarget(key: string) {
    targetMenu.anchor.current?.focus();
    if (key === selected && target) return;
    setSelected(key);
    const next = props.targets.find((candidate) => targetKey(candidate) === key);
    props.onSelectTarget?.(next ? { targetId: next.targetId, generation: next.generation } : null);
  }

  const targetName = target ? `${visibleText(target.label)} (${visibleText(target.targetId)})` : "선택 안 함";
  const targetItems: MenuItem[] = [
    ...props.targets.map((candidate) => ({
      label: `${visibleText(candidate.label)} (${visibleText(candidate.targetId)})`,
      icon: target && targetKey(candidate) === selected ? Check : undefined,
      onSelect: () => chooseTarget(targetKey(candidate)),
    })),
    ...(target ? [{ kind: "separator" as const }, { label: "대상 선택 해제", icon: X, onSelect: () => chooseTarget("") }] : []),
  ];
  const moreItems: MenuItem[] = [
    { kind: "label", label: "조회 승인 정책" },
    policy?.blocked
      ? { label: "조회 승인 차단 해제", icon: ShieldCheck, disabled: policy.saving, onSelect: () => policy.onChange(false) }
      : { label: "조회 승인 차단", icon: ShieldOff, disabled: !policy || policy.saving, onSelect: () => policy?.onChange(true) },
  ];

  return (
    <section className="workspace-chat a01-chat" aria-labelledby={title}>
      <header className="a01-head">
        <h2 id={title}>AI</h2>
        {policy?.blocked && <span className="a01-head-badge"><ShieldAlert aria-hidden="true" />승인 차단</span>}
        <div className="a01-head-actions">
          <IconButton icon={SquarePen} label="새 대화" disabled={chat.kind === "sending" || entries.length === 0}
            onClick={() => {
              setEntries([]);
              setChat({ kind: "idle" });
              stick.current = true;
              input.current?.focus();
            }} />
          {props.onOpenCliReview && <IconButton icon={Inbox} label="CLI 제안 가져오기" disabled={!target}
            onClick={props.onOpenCliReview} />}
          {props.onRefresh && <IconButton icon={RefreshCw} label="모델 및 대상 새로고침" onClick={props.onRefresh} />}
          <button type="button" ref={more.anchor} className="icon-button" aria-label="더보기" title="더보기"
            aria-haspopup="menu" aria-expanded={more.open} onClick={more.toggle} onKeyDown={menuKeys(more.show)}>
            <Ellipsis aria-hidden="true" />
          </button>
          {more.open && <AnchoredMenu anchor={more.anchor} align="end" items={moreItems} label="더보기" onClose={more.close} />}
        </div>
      </header>
      {props.policyStatus && <div className="a01-notice">{props.policyStatus}</div>}
      <div className="a01-scroll" ref={scroller} onScroll={(event) => {
        const element = event.currentTarget;
        stick.current = element.scrollHeight - element.scrollTop - element.clientHeight < 48;
      }}>
        <Conversation key={JSON.stringify([selected, !!target, target && props.policyVersion])} model={model} target={target}
          entries={entries} chat={chat} order={order.current} nextSeq={() => ++sequence.current} fallbackFocus={input}
          proposals={props.proposals} history={props.history} attempted={attempted.current}
          execution={props.execution} onExecuteProposal={props.onExecuteProposal} onStopExecution={props.onStopExecution}
          onRequestProposal={props.onRequestProposal} onDecideProposal={props.onDecideProposal} onReadDecision={props.onReadDecision} />
      </div>
      <div className="a01-bottom">
        {props.targetError && <p className="a01-inline-alert" role="alert">{props.targetError}</p>}
        {props.modelError && <p className="a01-inline-alert" role="alert">{props.modelError}</p>}
        {chat.kind === "failed" && <p className="a01-inline-alert" role="alert">전송 실패: {chat.message}</p>}
        <Composer models={props.models} model={model} onModelChange={setModelName} input={input} chat={chat}
          onSend={async (draft) => {
            stick.current = true;
            setChat({ kind: "sending", content: draft });
            const next: ChatMessage[] = [...entries.map(({ message }) => message), { role: "user", content: draft }];
            try {
              const reply = await props.onSendChat({ model, messages: next });
              if (reply.kind === "reply") {
                const user = ++sequence.current;
                const assistant = ++sequence.current;
                setEntries([...entries, { seq: user, message: next[next.length - 1] },
                  { seq: assistant, message: { role: "assistant", content: reply.content } }]);
                setChat({ kind: "idle" });
                return true;
              }
              setChat({ kind: "failed", message: reply.message });
            } catch {
              setChat({ kind: "failed", message: "로컬 모델 연결 오류" });
            }
            return false;
          }}>
          {props.targets.length > 0 && <>
            <button type="button" ref={targetMenu.anchor} className="a01-chip" data-active={!!target || undefined}
              aria-label={`조회 대상: ${targetName}`} title={target ? targetName : undefined}
              aria-haspopup="menu" aria-expanded={targetMenu.open} onClick={targetMenu.toggle} onKeyDown={menuKeys(targetMenu.show)}>
              <Crosshair aria-hidden="true" />
              <span className="a01-chip-text">{target ? visibleText(target.label) : "대상 선택"}</span>
              <ChevronDown className="a01-chip-caret" aria-hidden="true" />
            </button>
            {targetMenu.open && <AnchoredMenu anchor={targetMenu.anchor} placement="above" items={targetItems}
              label="조회 대상" onClose={targetMenu.close} />}
          </>}
          {props.sessionSelection && <SessionSelection {...props.sessionSelection}
            visible={props.sessionSelection.visible !== false} />}
        </Composer>
      </div>
    </section>
  );
}

function Composer({ models, model, onModelChange, input, chat, onSend, children }: {
  models: readonly string[];
  model: string;
  onModelChange: (model: string) => void;
  input: RefObject<HTMLTextAreaElement | null>;
  chat: ChatState;
  onSend: (draft: string) => Promise<boolean>;
  children: ReactNode;
}) {
  const [draft, setDraft] = useState("");
  const sending = useRef(false);
  const modelMenu = useAnchoredMenu<HTMLButtonElement>();
  const busy = chat.kind === "sending";

  useLayoutEffect(() => {
    const element = input.current;
    if (!element) return;
    element.style.height = "auto";
    element.style.height = `${Math.min(element.scrollHeight, 200)}px`;
  }, [draft, input]);

  async function submit() {
    if (!model || !draft.trim() || sending.current) return;
    sending.current = true;
    input.current?.focus();
    const content = draft;
    setDraft("");
    try {
      if (!await onSend(content)) setDraft(content);
    } finally {
      sending.current = false;
    }
  }

  const modelItems: MenuItem[] = models.map((name) => ({
    label: name,
    icon: name === model ? Check : undefined,
    onSelect: () => {
      onModelChange(name);
      input.current?.focus();
    },
  }));

  return <form className="a01-composer" aria-label="메시지 작성" data-busy={busy || undefined} onSubmit={(event) => {
    event.preventDefault();
    void submit();
  }}>
    <div className="a01-context">{children}</div>
    <textarea ref={input} aria-label="메시지 입력" placeholder="메시지 입력" rows={1} value={draft}
      readOnly={busy} onChange={(event) => setDraft(event.target.value)} onKeyDown={(event) => {
        if (event.key !== "Enter" || event.shiftKey) return;
        // 한글 등 IME 조합을 확정하는 Enter는 전송하지 않는다.
        if (event.nativeEvent.isComposing || event.keyCode === 229) return;
        event.preventDefault();
        void submit();
      }} />
    <div className="a01-toolbar">
      <button type="button" ref={modelMenu.anchor} className="a01-model" data-error={models.length === 0 || undefined}
        aria-label={`모델: ${models.length === 0 ? "없음" : model || "선택 안 함"}`} aria-haspopup="menu"
        aria-expanded={modelMenu.open} disabled={busy || models.length === 0}
        onClick={modelMenu.toggle} onKeyDown={menuKeys(modelMenu.show)}>
        {models.length === 0 ? <CircleAlert aria-hidden="true" /> : <Sparkles aria-hidden="true" />}
        <span>{models.length === 0 ? "모델 없음" : model || "모델 선택"}</span>
        {models.length > 0 && <ChevronDown className="a01-chip-caret" aria-hidden="true" />}
      </button>
      {modelMenu.open && models.length > 0 && <AnchoredMenu anchor={modelMenu.anchor} placement="above"
        items={[{ kind: "label", label: "Ollama 모델" }, ...modelItems]} label="모델" onClose={modelMenu.close} />}
      <button type="submit" className="a01-send" aria-label="전송" aria-busy={busy || undefined}
        disabled={!model || !draft.trim() || busy}>
        {busy ? <LoaderCircle className="a01-spin" aria-hidden="true" /> : <ArrowUp aria-hidden="true" />}
      </button>
    </div>
  </form>;
}

function Conversation({ model, target, entries, chat, order, nextSeq, fallbackFocus, proposals, history, attempted, execution,
  onExecuteProposal, onStopExecution, onRequestProposal, onDecideProposal, onReadDecision }:
  Pick<AiPanelProps, "proposals" | "history" | "execution" | "onExecuteProposal" | "onStopExecution" | "onRequestProposal" | "onDecideProposal" | "onReadDecision"> & {
    model: string;
    target: ProposalTarget | undefined;
    entries: readonly Entry[];
    chat: ChatState;
    order: Map<string, number>;
    nextSeq: () => number;
    fallbackFocus: RefObject<HTMLTextAreaElement | null>;
    attempted: Set<string>;
  }) {
  const [request, setRequest] = useState<ProposalState>({ kind: "idle" });
  const [created, setCreated] = useState<Proposal[]>([]);
  const [decisions, setDecisions] = useState<ReadonlyMap<string, DecisionState>>(new Map());
  const [executing, setExecuting] = useState<string>();
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
  for (const id of cards.keys()) if (!order.has(id)) order.set(id, nextSeq());

  const timeline = [
    ...entries.map((entry) => ({ seq: entry.seq, entry, proposal: undefined })),
    ...[...cards.values()].map((proposal) => ({ seq: order.get(proposal.id)!, entry: undefined, proposal })),
  ].sort((a, b) => a.seq - b.seq);
  const offerProposal = !!target && (!!model || request.kind === "requesting");

  function keepRequestFocus() {
    const cancel = cancelButton.current;
    if (cancel && cancel.ownerDocument.activeElement === cancel) {
      const next = model ? proposeButton.current : fallbackFocus.current;
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

  async function execute(proposal: Proposal) {
    if (!onExecuteProposal || executing) return;
    setExecuting(proposal.id);
    try {
      await onExecuteProposal(proposal.id, "uptime");
    } finally {
      if (mounted.current) setExecuting(undefined);
    }
  }

  function card(proposal: Proposal) {
    const decision = recorded.get(proposal.id) ?? decisions.get(proposal.id) ?? (attempted.has(proposal.id) ? "closed" : undefined);
    const active = execution !== undefined && executionActive(execution.phase);
    const KindIcon = proposal.kind === "reviewable" ? SquareTerminal : proposal.kind === "blocked" ? Ban : CircleHelp;
    return <article key={proposal.id} className="a01-card" data-kind={proposal.kind} aria-label="조회 제안" tabIndex={-1}>
      <header className="a01-card-head">
        <KindIcon className="a01-card-icon" aria-hidden="true" />
        {proposal.kind === "reviewable"
          ? <code className="a01-card-command" data-field="command">{visibleText(proposal.displayCommand)}</code>
          : <span className="a01-card-command">{proposal.kind === "blocked" ? "차단된 조회" : "미등록 조회"}</span>}
        {decision && <span className="a01-badge" data-state={decision} role="status" tabIndex={-1} ref={(status) => {
          if (status && status.ownerDocument.activeElement === status.closest("article")) status.focus({ preventScroll: true });
        }}>{DECISION_LABEL[decision]}</span>}
      </header>
      <div className="a01-card-target">
        <span className="a01-tag" title="대상"><Server aria-hidden="true" /><span data-field="node">{visibleText(proposal.node)}</span></span>
        {proposal.ssm && <>
          <span className="a01-tag" title="SSM 프로필"><Cloud aria-hidden="true" /><span data-field="profile">{visibleText(proposal.ssm.profile)}</span></span>
          <span className="a01-tag" title="리전"><Globe aria-hidden="true" /><span data-field="region">{visibleText(proposal.ssm.region)}</span></span>
        </>}
      </div>
      {proposal.kind === "reviewable" &&
        <p className="a01-card-text" data-field="interpretation">{visibleText(proposal.interpretation)}</p>}
      {!decision && <div className="a01-card-actions">
        {proposal.kind === "reviewable" && <button type="button" className="ui-button primary"
          onClick={(event) => decide(proposal, "approve", event.currentTarget)}><Check aria-hidden="true" />승인</button>}
        <button type="button" className="ui-button"
          onClick={(event) => decide(proposal, "reject", event.currentTarget)}><X aria-hidden="true" />거절</button>
        <button type="button" className="ui-button ghost"
          onClick={(event) => decide(proposal, "cancel", event.currentTarget)}>취소</button>
      </div>}
      {decision === "failed" && onReadDecision && <div className="a01-card-actions">
        <button type="button" className="ui-button" onClick={(event) => {
          if (decisions.get(proposal.id) === "failed") {
            void recordDecision(proposal.id, event.currentTarget, () => onReadDecision(proposal.id));
          }
        }}><RefreshCw aria-hidden="true" />다시 조회</button>
      </div>}
      {decision === "approved" && proposal.kind === "reviewable" && onExecuteProposal && !active &&
        targetKey(proposal.target) === key &&
        <div className="a01-card-actions">
          <button type="button" className="ui-button primary" disabled={!!executing} onClick={() => void execute(proposal)}>
            {executing === proposal.id
              ? <><LoaderCircle className="a01-spin" aria-hidden="true" />실행 요청 중</>
              : <><Play aria-hidden="true" />실행</>}
          </button>
        </div>}
      {execution?.proposalId === proposal.id && <Execution execution={execution} onStopExecution={onStopExecution} />}
    </article>;
  }

  const empty = timeline.length === 0 && chat.kind !== "sending";
  return <>
    {empty && <div className="a01-empty">
      <span className="a01-empty-mark" aria-hidden="true"><Sparkles /></span>
    </div>}
    <div className="a01-log" role="log" aria-label="대화 기록" hidden={empty}>
      {timeline.map(({ entry, proposal }) => entry
        ? <div key={`m${entry.seq}`} className="a01-msg" data-role={entry.message.role}>
          {entry.message.role === "assistant" && <span className="a01-avatar" aria-hidden="true"><Sparkles /></span>}
          <span className="a01-sr-only">{entry.message.role === "user" ? "나" : "AI"}</span>
          <p className="a01-msg-body">{entry.message.content}</p>
        </div>
        : card(proposal!))}
      {chat.kind === "sending" && <>
        <div className="a01-msg" data-role="user" data-pending="true">
          <p className="a01-msg-body">{chat.content}</p>
        </div>
        <div className="a01-msg" data-role="assistant" aria-hidden="true">
          <span className="a01-avatar"><Sparkles /></span>
          <span className="a01-typing"><i /><i /><i /></span>
        </div>
      </>}
    </div>
    <div className="a01-proposals">
      {request.kind === "requesting" && <div className="a01-pending">
        <LoaderCircle className="a01-spin" aria-hidden="true" />
        <span role="status">제안 요청 중</span>
        <button type="button" className="ui-button ghost" ref={cancelButton} onClick={() => {
          keepRequestFocus();
          pending.current?.abort();
          pending.current = null;
          setRequest({ kind: "idle" });
        }}>요청 취소</button>
      </div>}
      {request.kind === "failed" && <p className="a01-inline-alert" role="alert">제안 요청 실패</p>}
      {offerProposal && <div className="a01-suggestions">
        <button type="button" className="a01-suggestion" ref={proposeButton} disabled={!model}
          aria-disabled={request.kind === "requesting" || undefined} onClick={propose}>
          <Activity aria-hidden="true" />가동 시간 확인
        </button>
      </div>}
    </div>
  </>;
}

function Execution({ execution, onStopExecution }: Pick<AiPanelProps, "onStopExecution"> & { execution: ExecutionStatus }) {
  const { phase } = execution;
  const active = executionActive(phase);
  const step = EXECUTION_STEP[phase];
  const tone = phase === "exited" ? "done" : phase === "failed" ? "error" : phase === "unconfirmed" ? "warning" : "active";
  const StateIcon = active ? LoaderCircle : phase === "exited" ? CircleCheck : CircleAlert;
  return <section className="a01-execution" aria-label="실행 상태" data-tone={tone}>
    <div className="a01-exec-head">
      <StateIcon className={active ? "a01-exec-icon a01-spin" : "a01-exec-icon"} aria-hidden="true" />
      <span className="a01-exec-label" role="status">{EXECUTION_LABEL[phase]}</span>
      {phase === "running" && execution.approvalId && onStopExecution &&
        <button type="button" className="ui-button danger a01-stop"
          onClick={() => void onStopExecution(execution.approvalId as string)}>
          <Square aria-hidden="true" />중지
        </button>}
    </div>
    <div className="a01-steps" role="progressbar" aria-label="실행 단계" aria-valuemin={0} aria-valuemax={EXECUTION_STEPS}
      aria-valuenow={active ? step : EXECUTION_STEPS} aria-valuetext={EXECUTION_LABEL[phase]}>
      {Array.from({ length: EXECUTION_STEPS }, (_, index) =>
        <span key={index} data-state={!active || index < step ? "done" : index === step ? "current" : "todo"} />)}
    </div>
    {execution.pairingCode && <div className="a01-pairing">
      <span>연결 코드</span>
      <code>{visibleText(execution.pairingCode)}</code>
    </div>}
    {execution.error && <p className="a01-inline-alert" role="alert">{visibleText(execution.error)}</p>}
    {execution.output && <details className="a01-fold" open>
      <summary>출력</summary>
      <pre className="a01-execution-output">{blockText(execution.output)}</pre>
    </details>}
    <details className="a01-fold">
      <summary>세부 정보</summary>
      <dl className="a01-facts">
        {execution.node && <><dt>대상</dt><dd>{visibleText(execution.node)}</dd></>}
        {execution.sessionId && <><dt>세션</dt><dd>{visibleText(execution.sessionId)}</dd></>}
        {execution.program && <><dt>절대 경로</dt><dd><code>{visibleText(execution.program)}</code></dd></>}
        <dt>인자</dt><dd><code>{execution.args?.length ? execution.args.map(visibleText).join(" ") : "(없음)"}</code></dd>
        <dt>정책</dt><dd>{execution.policy === "allowed" ? "허용된 조회 작업" : "실행 불가"}</dd>
        {execution.jobId && <><dt>작업</dt><dd>{visibleText(execution.jobId)}</dd></>}
      </dl>
    </details>
  </section>;
}
