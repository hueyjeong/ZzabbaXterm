export type ChatMessage = Readonly<{ role: "user" | "assistant"; content: string }>;
export type ChatRequest = Readonly<{ model: string; messages: readonly ChatMessage[] }>;
export type ChatReply =
  | Readonly<{ kind: "reply"; content: string }>
  | Readonly<{ kind: "error"; message: string }>;

export type TargetRef = Readonly<{ targetId: string; generation: string }>;
export type ProposalTarget = TargetRef & Readonly<{ label: string }>;
export type ProposalRequest = Readonly<{ target: TargetRef; model: string; operation: "uptime" }>;
export type Proposal = Readonly<{ id: string; target: TargetRef }> & (
  | Readonly<{ kind: "reviewable"; node: string; displayCommand: "uptime"; interpretation: string }>
  | Readonly<{ kind: "blocked" | "unregistered" }>
);
export type ProposalDecision = Readonly<{ proposalId: string; decision: "approve" | "reject" | "cancel" }>;

// Rust resolves targets and revalidates the recorded proposal by ID; a decision is not an execution request.
export type AiPanelProps = Readonly<{
  models: readonly string[];
  targets: readonly ProposalTarget[];
  proposals: readonly Proposal[];
  onSelectTarget?: (target: TargetRef | null) => void;
  modelError?: string;
  targetError?: string;
  onRefresh?: () => void;
  onOpenCliReview?: () => void;
  policyControl?: React.ReactNode;
  onSendChat: (request: ChatRequest) => Promise<ChatReply>;
  onRequestProposal: (request: ProposalRequest) => Promise<Proposal>;
  onDecideProposal: (decision: ProposalDecision) => Promise<"recorded" | "rejected">;
}>;
