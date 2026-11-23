// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AiPanel, type AiTopTabs } from "./AiPanel";
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

async function render(props: AiPanelProps & { topTabs?: AiTopTabs }) {
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
    field.closest("details:not([open])")?.querySelector("summary")?.click();
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

it("오른쪽 탭을 전환해도 채팅 초안, SFTP 입력과 제안 검토 상태를 유지한다", async () => {
  const props = setup();
  await render(props);
  const chat = host.querySelector<HTMLElement>(".workspace-chat")!;
  const review = host.querySelector<HTMLElement>(".a01-proposals")!;
  expect(chat.contains(review)).toBe(true);
  expect(chat.querySelector("h2")?.textContent).toBe("AI 채팅");
  expect(host.querySelector('[role="tablist"]')).toBeNull();
  await select("Ollama 모델", "local:latest");
  await select("조회 대상", "개발 (a)");
  await type("채팅 초안");
  const draft = input();

  const onSelect = vi.fn<AiTopTabs["onSelect"]>();
  const item = { id: "ssh-1", label: "SFTP", content: <input aria-label="원격 경로" /> };
  const show = (selectedId: string | null) => render({ ...props, topTabs: { item, selectedId, onSelect } });
  await show("다른 세션");
  const chatTab = button("AI 채팅")!;
  const sftpTab = button("SFTP")!;
  const sftp = document.getElementById(sftpTab.getAttribute("aria-controls")!)!;
  const path = sftp.querySelector("input")!;
  expect(chatTab.closest('[role="tablist"]')).toBe(sftpTab.parentElement);
  expect(chat.getAttribute("role")).toBe("tabpanel");
  expect(chat.id).toBe(chatTab.getAttribute("aria-controls"));
  expect(chat.getAttribute("aria-labelledby")).toBe(chatTab.id);
  expect(sftp.getAttribute("aria-labelledby")).toBe(sftpTab.id);
  expect(chatTab.getAttribute("aria-selected")).toBe("true");
  expect(chatTab.tabIndex).toBe(0);
  expect(sftpTab.tabIndex).toBe(-1);
  expect(sftp.hidden && sftp.hasAttribute("inert")).toBe(true);

  await click("SFTP");
  expect(onSelect).toHaveBeenLastCalledWith(item.id);
  expect(document.activeElement).toBe(sftpTab);
  await show(item.id);
  expect(sftpTab.getAttribute("aria-selected")).toBe("true");
  expect(sftpTab.tabIndex).toBe(0);
  expect(chatTab.tabIndex).toBe(-1);
  expect(chat.hidden && chat.hasAttribute("inert")).toBe(true);
  expect(sftp.hidden || sftp.hasAttribute("inert")).toBe(false);
  path.value = "/home/dev";

  const key = (tab: HTMLButtonElement, name: string) => act(async () => {
    tab.dispatchEvent(new KeyboardEvent("keydown", { key: name, bubbles: true, cancelable: true }));
  });
  await key(sftpTab, "ArrowRight");
  expect(onSelect).toHaveBeenLastCalledWith(null);
  expect(document.activeElement).toBe(chatTab);
  await show(null);
  expect(chat.hidden || chat.hasAttribute("inert")).toBe(false);
  expect(input()).toBe(draft);
  expect(draft.value).toBe("채팅 초안");
  expect(button("전송")?.disabled).toBe(false);
  expect(chat.querySelector(".a01-proposals")).toBe(review);
  expect(review.querySelector("select")?.selectedOptions[0].textContent?.trim()).toBe("개발 (a)");
  const proposal = Promise.withResolvers<Proposal>();
  props.onRequestProposal.mockReturnValueOnce(proposal.promise);
  await click("가동 시간 조회 제안");
  expect(review.querySelector('[role="status"]')?.textContent).toBe("제안 요청 중");

  await key(chatTab, "End");
  expect(onSelect).toHaveBeenLastCalledWith(item.id);
  await show(item.id);
  expect(sftp.querySelector("input")).toBe(path);
  expect(path.value).toBe("/home/dev");
  await act(async () => { proposal.resolve(card); });
  expect(review.closest("[inert]")).toBe(chat);
  expect(sftp.querySelector(".a01-proposals")).toBeNull();
  await click("AI 채팅");
  await show(null);
  expect(button("승인")?.closest(".a01-proposals")).toBe(review);
  expect(props.onSendChat).not.toHaveBeenCalled();
  expect(props.onRequestProposal).toHaveBeenCalledTimes(1);
  expect(props.onDecideProposal).not.toHaveBeenCalled();
});

describe("제안 검토", () => {
  it.each(["target", "generation", "removed", "cancel"])("대상 전환·삭제 또는 취소(%s) 후 이전 응답을 버린다", async (change) => {
    const props = setup();
    const pending = Promise.withResolvers<Proposal>();
    props.onRequestProposal.mockReturnValueOnce(pending.promise);
    await render(props);
    await select("Ollama 모델", "local:latest");
    await select("조회 대상", "개발 (a)");
    await type("채팅 초안");
    await click("가동 시간 조회 제안");
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
    const decision = Promise.withResolvers<"approved">();
    props.onDecideProposal.mockReturnValueOnce(decision.promise);
    const second = { ...card, id: "p2", node: "dev-02", target: beta };
    await render({ ...props, proposals: [card, second] });
    await select("조회 대상", "개발 (a)");
    await click("승인");
    await select("조회 대상", "개발 (b)");
    await act(async () => { decision.resolve("approved"); });
    expect(host.querySelector("article dd")?.textContent).toBe("dev-02");
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
    const values = () => [...host.querySelectorAll("article dd")].map((item) => item.textContent);
    await select("조회 대상", "개발 (a)");
    expect(values()).toEqual(["ops-a", "ap-northeast-2", "dev-01\\u{1b}", "uptime", card.interpretation]);
    await select("조회 대상", "개발 (b)");
    expect(values()).toEqual(["ops-b", "us-west-2", "dev-01\\u{1b}"]);
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
    expect(status?.textContent).toBe("결정 기록 중");
    expect(document.activeElement).toBe(status);
    const terminal = host.querySelector("textarea")!;
    if (move) terminal.focus();
    await act(async () => { response.resolve("rejected"); });
    expect(status?.textContent).toBe("거절 기록됨");
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
    expect(button("실행 승인")).toBeDefined();
    await click("실행 승인");
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
