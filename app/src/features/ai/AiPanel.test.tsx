// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AiPanel } from "./AiPanel";
import type { AiPanelProps, ChatReply, Proposal } from "./contracts";

const alpha = { targetId: "a", generation: "1", label: "개발" };
const beta = { targetId: "b", generation: "1", label: "개발" };
const card = {
  kind: "reviewable", id: "p1", target: { targetId: "a", generation: "1" },
  node: "dev-01\u001b", displayCommand: "uptime", interpretation: "선택한 노드의 가동 시간과 부하를 조회합니다",
} satisfies Proposal;
let host: HTMLDivElement;
let root: Root;

function setup() {
  return {
    models: ["local:latest"], targets: [alpha, beta], proposals: [] as Proposal[],
    onSendChat: vi.fn<AiPanelProps["onSendChat"]>().mockResolvedValue({ kind: "reply", content: "응답" }),
    onRequestProposal: vi.fn<AiPanelProps["onRequestProposal"]>().mockResolvedValue(card),
    onDecideProposal: vi.fn<AiPanelProps["onDecideProposal"]>().mockResolvedValue("recorded"),
  };
}

async function render(props: AiPanelProps) {
  await act(async () => { root.render(<><textarea aria-label="터미널 입력" /><AiPanel {...props} /></>); });
}

function button(label: string) {
  return [...host.querySelectorAll("button")].find((element) => element.textContent === label);
}

async function click(label: string) {
  const element = button(label);
  expect(element, label).toBeDefined();
  await act(async () => { element!.click(); });
}

async function select(label: string, option: string) {
  const field = [...host.querySelectorAll("label")].find((element) => element.firstChild?.textContent === label)!
    .querySelector("select")!;
  await act(async () => {
    field.value = [...field.options].find((element) => element.textContent?.trim() === option)!.value;
    field.dispatchEvent(new Event("change", { bubbles: true }));
  });
}

const input = () => host.querySelector<HTMLTextAreaElement>('textarea[aria-label="메시지 입력"]')!;
async function type(value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(input(), value);
    input().dispatchEvent(new InputEvent("input", { bubbles: true }));
  });
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => { root.unmount(); });
  host.remove();
  vi.unstubAllGlobals();
});

describe("일반 대화", () => {
  it("선택한 모델에 대화만 보내고 응답의 HTML과 명령을 텍스트로 표시한다", async () => {
    const props = setup();
    const reply = '<img src="invalid"><code>uptime</code> {"operation":"uptime"}';
    props.onSendChat.mockResolvedValueOnce({ kind: "reply", content: reply });
    await render(props);
    await select("조회 대상", "개발 (a)");
    await type("echo CHAT_ONLY");
    expect(button("전송")?.disabled).toBe(true);
    await select("Ollama 모델", "local:latest");
    await click("전송");
    expect(document.activeElement).toBe(input());
    expect(host.querySelector('[role="log"]')?.textContent).toContain(reply);
    expect(host.querySelector("img, article")).toBeNull();
    expect(host.querySelector(".a01-proposals [role=status]")?.textContent).toBe("대기");
    await type("무슨 말을 했지?");
    await click("전송");
    expect(props.onSendChat.mock.calls).toEqual([[{
      model: "local:latest", messages: [
        { role: "user", content: "echo CHAT_ONLY" },
      ]
    }], [{
      model: "local:latest", messages: [
        { role: "user", content: "echo CHAT_ONLY" }, { role: "assistant", content: reply },
        { role: "user", content: "무슨 말을 했지?" },
      ]
    }]]);
    expect(props.onRequestProposal).not.toHaveBeenCalled();
    expect(props.onDecideProposal).not.toHaveBeenCalled();
    expect(host.querySelector("textarea")?.value).toBe("");
  });

  it("실패한 전송은 초안을 유지하고 늦은 오류는 사용자가 옮긴 포커스를 뺏지 않는다", async () => {
    const props = setup();
    const reply = Promise.withResolvers<ChatReply>();
    props.onSendChat.mockReturnValueOnce(reply.promise);
    await render(props);
    await select("Ollama 모델", "local:latest");
    await type("다시 보낼 문장");
    await click("전송");
    const terminal = host.querySelector("textarea")!;
    terminal.focus();
    await act(async () => { reply.resolve({ kind: "error", message: "로컬 모델 연결 오류" }); });
    expect(document.activeElement).toBe(terminal);
    expect(host.querySelector('[role="alert"]')?.textContent).toBe("전송 실패: 로컬 모델 연결 오류");
    expect(input().value).toBe("다시 보낼 문장");
    await click("전송");
    expect(props.onSendChat.mock.calls[1]).toEqual([{
      model: "local:latest", messages: [
        { role: "user", content: "다시 보낼 문장" },
      ]
    }]);
  });

  it("요청 크기 오류가 나면 기존 대화와 초안을 유지한다", async () => {
    const props = setup();
    await render(props);
    await select("Ollama 모델", "local:latest");
    await type("첫 질문");
    await click("전송");
    const message = "Ollama 요청 크기 한도를 초과했습니다. 전송되지 않았습니다.";
    props.onSendChat.mockResolvedValueOnce({ kind: "error", message });
    const oversized = "한".repeat(11_000);
    await type(oversized);
    await click("전송");
    expect(host.querySelector('[role="alert"]')?.textContent).toBe(`전송 실패: ${message}`);
    expect(host.querySelector('[role="log"]')?.textContent).toBe("나첫 질문AI응답");
    expect(input().value).toBe(oversized);
    expect(props.onRequestProposal).not.toHaveBeenCalled();
    expect(props.onDecideProposal).not.toHaveBeenCalled();
  });
});

