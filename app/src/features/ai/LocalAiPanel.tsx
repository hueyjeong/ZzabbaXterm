import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";
import { AiPanel } from "./AiPanel";
import type { AiPanelProps, ChatReply, Proposal, ProposalTarget, TargetRef } from "./contracts";

const sendChat: AiPanelProps["onSendChat"] = async (request) => {
  try {
    return await invoke<ChatReply>("ai_chat", request);
  } catch (error) {
    return { kind: "error", message: String(error) };
  }
};

const decide: AiPanelProps["onDecideProposal"] = (request) => invoke("ai_decide", request);

export function LocalAiPanel({ profilesVersion }: { profilesVersion: number }) {
  const [models, setModels] = useState<readonly string[]>([]);
  const [targets, setTargets] = useState<readonly ProposalTarget[]>([]);
  const [modelError, setModelError] = useState("");
  const [targetError, setTargetError] = useState("");
  const [retry, setRetry] = useState(0);
  const selection = useRef<Promise<boolean>>(Promise.resolve(false));

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

  function select(target: TargetRef | null) {
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

  const propose: AiPanelProps["onRequestProposal"] = async (request) => {
    const selected = selection.current;
    if (!await selected || selection.current !== selected) throw new Error("조회 대상을 다시 선택하세요");
    return invoke<Proposal>("ai_propose", request);
  };

  return <AiPanel models={models} targets={targets} proposals={[]} onSelectTarget={select}
    modelError={modelError} targetError={targetError} onRefresh={() => setRetry((value) => value + 1)}
    onSendChat={sendChat} onRequestProposal={propose} onDecideProposal={decide} />;
}
