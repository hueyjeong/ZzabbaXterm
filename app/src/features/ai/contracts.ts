export type ChatMessage = Readonly<{ role: "user" | "assistant"; content: string }>;
export type ChatRequest = Readonly<{ model: string; messages: readonly ChatMessage[] }>;
export type ChatReply =
  | Readonly<{ kind: "reply"; content: string }>
  | Readonly<{ kind: "error"; message: string }>;

export type TargetRef = Readonly<{ targetId: string; generation: string }>;
export type ProposalTarget = TargetRef & Readonly<{ label: string }>;
export type ProposalRequest = Readonly<{ target: TargetRef; model: string; operation: "uptime" }>;
export type Proposal = Readonly<{
  id: string; target: TargetRef; node: string; ssm?: Readonly<{ profile: string; region: string }>;
}> & (
    | Readonly<{ kind: "reviewable"; displayCommand: "uptime"; interpretation: string }>
    | Readonly<{ kind: "blocked" | "unregistered" }>
  );
export type ProposalDecision = Readonly<{ proposalId: string; decision: "approve" | "reject" | "cancel" }>;
export type RecordedDecision = "approved" | "rejected" | "cancelled";
export type DecisionRecord = Readonly<{ proposal: Proposal; decision: RecordedDecision }>;

export type ExecutionPhase =
  | "starting" | "pairing" | "waitingClaim" | "awaitingStart" | "running"
  | "stopRequested" | "exited" | "unconfirmed" | "failed";

export type ExecutionApproval = Readonly<{
  approvalId: string;
  proposalId: string;
  taskId: "uptime";
  sessionId: string;
  selectionId: string;
  node: string;
  account: string;
  program: string;
  args: readonly string[];
  policy: "allowed";
  expiresInMs: number;
}>;

export function executionActive(phase: ExecutionPhase) {
  return phase === "starting" || phase === "pairing" || phase === "waitingClaim"
    || phase === "awaitingStart" || phase === "running" || phase === "stopRequested";
}

export type ExecutionStatus = Readonly<{
  proposalId: string;
  approvalId?: string;
  jobId?: string;
  taskId?: "uptime";
  sessionId?: string;
  selectionId?: string;
  node?: string;
  account?: string;
  program?: string;
  args?: readonly string[];
  policy?: "allowed";
  phase: ExecutionPhase;
  output: string;
  pairingCode?: string;
  code?: number;
  error?: string;
}>;

// Rust resolves targets and revalidates the recorded proposal by ID; a decision is not an execution request.
export type AiPanelProps = Readonly<{
  models: readonly string[];
  targets: readonly ProposalTarget[];
  proposals: readonly Proposal[];
  history?: readonly DecisionRecord[];
  policyVersion?: number;
  onReadDecision?: (proposalId: string) => Promise<RecordedDecision | undefined>;
  onSelectTarget?: (target: TargetRef | null) => void;
  modelError?: string;
  targetError?: string;
  onRefresh?: () => void;
  onOpenCliReview?: () => void;
  policy?: Readonly<{ blocked: boolean; saving: boolean; onChange: (blocked: boolean) => void }>;
  onSendChat: (request: ChatRequest) => Promise<ChatReply>;
  onRequestProposal: (request: ProposalRequest, signal: AbortSignal) => Promise<Proposal>;
  onDecideProposal: (decision: ProposalDecision) => Promise<RecordedDecision>;
  execution?: ExecutionStatus;
  onExecuteProposal?: (proposalId: string, taskId: "uptime") => Promise<void>;
  onStopExecution?: (approvalId: string) => Promise<void>;
}>;