describe("제안 검토", () => {
  it.each(["target", "generation", "cancel"])("대상 전환 또는 취소(%s) 후 이전 응답을 버린다", async (change) => {
    const props = setup();
    const pending = Promise.withResolvers<Proposal>();
    props.onRequestProposal.mockReturnValueOnce(pending.promise);
    await render(props);
    await select("Ollama 모델", "local:latest");
    await select("조회 대상", "개발 (a)");
    await type("채팅 초안");
    await click("가동 시간 조회 제안");
    expect(props.onRequestProposal.mock.calls).toEqual([[{
      model: "local:latest", target: { targetId: "a", generation: "1" }, operation: "uptime",
    }]]);
    if (change === "target") {
      await select("조회 대상", "개발 (b)");
      await select("조회 대상", "개발 (a)");
    } else if (change === "generation") {
      await render({ ...props, targets: [{ ...alpha, generation: "2" }, beta] });
      await select("조회 대상", "개발 (a)");
    } else {
      await click("요청 취소");
    }
    await act(async () => { pending.resolve(card); });
    expect(host.querySelector("article")).toBeNull();
    expect(host.querySelector(".a01-proposals [role=status]")?.textContent).toBe("대기");
    expect(input().value).toBe("채팅 초안");
    expect(props.onSendChat).not.toHaveBeenCalled();
    expect(props.onDecideProposal).not.toHaveBeenCalled();
  });

  it("같은 표시명의 다른 대상이 응답하면 오류로 표시한다", async () => {
    const props = setup();
    props.onRequestProposal.mockResolvedValueOnce({ ...card, target: beta });
    await render(props);
    await select("Ollama 모델", "local:latest");
    await select("조회 대상", "개발 (a)");
    await click("가동 시간 조회 제안");
    expect(host.querySelector('[role="alert"]')?.textContent).toBe("제안 요청 실패");
    expect(button("승인")).toBeUndefined();
  });

  it("조회 카드의 대상·명령·해석을 표시하고 중복 승인을 한 번만 기록한다", async () => {
    const props = setup();
    await render({ ...props, proposals: [card, card] });
    await select("조회 대상", "개발 (a)");
    const [article] = host.querySelectorAll("article");
    expect([...article.querySelectorAll("dd")].map((element) => element.textContent)).toEqual([
      "dev-01\\u{1b}", "uptime", card.interpretation,
    ]);
    expect(host.querySelectorAll("article")).toHaveLength(1);
    const approval = button("승인")!;
    await act(async () => { approval.click(); approval.click(); });
    expect(props.onDecideProposal.mock.calls).toEqual([[{ proposalId: "p1", decision: "approve" }]]);
    expect(article.textContent).toContain("승인 기록됨");
    expect(props.onSendChat).not.toHaveBeenCalled();
    expect(props.onRequestProposal).not.toHaveBeenCalled();
  });

  it.each(["reviewable", "blocked", "unregistered"] as const)("%s 카드의 취소를 기록하고 재승인을 막는다", async (kind) => {
    const props = setup();
    await render({ ...props, proposals: [{ ...card, kind }] });
    await select("조회 대상", "개발 (a)");
    if (kind !== "reviewable") {
      expect(button("승인")).toBeUndefined();
      expect(host.querySelector("article code")).toBeNull();
      expect(host.querySelector("article")?.textContent).toContain(kind === "blocked" ? "차단된 조회" : "미등록 조회");
    }
    await click("취소");
    expect(props.onDecideProposal.mock.calls).toEqual([[{ proposalId: "p1", decision: "cancel" }]]);
    expect(host.querySelector("article [role=status]")?.textContent).toBe("취소됨");
    expect(button("승인")).toBeUndefined();
    expect(button("취소")).toBeUndefined();
  });

  it("검토 중 대상을 바꾸면 이전 결정 응답을 다른 카드에 적용하지 않는다", async () => {
    const props = setup();
    const decision = Promise.withResolvers<"recorded">();
    props.onDecideProposal.mockReturnValueOnce(decision.promise);
    const second = { ...card, id: "p2", node: "dev-02", target: beta };
    await render({ ...props, proposals: [card, second] });
    await select("조회 대상", "개발 (a)");
    await click("승인");
    await select("조회 대상", "개발 (b)");
    await act(async () => { decision.resolve("recorded"); });
    expect(host.querySelector("article dd")?.textContent).toBe("dev-02");
    expect(host.querySelector("article [role=status]")).toBeNull();
    await select("조회 대상", "개발 (a)");
    expect(host.querySelector("article [role=status]")?.textContent).toBe("검토 종료");
    expect(button("승인")).toBeUndefined();
  });
});
