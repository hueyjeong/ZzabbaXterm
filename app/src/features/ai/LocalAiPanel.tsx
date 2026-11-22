import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";
import { AiPanel } from "./AiPanel";
import type { AiPanelProps, ChatReply, DecisionRecord, Proposal, ProposalTarget, RecordedDecision, TargetRef } from "./contracts";

const sendChat: AiPanelProps["onSendChat"] = async (request) => {
  try {
    return await invoke<ChatReply>("ai_chat", request);
  } catch (error) {
    return { kind: "error", message: String(error) };
  }
};

let nextRequestId = 0;

export function LocalAiPanel({ profilesVersion }: { profilesVersion: number }) {
  const [models, setModels] = useState<readonly string[]>([]);
  const [history, setHistory] = useState<readonly DecisionRecord[]>([]);
  const [policyVersion, setPolicyVersion] = useState(0);
  const historyRequest = useRef(0);
  const [targets, setTargets] = useState<readonly ProposalTarget[]>([]);
  const [modelError, setModelError] = useState("");
  const [targetError, setTargetError] = useState("");
  const [retry, setRetry] = useState(0);
  const [blocked, setBlocked] = useState<boolean>();
  const [savingPolicy, setSavingPolicy] = useState(false);
  const [policyError, setPolicyError] = useState("");
  const policyPending = useRef(false);
  const [cliCard, setCliCard] = useState<Proposal>();
  const opening = useRef(false);
  const selection = useRef<Promise<boolean>>(Promise.resolve(false));

  useEffect(() => {
    let current = true;
    void invoke<boolean>("ai_policy", {}).then((value) => {
      if (current) setBlocked(value);
    }, (error: unknown) => {
      if (current) setPolicyError(String(error));
    });
    return () => { current = false; };
  }, []);

  useEffect(() => {
    let current = true;
    setModelError("");
    void invoke<string[]>("ai_models", {}).then((value) => {
      if (current) setModels(value);
    }, (error: unknown) => {
      if (current) {
        setModels([]);
        setModelError(String(error));
      }
    });
    return () => { current = false; };
  }, [retry]);

  useEffect(() => {
    let current = true;
    setTargetError("");
    void invoke<ProposalTarget[]>("ai_targets", {}).then((value) => {
      if (current) setTargets(value);
    }, (error: unknown) => {
      if (current) {
        setTargets([]);
        setTargetError(String(error));
      }
    });
    return () => { current = false; };
  }, [profilesVersion, retry]);

  async function readHistory() {
    const version = ++historyRequest.current;
    const records = await invoke<DecisionRecord[]>("ai_decisions", {});
    if (version === historyRequest.current) setHistory(records);
    return records;
  }

  useEffect(() => {
    void readHistory().catch(console.error);
    return () => { historyRequest.current++; };
  }, []);

  async function readDecision(proposalId: string) {
    const records = await readHistory();
    return records.find(({ proposal }) => proposal.id === proposalId)?.decision;
  }

  const decide: AiPanelProps["onDecideProposal"] = async (request) => {
    try {
      const result = await invoke<RecordedDecision>("ai_decide", request);
      void readHistory().catch(console.error);
      return result;
    } catch (error) {
      const existing = await readDecision(request.proposalId);
      if (existing) return existing;
      throw error;
    }
  };

  function select(target: TargetRef | null) {
    setCliCard(undefined);
    void readHistory().catch(console.error);
    setTargetError("");
    const next = selection.current.then(() => invoke<boolean>("ai_select", { target })).catch((error: unknown) => {
      if (selection.current === next) setTargetError(String(error));
      return false;
    });
    selection.current = next;
    void next.then((accepted) => {
      if (target && !accepted && selection.current === next) {
        setTargetError("선택한 조회 대상이 바뀌었습니다");
      }
    });
  }

  const propose: AiPanelProps["onRequestProposal"] = async (request, signal) => {
    const selected = selection.current;
    if (!await selected || selection.current !== selected) throw new Error("조회 대상을 다시 선택하세요");
    signal.throwIfAborted();
    const requestId = ++nextRequestId;
    const cancel = () => { void invoke("ai_cancel_proposal", { requestId }).catch(console.error); };
    signal.addEventListener("abort", cancel, { once: true });
    try {
      return await invoke<Proposal>("ai_propose", { ...request, requestId });
    } finally {
      signal.removeEventListener("abort", cancel);
    }
  };

  async function openCliReview() {
    if (opening.current) return;
    opening.current = true;
    const selected = selection.current;
    setTargetError("");
    try {
      if (!await selected || selection.current !== selected) return;
      const card = await invoke<Proposal | null>("ai_cli_review", {});
      if (selection.current !== selected) return;
      if (card) setCliCard(card);
      else setTargetError("접수된 CLI 제안이 없습니다");
    } catch (error) {
      if (selection.current === selected) setTargetError(String(error));
    } finally {
      opening.current = false;
    }
  }

  async function updatePolicy(next: boolean) {
    if (policyPending.current) return;
    policyPending.current = true;
    setSavingPolicy(true);
    setPolicyError("");
    try {
      await invoke("ai_set_policy", { blocked: next });
      setBlocked(next);
      setCliCard(undefined);
      setPolicyVersion((value) => value + 1);
      void readHistory().catch(console.error);
    } catch (error) {
      setPolicyError(String(error));
    } finally {
      setSavingPolicy(false);
      policyPending.current = false;
    }
  }

  return <AiPanel models={models} targets={targets} proposals={cliCard ? [cliCard] : []} onSelectTarget={select}
    history={history} policyVersion={policyVersion} onReadDecision={readDecision}
    onOpenCliReview={openCliReview}
    policyControl={<>
      {blocked !== undefined && <label><input type="checkbox" checked={blocked} disabled={savingPolicy}
        onChange={(event) => void updatePolicy(event.currentTarget.checked)} /> 조회 승인 차단</label>}
      {policyError && <p role="alert">{policyError}</p>}
    </>}
    modelError={modelError} targetError={targetError} onRefresh={() => setRetry((value) => value + 1)}
    onSendChat={sendChat} onRequestProposal={propose} onDecideProposal={decide} />;
}
