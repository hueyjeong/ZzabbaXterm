// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AiPanel } from "./AiPanel";
import type { AiPanelProps, ChatReply, ExecutionStatus, Proposal } from "./contracts";

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
    onDecideProposal: vi.fn<AiPanelProps["onDecideProposal"]>().mockImplementation(async ({ decision }) =>
      ({ approve: "approved", reject: "rejected", cancel: "cancelled" })[decision] as "approved" | "rejected" | "cancelled"),
  };
}

async function render(props: AiPanelProps) {
  await act(async () => { root.render(<><textarea aria-label="터미널 입력" /><AiPanel {...props} /></>); });
}

function button(label: string) {
  return [...host.querySelectorAll("button")].find((element) =>
    element.textContent === label || element.getAttribute("aria-label") === label);
}

async function click(label: string) {
  const element = button(label);
  expect(element, label).toBeDefined();
  await act(async () => { element!.click(); });
}

const chip = (label: string) => [...host.querySelectorAll<HTMLButtonElement>('button[aria-haspopup="menu"]')]
  .find((element) => element.getAttribute("aria-label")?.startsWith(`${label}: `));

async function select(label: string, option: string) {
  const trigger = chip(label);
  expect(trigger, label).toBeDefined();
  await act(async () => { trigger!.click(); });
  const item = [...host.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')].find((element) => element.textContent === option);
  expect(item, option).toBeDefined();
  await act(async () => { item!.click(); });
  expect(host.querySelector('[role="menu"]')).toBeNull();
}

const fields = (article: Element | null | undefined) =>
  [...article?.querySelectorAll("[data-field]") ?? []].map((element) => element.textContent);

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
    // 사용자가 고르기 전에는 첫 모델을 쓴다.
    expect(chip("모델")?.textContent).toBe("local:latest");
    await click("전송");
    expect(document.activeElement).toBe(input());
    expect(host.querySelector('[role="log"]')?.textContent).toContain(reply);
    expect(host.querySelector("img, article")).toBeNull();
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
    await select("모델", "local:latest");
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
    await select("모델", "local:latest");
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

it("채팅과 제안 카드를 하나의 대화 기록에 시간 순서로 표시하고 대상 전환 중에도 초안을 유지한다", async () => {
  const props = setup();
  await render(props);
  const chat = host.querySelector<HTMLElement>(".workspace-chat")!;
  expect(chat.querySelector("h2")?.textContent).toBe("AI");
  expect(chat.getAttribute("aria-labelledby")).toBe(chat.querySelector("h2")?.id);
  expect(host.querySelector('[role="tablist"]')).toBeNull();
  expect(host.querySelector(".a01-empty")?.textContent).toBe("무엇을 도와드릴까요?");
  expect(button("가동 시간 확인")).toBeUndefined();
  expect(chip("조회 대상")?.textContent).toBe("대상 선택");
  expect(chip("모델")?.textContent).toBe("local:latest");

  await select("모델", "local:latest");
  await select("조회 대상", "개발 (a)");
  expect(chip("조회 대상")?.getAttribute("aria-label")).toBe("조회 대상: 개발 (a)");
  await type("첫 질문");
  await click("전송");
  await click("가동 시간 확인");
  await type("두 번째 질문");
  await click("전송");
  const roles = () => [...host.querySelector('[role="log"]')!.children]
    .map((element) => element.tagName === "ARTICLE" ? "카드" : element.getAttribute("data-role"));
  expect(roles()).toEqual(["user", "assistant", "카드", "user", "assistant"]);
  expect(host.querySelector(".a01-empty")).toBeNull();
  expect(fields(host.querySelector("article"))).toEqual(["uptime", "dev-01\\u{1b}", card.interpretation]);

  await type("보존할 초안");
  const draft = input();
  await select("조회 대상", "개발 (b)");
  expect(input()).toBe(draft);
  expect(draft.value).toBe("보존할 초안");
  expect(roles()).toEqual(["user", "assistant", "user", "assistant"]);
  await select("조회 대상", "개발 (a)");
  expect(roles()).toEqual(["user", "assistant", "user", "assistant"]);
  expect(draft.value).toBe("보존할 초안");
  expect(props.onRequestProposal).toHaveBeenCalledTimes(1);
  expect(props.onDecideProposal).not.toHaveBeenCalled();
});

it("새 대화는 대화 기록만 비우고 초안과 제안 카드는 유지한다", async () => {
  const props = setup();
  await render({ ...props, proposals: [card] });
  expect(button("새 대화")?.disabled).toBe(true);
  await select("모델", "local:latest");
  await select("조회 대상", "개발 (a)");
  await type("질문");
  await click("전송");
  await type("남길 초안");
  expect(button("새 대화")?.disabled).toBe(false);
  await click("새 대화");
  expect(host.querySelectorAll(".a01-msg")).toHaveLength(0);
  expect(host.querySelectorAll("article")).toHaveLength(1);
  expect(input().value).toBe("남길 초안");
  expect(document.activeElement).toBe(input());
  await click("전송");
  expect(props.onSendChat.mock.calls[1]).toEqual([{ model: "local:latest", messages: [{ role: "user", content: "남길 초안" }] }]);
});

describe("입력 키", () => {
  async function key(init: KeyboardEventInit) {
    const event = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true, ...init });
    await act(async () => { input().dispatchEvent(event); });
    return event;
  }

  it("Enter로 전송하고 Shift+Enter는 줄바꿈으로 남긴다", async () => {
    const props = setup();
    await render(props);
    await select("모델", "local:latest");
    await type("첫 줄");
    const shifted = await key({ shiftKey: true });
    expect(shifted.defaultPrevented).toBe(false);
    expect(props.onSendChat).not.toHaveBeenCalled();
    const enter = await key({});
    expect(enter.defaultPrevented).toBe(true);
    expect(props.onSendChat.mock.calls).toEqual([[{ model: "local:latest", messages: [{ role: "user", content: "첫 줄" }] }]]);
    expect(input().value).toBe("");
    expect(host.querySelector('[role="log"]')?.textContent).toBe("나첫 줄AI응답");
  });

  it.each([{ isComposing: true }, { keyCode: 229 }])("IME 조합 중 Enter는 전송하지 않는다 (%o)", async (init) => {
    const props = setup();
    await render(props);
    await select("모델", "local:latest");
    await type("한글 조합");
    const event = await key(init);
    expect(event.defaultPrevented).toBe(false);
    expect(props.onSendChat).not.toHaveBeenCalled();
    expect(input().value).toBe("한글 조합");
  });

  it("모델이 없으면 Enter로 전송하지 않는다", async () => {
    const props = { ...setup(), models: [] };
    await render(props);
    await type("보낼 수 없는 문장");
    await key({});
    expect(props.onSendChat).not.toHaveBeenCalled();
    expect(input().value).toBe("보낼 수 없는 문장");
  });
});

