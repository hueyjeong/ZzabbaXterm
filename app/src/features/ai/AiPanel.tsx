import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type MouseEvent, type ReactNode, type RefObject } from "react";
import {
  Activity, ArrowUp, Ban, Check, ChevronDown, CircleAlert, CircleCheck, CircleHelp, ClipboardPaste, Cloud, Copy, Crosshair, Ellipsis,
  Globe, Inbox, LoaderCircle, Play, RefreshCw, Scissors, Server, ShieldAlert, ShieldCheck, ShieldOff, Sparkles, Square, SquarePen,
  SquareTerminal, TextCursorInput, TextSelect, X,
} from "lucide-react";
import { copyText, readText } from "../ui/clipboard";
import { IconButton, type MenuItem } from "../ui/controls";
import { useContextMenu } from "../ui/useContextMenu";
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
type ConversationMenu = Readonly<{ kind: "card"; id: string } | { kind: "message"; seq: number; selection: string }>;

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

/** 요소 안에서 고른 글자다. 선택이 요소 밖으로 걸치면 빈 문자열이다. */
function selectedText(element: HTMLElement) {
  const selection = element.ownerDocument.getSelection();
  if (!selection || selection.isCollapsed || !element.contains(selection.anchorNode) || !element.contains(selection.focusNode)) return "";
  return selection.toString();
}

/**
 * 입력창의 범위를 글자로 바꾼다. execCommand가 되면 실행 취소 기록에 남고 브라우저가 input 이벤트를 보낸다.
 * 안 되면 setRangeText 뒤 input 이벤트를 직접 보내 React onChange가 값을 받게 한다.
 */
function replaceRange(element: HTMLTextAreaElement, text: string, start: number, end: number) {
  if (!text && start === end) return;
  element.focus();
  element.setSelectionRange(start, end);
  const { ownerDocument } = element;
  if (typeof ownerDocument.execCommand === "function" && ownerDocument.execCommand(text ? "insertText" : "delete", false, text)) return;
  element.setRangeText(text, start, end, "end");
  element.dispatchEvent(new Event("input", { bubbles: true }));
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
  const context = useContextMenu<"log" | "target">();
  const { policy } = props;
  const canReset = chat.kind !== "sending" && entries.length > 0;

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

  function newChat() {
    setEntries([]);
    setChat({ kind: "idle" });
    stick.current = true;
    input.current?.focus();
  }

  /** 입력창의 커서 위치에 글자를 넣는다. 전송 중에는 넣지 않는다. */
  function insertDraft(text: string) {
    const element = input.current;
    if (!element || element.readOnly) return;
    replaceRange(element, text, element.selectionStart, element.selectionEnd);
  }

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
  function logItems(): MenuItem[] {
    const transcript = entries.map(({ message }) => `${message.role === "user" ? "나" : "AI"}: ${message.content}`).join("\n\n");
    return [
      { label: "대화 복사", icon: Copy, disabled: entries.length === 0, onSelect: () => copyText(transcript) },
      { label: "새 대화", icon: SquarePen, disabled: !canReset, onSelect: newChat },
    ];
  }
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
          <IconButton icon={SquarePen} label="새 대화" disabled={!canReset} onClick={newChat} />
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
      }} onContextMenu={(event) => context.open(event, "log", "대화 기록 메뉴")}
      onKeyDown={(event) => context.open(event, "log", "대화 기록 메뉴")}>
        <Conversation key={JSON.stringify([selected, !!target, target && props.policyVersion])} model={model} target={target}
          entries={entries} chat={chat} order={order.current} nextSeq={() => ++sequence.current} fallbackFocus={input}
          onInsertDraft={insertDraft}
          proposals={props.proposals} history={props.history} attempted={attempted.current}
          execution={props.execution} onExecuteProposal={props.onExecuteProposal} onStopExecution={props.onStopExecution}
          onRequestProposal={props.onRequestProposal} onDecideProposal={props.onDecideProposal} onReadDecision={props.onReadDecision} />
      </div>
      <div className="a01-bottom">
        {props.targetError && <p className="a01-inline-alert" role="alert">{props.targetError}</p>}
        {props.modelError && <p className="a01-inline-alert" role="alert">{props.modelError}</p>}
        {chat.kind === "failed" && <p className="a01-inline-alert" role="alert">전송 실패: {chat.message}</p>}
        <Composer models={props.models} model={model} onModelChange={setModelName} input={input} chat={chat}
          onSend={async (content) => {
            stick.current = true;
            setChat({ kind: "sending", content });
            const next: ChatMessage[] = [...entries.map(({ message }) => message), { role: "user", content }];
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
              aria-haspopup="menu" aria-expanded={targetMenu.open} onClick={targetMenu.toggle}
              onContextMenu={(event) => context.open(event, "target", "조회 대상")}
              onKeyDown={(event) => { if (!context.open(event, "target", "조회 대상")) menuKeys(targetMenu.show)(event); }}>
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
      {context.element((source) => source === "log" ? logItems() : targetItems)}
    </section>
  );
}

