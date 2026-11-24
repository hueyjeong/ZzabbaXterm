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
  expect(host.querySelector(".a01-empty")?.textContent).toBe("");
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

describe("우클릭 메뉴", () => {
  let clipboard: { writeText: ReturnType<typeof vi.fn>; readText: ReturnType<typeof vi.fn> };
  beforeEach(() => {
    clipboard = { writeText: vi.fn().mockResolvedValue(undefined), readText: vi.fn().mockResolvedValue("붙임") };
    Object.defineProperty(navigator, "clipboard", { value: clipboard, configurable: true });
  });
  afterEach(() => { Reflect.deleteProperty(navigator, "clipboard"); });

  async function contextMenu(element: Element, init: MouseEventInit = {}) {
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 10, clientY: 20, ...init });
    await act(async () => { element.dispatchEvent(event); });
    return event;
  }
  async function keyMenu(element: Element, init: KeyboardEventInit) {
    const event = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
    await act(async () => { element.dispatchEvent(event); });
    return event;
  }
  async function escape() {
    await act(async () => { menu()!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })); });
    expect(menu()).toBeNull();
  }
  const menu = () => host.querySelector('[role="menu"]');
  const menuItems = () => [...host.querySelectorAll<HTMLButtonElement>('[role="menu"] [role="menuitem"]')];
  const items = () => menuItems().map((item) => item.textContent);
  async function choose(label: string) {
    const item = menuItems().find((element) => element.textContent === label);
    expect(item, label).toBeDefined();
    await act(async () => { item!.click(); });
    expect(menu()).toBeNull();
  }
  async function chat(props: ReturnType<typeof setup>, text: string) {
    await type(text);
    await click("전송");
    expect(props.onSendChat).toHaveBeenCalled();
  }

  it("메시지 메뉴로 전체와 선택 영역을 복사하고 입력창 커서 위치에 넣는다", async () => {
    const props = setup();
    await render(props);
    await chat(props, "echo 질문");
    const [user, assistant] = [...host.querySelectorAll<HTMLElement>(".a01-msg")];

    expect((await contextMenu(user)).defaultPrevented).toBe(true);
    expect(menu()?.getAttribute("aria-label")).toBe("내 메시지 메뉴");
    expect(items()).toEqual(["복사", "입력창에 넣기"]);
    await choose("복사");
    expect(clipboard.writeText.mock.calls).toEqual([["echo 질문"]]);

    await contextMenu(assistant);
    expect(menu()?.getAttribute("aria-label")).toBe("AI 메시지 메뉴");
    expect(items()).toEqual(["복사"]);
    await choose("복사");
    expect(clipboard.writeText.mock.calls[1]).toEqual(["응답"]);

    const body = user.querySelector("p")!.firstChild!;
    document.getSelection()!.setBaseAndExtent(body, 0, body, 4);
    await contextMenu(user);
    expect(items()).toEqual(["복사", "선택 영역 복사", "입력창에 넣기"]);
    await choose("선택 영역 복사");
    expect(clipboard.writeText.mock.calls[2]).toEqual(["echo"]);
    await contextMenu(assistant);
    expect(items()).toEqual(["복사"]);
    await escape();
    document.getSelection()!.removeAllRanges();

    await type("앞뒤");
    input().setSelectionRange(1, 1);
    await contextMenu(user);
    await choose("입력창에 넣기");
    expect(input().value).toBe("앞echo 질문뒤");
    expect(document.activeElement).toBe(input());
    expect(input().selectionStart).toBe("앞echo 질문".length);
  });

  it("제안 카드 메뉴는 켜진 동작만 넣고 승인과 실행은 넣지 않는다", async () => {
    const props = setup();
    const ssmCard = { ...card, ssm: { profile: "dev", region: "ap-northeast-2" } } satisfies Proposal;
    await render({ ...props, proposals: [ssmCard] });
    await select("조회 대상", "개발 (a)");
    const article = host.querySelector("article")!;
    await contextMenu(article);
    expect(menu()?.getAttribute("aria-label")).toBe("조회 제안 메뉴");
    expect(items()).toEqual(["명령 복사", "대상 복사", "거절", "취소"]);
    await choose("명령 복사");
    await contextMenu(article.querySelector('[data-field="node"]')!);
    expect(host.querySelectorAll('[role="menu"]')).toHaveLength(1);
    await choose("대상 복사");
    expect(clipboard.writeText.mock.calls).toEqual([["uptime"], ["dev-01\\u{1b} / dev / ap-northeast-2"]]);

    await contextMenu(article);
    await choose("거절");
    expect(props.onDecideProposal.mock.calls).toEqual([[{ proposalId: "p1", decision: "reject" }]]);
    expect(host.querySelector(".a01-badge")?.textContent).toBe("거절됨");
    expect(document.activeElement).toBe(host.querySelector(".a01-badge"));
    await contextMenu(article);
    expect(items()).toEqual(["명령 복사", "대상 복사"]);
  });

  it("승인된 카드 메뉴에 실행이 없고 차단 카드에는 명령 복사가 없다", async () => {
    const props = setup();
    const blocked = { kind: "blocked", id: "p2", target: card.target, node: "dev-02" } satisfies Proposal;
    await render({ ...props, proposals: [blocked], history: [{ proposal: card, decision: "approved" }],
      onExecuteProposal: vi.fn<NonNullable<AiPanelProps["onExecuteProposal"]>>().mockResolvedValue(undefined) });
    await select("조회 대상", "개발 (a)");
    const articles = [...host.querySelectorAll("article")];
    const approved = articles.find((article) => article.dataset.kind === "reviewable")!;
    const other = articles.find((article) => article.dataset.kind === "blocked")!;
    expect(button("실행")).toBeDefined();
    await contextMenu(approved);
    expect(items()).toEqual(["명령 복사", "대상 복사"]);
    await contextMenu(other);
    expect(items()).toEqual(["대상 복사", "거절", "취소"]);
    expect(items()).not.toContain("승인");
  });

  it("실행 결과 메뉴는 출력을 복사하고 실행 중에는 승인 ID로 중지한다", async () => {
    const props = setup();
    const stop = vi.fn<NonNullable<AiPanelProps["onStopExecution"]>>().mockResolvedValue(undefined);
    const status: ExecutionStatus = { proposalId: "p1", approvalId: "3", phase: "running", output: " 10:32 up 2 days\n\u0007" };
    const view = (execution: ExecutionStatus) => render({ ...props, history: [{ proposal: card, decision: "approved" }],
      execution, onStopExecution: stop });
    await view(status);
    await select("조회 대상", "개발 (a)");
    const panel = host.querySelector(".a01-execution")!;
    await contextMenu(panel.querySelector("pre")!);
    expect(menu()?.getAttribute("aria-label")).toBe("실행 상태 메뉴");
    expect(items()).toEqual(["출력 복사", "중지"]);
    await choose("출력 복사");
    expect(clipboard.writeText.mock.calls).toEqual([[" 10:32 up 2 days\n\\u{7}"]]);
    await contextMenu(panel);
    await choose("중지");
    expect(stop.mock.calls).toEqual([["3"]]);

    await view({ ...status, phase: "exited", code: 0 });
    await contextMenu(host.querySelector(".a01-execution")!);
    expect(items()).toEqual(["출력 복사"]);
  });

  it("대화 기록 빈 공간 메뉴로 대화를 복사하고 새 대화를 시작한다", async () => {
    const props = setup();
    await render(props);
    await contextMenu(host.querySelector(".a01-scroll")!);
    expect(menu()?.getAttribute("aria-label")).toBe("대화 기록 메뉴");
    expect(items()).toEqual(["대화 복사", "새 대화"]);
    expect(menuItems().every((item) => item.disabled)).toBe(true);
    await escape();

    await chat(props, "질문");
    await contextMenu(host.querySelector(".a01-log")!);
    await choose("대화 복사");
    expect(clipboard.writeText.mock.calls).toEqual([["나: 질문\n\nAI: 응답"]]);
    await contextMenu(host.querySelector(".a01-log")!);
    await choose("새 대화");
    expect(host.querySelectorAll(".a01-msg")).toHaveLength(0);
    expect(document.activeElement).toBe(input());
  });

  it("입력창 메뉴는 기본 편집 메뉴를 막고 커서 위치에 붙여넣는다", async () => {
    const props = setup();
    await render(props);
    await type("가나다");
    input().focus();
    input().setSelectionRange(1, 2);
    const event = await contextMenu(input());
    expect(event.defaultPrevented).toBe(true);
    expect(menu()?.getAttribute("aria-label")).toBe("입력 메뉴");
    expect(items()).toEqual(["잘라내기", "복사", "붙여넣기", "모두 선택", "보내기"]);
    await choose("복사");
    expect(clipboard.writeText.mock.calls).toEqual([["나"]]);
    expect(document.activeElement).toBe(input());

    await contextMenu(input());
    await choose("붙여넣기");
    expect(clipboard.readText).toHaveBeenCalledTimes(1);
    expect(input().value).toBe("가붙임다");
    expect(input().selectionStart).toBe(3);

    input().setSelectionRange(0, 1);
    await contextMenu(input());
    await choose("잘라내기");
    expect(clipboard.writeText.mock.calls[1]).toEqual(["가"]);
    expect(input().value).toBe("붙임다");

    await contextMenu(input());
    await choose("모두 선택");
    expect([input().selectionStart, input().selectionEnd]).toEqual([0, 3]);
    await contextMenu(input());
    await choose("보내기");
    expect(props.onSendChat.mock.calls).toEqual([[{ model: "local:latest", messages: [{ role: "user", content: "붙임다" }] }]]);

    await contextMenu(input());
    expect(items()).toEqual(["잘라내기", "복사", "붙여넣기", "모두 선택"]);
    expect(menuItems().filter((item) => !item.disabled).map((item) => item.textContent)).toEqual(["붙여넣기"]);
  });

  it("Shift 우클릭은 기본 메뉴로 남긴다", async () => {
    await render(setup());
    expect((await contextMenu(input(), { shiftKey: true })).defaultPrevented).toBe(false);
    expect((await contextMenu(host.querySelector(".a01-scroll")!, { shiftKey: true })).defaultPrevented).toBe(false);
    expect(menu()).toBeNull();
  });

  it("메뉴 키와 Shift+F10으로 열고 닫으면 원래 요소로 포커스를 돌린다", async () => {
    await render(setup());
    input().focus();
    const event = await keyMenu(input(), { key: "ContextMenu" });
    expect(event.defaultPrevented).toBe(true);
    expect(items()).toEqual(["잘라내기", "복사", "붙여넣기", "모두 선택"]);
    expect(document.activeElement?.textContent).toBe("붙여넣기");
    await escape();
    expect(document.activeElement).toBe(input());

    expect((await keyMenu(input(), { key: "F10" })).defaultPrevented).toBe(false);
    expect(menu()).toBeNull();

    const targetChip = chip("조회 대상")!;
    targetChip.focus();
    await keyMenu(targetChip, { key: "F10", shiftKey: true });
    expect(menu()?.getAttribute("aria-label")).toBe("조회 대상");
    expect(items()).toEqual(["개발 (a)", "개발 (b)"]);
    await escape();
    expect(document.activeElement).toBe(targetChip);
  });

  it("조회 대상 칩을 우클릭하면 대상 목록을 포인터 위치에 띄운다", async () => {
    await render(setup());
    await select("조회 대상", "개발 (a)");
    await contextMenu(chip("조회 대상")!, { clientX: 30, clientY: 40 });
    expect(items()).toEqual(["개발 (a)", "개발 (b)", "대상 선택 해제"]);
    const opened = menu() as HTMLElement;
    expect([opened.style.left, opened.style.top]).toEqual(["30px", "40px"]);
    await choose("개발 (b)");
    expect(chip("조회 대상")?.getAttribute("aria-label")).toBe("조회 대상: 개발 (b)");
  });

  it("붙여넣기와 잘라내기 뒤 같은 값으로 다시 입력해도 초안이 화면과 맞는다", async () => {
    const props = setup();
    await render(props);
    await type("abc");
    input().focus();
    input().setSelectionRange(0, 1);
    await contextMenu(input());
    await choose("잘라내기");
    expect(input().value).toBe("bc");
    await type("abc");
    expect(input().value).toBe("abc");
    await click("전송");
    expect(props.onSendChat.mock.calls[0]).toEqual([{ model: "local:latest", messages: [{ role: "user", content: "abc" }] }]);

    input().focus();
    await contextMenu(input());
    await choose("붙여넣기");
    expect(input().value).toBe("붙임");
    await type("");
    expect(input().value).toBe("");
    expect(button("전송")?.disabled).toBe(true);
  });

  it("입력창에 넣은 뒤 모두 지우면 초안도 빈다", async () => {
    const props = setup();
    await render(props);
    await chat(props, "질문");
    await contextMenu(host.querySelector('.a01-msg[data-role="user"]')!);
    await choose("입력창에 넣기");
    expect(input().value).toBe("질문");
    expect(button("전송")?.disabled).toBe(false);
    await type("");
    expect(input().value).toBe("");
    expect(button("전송")?.disabled).toBe(true);
  });

  it("늦게 온 붙여넣기 응답은 그사이 입력이 바뀌었으면 버리고 겹친 붙여넣기는 무시한다", async () => {
    await render(setup());
    const late = Promise.withResolvers<string>();
    clipboard.readText.mockReturnValueOnce(late.promise);
    await type("처음");
    input().focus();
    await contextMenu(input());
    await choose("붙여넣기");
    await contextMenu(input());
    await choose("붙여넣기");
    expect(clipboard.readText).toHaveBeenCalledTimes(1);
    await type("바뀐 값");
    await act(async () => { late.resolve("늦은 글자"); });
    expect(input().value).toBe("바뀐 값");

    await contextMenu(input());
    await choose("붙여넣기");
    expect(clipboard.readText).toHaveBeenCalledTimes(2);
    expect(input().value).toBe("바뀐 값붙임");
  });

  it("열린 메뉴는 상태가 바뀌면 사라진 동작을 바로 뺀다", async () => {
    const props = setup();
    const stop = vi.fn<NonNullable<AiPanelProps["onStopExecution"]>>().mockResolvedValue(undefined);
    const running: ExecutionStatus = { proposalId: "p1", approvalId: "3", phase: "running", output: "" };
    await render({ ...props, proposals: [card] });
    await select("조회 대상", "개발 (a)");
    await contextMenu(host.querySelector("article")!);
    expect(items()).toEqual(["명령 복사", "대상 복사", "거절", "취소"]);
    await render({ ...props, proposals: [card], history: [{ proposal: card, decision: "rejected" }] });
    expect(items()).toEqual(["명령 복사", "대상 복사"]);
    await escape();

    const approved = { ...props, history: [{ proposal: card, decision: "approved" as const }], onStopExecution: stop };
    await render({ ...approved, execution: running });
    await contextMenu(host.querySelector(".a01-execution")!);
    expect(items()).toEqual(["출력 복사", "중지"]);
    await render({ ...approved, execution: { ...running, phase: "exited", code: 0 } });
    expect(items()).toEqual(["출력 복사"]);
    expect(stop).not.toHaveBeenCalled();
  });

  it("켜진 항목이 없는 메뉴도 키보드로 닫고 메뉴 키를 뗄 때 오는 contextmenu로 다시 열지 않는다", async () => {
    await render(setup());
    await select("조회 대상", "개발 (a)");
    const suggestion = button("가동 시간 확인")!;
    suggestion.focus();
    await keyMenu(suggestion, { key: "ContextMenu" });
    expect(menu()?.getAttribute("aria-label")).toBe("대화 기록 메뉴");
    expect(menuItems().every((item) => item.disabled)).toBe(true);
    expect(document.activeElement).toBe(menu());
    const after = await contextMenu(document.activeElement!);
    expect(after.defaultPrevented).toBe(true);
    expect(host.querySelectorAll('[role="menu"]')).toHaveLength(1);
    await escape();
    expect(document.activeElement).toBe(suggestion);
  });

  it("메시지에 포커스를 두고 메뉴 키로 메뉴를 연다", async () => {
    const props = setup();
    await render(props);
    await chat(props, "질문");
    const message = host.querySelector<HTMLElement>('.a01-msg[data-role="assistant"]')!;
    message.focus();
    expect(document.activeElement).toBe(message);
    await keyMenu(message, { key: "F10", shiftKey: true });
    expect(menu()?.getAttribute("aria-label")).toBe("AI 메시지 메뉴");
    await escape();
    expect(document.activeElement).toBe(message);
  });

  it("실행 출력의 선택 영역을 복사한다", async () => {
    const props = setup();
    await render({ ...props, history: [{ proposal: card, decision: "approved" }],
      execution: { proposalId: "p1", phase: "exited", output: "10:32 up 2 days" } });
    await select("조회 대상", "개발 (a)");
    const output = host.querySelector("pre")!.firstChild!;
    document.getSelection()!.setBaseAndExtent(output, 6, output, 15);
    await contextMenu(host.querySelector("pre")!);
    expect(items()).toEqual(["출력 복사", "선택 영역 복사"]);
    await choose("선택 영역 복사");
    expect(clipboard.writeText.mock.calls).toEqual([["up 2 days"]]);
    document.getSelection()!.removeAllRanges();
  });
});