describe("제안 검토", () => {
  it.each(["target", "generation", "removed", "cancel"])("대상 전환·삭제 또는 취소(%s) 후 이전 응답을 버린다", async (change) => {
    const props = setup();
    const pending = Promise.withResolvers<Proposal>();
    props.onRequestProposal.mockReturnValueOnce(pending.promise);
    await render(props);
    await select("모델", "local:latest");
    await select("조회 대상", "개발 (a)");
    await type("채팅 초안");
    await click("가동 시간 확인");
    expect(props.onRequestProposal.mock.calls[0][0]).toEqual({
      model: "local:latest", target: { targetId: "a", generation: "1" }, operation: "uptime",
    });
    await render({ ...props, targets: [{ ...alpha }, beta] });
    expect(props.onRequestProposal.mock.calls[0][1].aborted).toBe(false);
    expect(host.querySelector(".a01-proposals [role=status]")?.textContent).toBe("제안 요청 중");
    if (change === "target") {
      await select("조회 대상", "개발 (b)");
      await select("조회 대상", "개발 (a)");
    } else if (change === "generation") {
      await render({ ...props, targets: [{ ...alpha, generation: "2" }, beta] });
      await select("조회 대상", "개발 (a)");
    } else if (change === "removed") {
      await render({ ...props, targets: [beta] });
    } else {
      await click("요청 취소");
    }
    expect(props.onRequestProposal.mock.calls[0][1].aborted).toBe(true);
    expect(button("요청 취소")).toBeUndefined();
    if (change === "removed") await render(props);
    await act(async () => { pending.resolve(card); });
    expect(host.querySelector("article")).toBeNull();
    expect(host.querySelector(".a01-proposals [role=alert]")).toBeNull();
    expect(input().value).toBe("채팅 초안");
    expect(props.onSendChat).not.toHaveBeenCalled();
    expect(props.onDecideProposal).not.toHaveBeenCalled();
  });

  it("같은 표시명의 다른 대상이 응답하면 오류로 표시한다", async () => {
    const props = setup();
    props.onRequestProposal.mockResolvedValueOnce({ ...card, target: beta });
    await render(props);
    await select("모델", "local:latest");
    await select("조회 대상", "개발 (a)");
    await click("가동 시간 확인");
    expect(host.querySelector('[role="alert"]')?.textContent).toBe("제안 요청 실패");
    expect(button("승인")).toBeUndefined();
  });

  it("조회 카드의 대상·명령·해석을 표시하고 중복 승인을 한 번만 기록한다", async () => {
    const props = setup();
    await render({ ...props, proposals: [card, card] });
    await select("조회 대상", "개발 (a)");
    const [article] = host.querySelectorAll("article");
    expect(fields(article)).toEqual(["uptime", "dev-01\\u{1b}", card.interpretation]);
    expect(host.querySelectorAll("article")).toHaveLength(1);
    const approval = button("승인")!;
    await act(async () => { approval.click(); approval.click(); });
    expect(props.onDecideProposal.mock.calls).toEqual([[{ proposalId: "p1", decision: "approve" }]]);
    expect(article.querySelector("[role=status]")?.textContent).toBe("승인됨");
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
    const decision = Promise.withResolvers<"approved">();
    props.onDecideProposal.mockReturnValueOnce(decision.promise);
    const second = { ...card, id: "p2", node: "dev-02", target: beta };
    await render({ ...props, proposals: [card, second] });
    await select("조회 대상", "개발 (a)");
    await click("승인");
    await select("조회 대상", "개발 (b)");
    await act(async () => { decision.resolve("approved"); });
    expect(host.querySelector("article [data-field=node]")?.textContent).toBe("dev-02");
    expect(host.querySelector("article [role=status]")).toBeNull();
    await select("조회 대상", "개발 (a)");
    expect(host.querySelector("article [role=status]")?.textContent).toBe("검토 종료");
    expect(button("승인")).toBeUndefined();
  });

  it("같은 노드 ID라도 실제 SSM 프로필과 리전을 구분해 표시한다", async () => {
    const first = { ...card, ssm: { profile: "ops-a", region: "ap-northeast-2" } };
    const second = {
      ...card, kind: "blocked" as const, id: "p2", target: beta,
      ssm: { profile: "ops-b", region: "us-west-2" }
    };
    await render({ ...setup(), proposals: [first, second] });
    const values = () => fields(host.querySelector("article"));
    await select("조회 대상", "개발 (a)");
    expect(values()).toEqual(["uptime", "dev-01\\u{1b}", "ops-a", "ap-northeast-2", card.interpretation]);
    await select("조회 대상", "개발 (b)");
    expect(values()).toEqual(["dev-01\\u{1b}", "ops-b", "us-west-2"]);
  });

  it.each([false, true])("결정 활성화 후 포커스를 이어 받고 다른 탭 입력은 유지한다 (%s)", async (move) => {
    const props = setup();
    const response = Promise.withResolvers<"rejected">();
    props.onDecideProposal.mockReturnValueOnce(response.promise);
    await render({ ...props, proposals: [card] });
    await select("조회 대상", "개발 (a)");
    button("승인")!.focus();
    await click("승인");
    const status = host.querySelector("article [role=status]");
    expect(status?.textContent).toBe("기록 중");
    expect(document.activeElement).toBe(status);
    const terminal = host.querySelector("textarea")!;
    if (move) terminal.focus();
    await act(async () => { response.resolve("rejected"); });
    expect(status?.textContent).toBe("거절됨");
    expect(document.activeElement).toBe(move ? terminal : status);
  });
});