function Composer({ models, model, onModelChange, input, chat, onSend, children }: {
  models: readonly string[];
  model: string;
  onModelChange: (model: string) => void;
  input: RefObject<HTMLTextAreaElement | null>;
  chat: ChatState;
  onSend: (content: string) => Promise<boolean>;
  children: ReactNode;
}) {
  const [draft, setDraft] = useState("");
  const sending = useRef(false);
  const pasting = useRef(false);
  const context = useContextMenu<null>();
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

  function paste(element: HTMLTextAreaElement) {
    if (pasting.current) return;
    pasting.current = true;
    const { value, selectionStart, selectionEnd } = element;
    void readText().then((text) => {
      // 권한 확인 등으로 늦게 온 응답은 그사이 입력창이 바뀌었으면 버린다.
      if (!text || !element.isConnected || element.readOnly || element.value !== value) return;
      replaceRange(element, text, selectionStart, selectionEnd);
    }).finally(() => { pasting.current = false; });
  }

  // 메뉴 항목을 고르면 Menu가 입력창으로 포커스를 돌려 선택 영역이 그대로 남는다.
  function editItems(element: HTMLTextAreaElement): MenuItem[] {
    const selected = element.selectionStart !== element.selectionEnd;
    const selection = () => element.value.slice(element.selectionStart, element.selectionEnd);
    const items: MenuItem[] = [
      { label: "잘라내기", icon: Scissors, disabled: busy || !selected, onSelect: () => {
        if (element.readOnly) return;
        copyText(selection());
        replaceRange(element, "", element.selectionStart, element.selectionEnd);
      } },
      { label: "복사", icon: Copy, disabled: !selected, onSelect: () => copyText(selection()) },
      { label: "붙여넣기", icon: ClipboardPaste, disabled: busy, onSelect: () => paste(element) },
      { label: "모두 선택", icon: TextSelect, disabled: !element.value, onSelect: () => element.select() },
    ];
    if (draft.trim()) items.push({ kind: "separator" }, { label: "보내기", icon: ArrowUp, disabled: !model || busy, onSelect: () => void submit() });
    return items;
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
      readOnly={busy} onChange={(event) => setDraft(event.target.value)}
      onContextMenu={(event) => context.open(event, null, "입력 메뉴")} onKeyDown={(event) => {
        if (context.open(event, null, "입력 메뉴")) return;
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
    {context.element((_, element) => element instanceof HTMLTextAreaElement ? editItems(element) : [])}
  </form>;
}

function Conversation({ model, target, entries, chat, order, nextSeq, fallbackFocus, onInsertDraft, proposals, history,
  attempted, execution, onExecuteProposal, onStopExecution, onRequestProposal, onDecideProposal, onReadDecision }:
  Pick<AiPanelProps, "proposals" | "history" | "execution" | "onExecuteProposal" | "onStopExecution" | "onRequestProposal" | "onDecideProposal" | "onReadDecision"> & {
    model: string;
    target: ProposalTarget | undefined;
    entries: readonly Entry[];
    chat: ChatState;
    order: Map<string, number>;
    nextSeq: () => number;
    fallbackFocus: RefObject<HTMLTextAreaElement | null>;
    onInsertDraft: (text: string) => void;
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
  const context = useContextMenu<ConversationMenu>();
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

  async function recordDecision(proposalId: string, button: HTMLElement, read: () => Promise<RecordedDecision | undefined>) {
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

  function decide(proposal: Proposal, decision: "approve" | "reject" | "cancel", button: HTMLElement) {
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

  function decisionOf(proposal: Proposal) {
    return recorded.get(proposal.id) ?? decisions.get(proposal.id) ?? (attempted.has(proposal.id) ? "closed" : undefined);
  }

  // 승인과 실행은 카드 단추로만 받는다. 메뉴가 열린 동안에도 지금 켜진 동작만 남긴다.
  function cardItems(proposal: Proposal, article: HTMLElement): MenuItem[] {
    const decision = decisionOf(proposal);
    const items: MenuItem[] = [];
    if (proposal.kind === "reviewable") {
      items.push({ label: "명령 복사", icon: Copy, onSelect: () => copyText(visibleText(proposal.displayCommand)) });
    }
    const place = [proposal.node, ...(proposal.ssm ? [proposal.ssm.profile, proposal.ssm.region] : [])].map(visibleText).join(" / ");
    items.push({ label: "대상 복사", icon: Copy, onSelect: () => copyText(place) });
    const record = (choice: "reject" | "cancel") => () => {
      article.focus({ preventScroll: true });
      decide(proposal, choice, article);
    };
    if (!decision) {
      items.push({ kind: "separator" }, { label: "거절", icon: X, onSelect: record("reject") }, { label: "취소", onSelect: record("cancel") });
    }
    if (decision === "failed" && onReadDecision) {
      items.push({ kind: "separator" }, { label: "다시 조회", icon: RefreshCw, onSelect: () => {
        article.focus({ preventScroll: true });
        void recordDecision(proposal.id, article, () => onReadDecision(proposal.id));
      } });
    }
    return items;
  }

  function messageItems(entry: Entry, selection: string): MenuItem[] {
    const { content, role } = entry.message;
    const items: MenuItem[] = [{ label: "복사", icon: Copy, onSelect: () => copyText(content) }];
    if (selection) items.push({ label: "선택 영역 복사", icon: TextSelect, onSelect: () => copyText(selection) });
    if (role === "user") {
      items.push({ label: "입력창에 넣기", icon: TextCursorInput, disabled: chat.kind === "sending", onSelect: () => onInsertDraft(content) });
    }
    return items;
  }

  function menuItems(source: ConversationMenu, element: HTMLElement): readonly MenuItem[] {
    if (source.kind === "card") {
      const proposal = cards.get(source.id);
      return proposal ? cardItems(proposal, element) : [];
    }
    const entry = entries.find(({ seq }) => seq === source.seq);
    return entry ? messageItems(entry, source.selection) : [];
  }

  function openMessageMenu(event: MouseEvent<HTMLElement> | KeyboardEvent<HTMLElement>, entry: Entry) {
    const source: ConversationMenu = { kind: "message", seq: entry.seq, selection: selectedText(event.currentTarget) };
    context.open(event, source, entry.message.role === "user" ? "내 메시지 메뉴" : "AI 메시지 메뉴");
  }

  function card(proposal: Proposal) {
    const decision = decisionOf(proposal);
    const active = execution !== undefined && executionActive(execution.phase);
    const KindIcon = proposal.kind === "reviewable" ? SquareTerminal : proposal.kind === "blocked" ? Ban : CircleHelp;
    const source: ConversationMenu = { kind: "card", id: proposal.id };
    return <article key={proposal.id} className="a01-card" data-kind={proposal.kind} aria-label="조회 제안" tabIndex={-1}
      onContextMenu={(event) => context.open(event, source, "조회 제안 메뉴")}
      onKeyDown={(event) => context.open(event, source, "조회 제안 메뉴")}>
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
        ? <div key={`m${entry.seq}`} className="a01-msg" data-role={entry.message.role} tabIndex={-1}
          onContextMenu={(event) => openMessageMenu(event, entry)} onKeyDown={(event) => openMessageMenu(event, entry)}>
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
    {context.element(menuItems)}
  </>;
}

function Execution({ execution, onStopExecution }: Pick<AiPanelProps, "onStopExecution"> & { execution: ExecutionStatus }) {
  const { phase, approvalId } = execution;
  const active = executionActive(phase);
  const stop = phase === "running" && approvalId && onStopExecution ? () => void onStopExecution(approvalId) : undefined;
  // 메뉴를 연 순간의 출력 선택 영역을 기억한다. 항목은 렌더링마다 지금 상태로 다시 만든다.
  const context = useContextMenu<string>();
  const menuItems = (selection: string) => {
    const items: MenuItem[] = [
      { label: "출력 복사", icon: Copy, disabled: !execution.output, onSelect: () => copyText(blockText(execution.output)) },
    ];
    if (selection) items.push({ label: "선택 영역 복사", icon: TextSelect, onSelect: () => copyText(selection) });
    if (stop) items.push({ kind: "separator" }, { label: "중지", icon: Square, onSelect: stop });
    return items;
  };
  const openMenu = (event: MouseEvent<HTMLElement> | KeyboardEvent<HTMLElement>) => {
    const output = event.currentTarget.querySelector<HTMLElement>(".a01-execution-output");
    context.open(event, output ? selectedText(output) : "", "실행 상태 메뉴");
  };
  const step = EXECUTION_STEP[phase];
  const tone = phase === "exited" ? "done" : phase === "failed" ? "error" : phase === "unconfirmed" ? "warning" : "active";
  const StateIcon = active ? LoaderCircle : phase === "exited" ? CircleCheck : CircleAlert;
  return <section className="a01-execution" aria-label="실행 상태" data-tone={tone}
    onContextMenu={openMenu} onKeyDown={openMenu}>
    <div className="a01-exec-head">
      <StateIcon className={active ? "a01-exec-icon a01-spin" : "a01-exec-icon"} aria-hidden="true" />
      <span className="a01-exec-label" role="status">{EXECUTION_LABEL[phase]}</span>
      {stop &&
        <button type="button" className="ui-button danger a01-stop" onClick={stop}>
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
    {context.element(menuItems)}
  </section>;
}