describe("승인된 작업 실행", () => {
  const history = [{ proposal: card, decision: "approved" as const }];

  it("승인된 카드에 대상·절대 경로·인자·정책을 표시하고 중지와 종료 상태를 구분한다", async () => {
    const props = setup();
    const execute = vi.fn<NonNullable<AiPanelProps["onExecuteProposal"]>>().mockImplementation(async () => {});
    const stop = vi.fn<NonNullable<AiPanelProps["onStopExecution"]>>().mockImplementation(async () => {});
    await render({ ...props, history, onExecuteProposal: execute, onStopExecution: stop });
    await select("조회 대상", "개발 (a)");
    expect(button("실행")).toBeDefined();
    await click("실행");
    expect(execute.mock.calls).toEqual([["p1", "uptime"]]);

    const status: ExecutionStatus = {
      proposalId: "p1", approvalId: "3", jobId: "3", taskId: "uptime", sessionId: "7", selectionId: "2",
      node: "dev-01", account: "zzabba-ai-control", program: "/usr/bin/uptime", args: [], policy: "allowed",
      phase: "running", output: " 10:32 up 2 days\n",
    };
    const rerender = async (next: ExecutionStatus) => act(async () => {
      root.render(<><textarea aria-label="터미널 입력" /><AiPanel {...props} history={history}
        execution={next} onExecuteProposal={execute} onStopExecution={stop} /></>);
    });
    await rerender(status);
    const panel = host.querySelector(".a01-execution");
    expect(panel?.textContent).toContain("dev-01");
    expect(panel?.textContent).toContain("/usr/bin/uptime");
    expect(panel?.textContent).toContain("(없음)");
    expect(panel?.textContent).toContain("허용된 조회 작업");
    expect(panel?.textContent).toContain("실행 중");
    expect(panel?.textContent).toContain("10:32 up 2 days");
    await click("중지");
    expect(stop.mock.calls).toEqual([["3"]]);

    await rerender({ ...status, phase: "unconfirmed", error: "제어 채널이 끊겼습니다" });
    expect(host.querySelector(".a01-execution")?.textContent).toContain("종료 확인 불가");
    await rerender({ ...status, phase: "exited", code: 0 });
    expect(host.querySelector(".a01-execution")?.textContent).toContain("종료됨");
  });
});
