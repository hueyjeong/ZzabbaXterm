// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { LocalAiPanel } from "./LocalAiPanel";
import type { SessionSelectionCandidate, SessionSelectionSnapshot } from "./SessionSelection";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

const target = { targetId: "work", generation: "9007199254740993" };
let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  invoke.mockReset();
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.unstubAllGlobals();
});

const render = (profilesVersion = 0, sessionCandidates?: readonly SessionSelectionCandidate[], focused = false) =>
  act(async () => root.render(<LocalAiPanel profilesVersion={profilesVersion}
    sessionCandidates={sessionCandidates} focused={focused} />));
const button = (name: string) => [...host.querySelectorAll("button")]
  .find((item) => item.textContent === name || item.getAttribute("aria-label") === name);
const chip = (label: string) => [...host.querySelectorAll<HTMLButtonElement>('button[aria-haspopup="menu"]')]
  .find((item) => item.getAttribute("aria-label")?.startsWith(`${label}: `));
const menuItems = () => [...host.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')];

async function openMenu(trigger: HTMLButtonElement | undefined) {
  expect(trigger).toBeDefined();
  await act(async () => trigger!.click());
  return menuItems();
}

/** 칩 메뉴에서 index번째 항목을 고른다. */
async function pick(label: string, index: number) {
  const items = await openMenu(chip(label));
  expect(items[index], `${label} ${index}`).toBeDefined();
  await act(async () => items[index].click());
}

const chooseModel = (index = 0) => pick("모델", index);
const chooseTarget = (index = 0) => pick("조회 대상", index);
async function clearTarget() {
  const items = await openMenu(chip("조회 대상"));
  await act(async () => items.find((item) => item.textContent === "대상 선택 해제")!.click());
}
async function togglePolicy() {
  const items = await openMenu(button("더보기"));
  const toggle = items.find((item) => item.textContent?.startsWith("조회 승인 차단"));
  expect(toggle).toBeDefined();
  await act(async () => toggle!.click());
}

it("passes the exact saved generation through selection and keeps decisions separate from chat", async () => {
  const selected = Promise.withResolvers<boolean>();
  invoke.mockImplementation(async (command) => {
    if (command === "ai_policy") return false;
    if (command === "ai_decisions") return [];
    if (command === "ai_models") return ["fixture:latest"];
    if (command === "ai_targets") return [{ ...target, label: "작업 셸" }];
    if (command === "ai_select") return selected.promise;
    if (command === "ai_propose") return { id: "7", target, kind: "reviewable", node: "내 PC", displayCommand: "uptime", interpretation: "가동 시간" };
    if (command === "ai_decide") return "cancelled";
    if (command === "ai_chat") return { kind: "error", message: "선택한 로컬 Ollama 모델이 설치되어 있지 않습니다." };
    throw new Error(command);
  });
  await render();
  await chooseModel();
  await chooseTarget();
  await act(async () => button("가동 시간 확인")?.click());
  expect(invoke).toHaveBeenCalledWith("ai_select", { target });
  expect(invoke.mock.calls.some(([command]) => command === "ai_propose")).toBe(false);
  await act(async () => selected.resolve(true));
  expect(invoke).toHaveBeenCalledWith("ai_propose", { model: "fixture:latest", target, operation: "uptime", requestId: expect.any(Number) });
  await act(async () => button("취소")?.click());
  expect(invoke).toHaveBeenCalledWith("ai_decide", { proposalId: "7", decision: "cancel" });
  expect(host.textContent).toContain("취소됨");
  const draft = host.querySelector("textarea")!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(draft, "채팅 질문");
    draft.dispatchEvent(new InputEvent("input", { bubbles: true }));
  });
  await act(async () => button("전송")?.click());
  expect(invoke).toHaveBeenCalledWith("ai_chat", { model: "fixture:latest", messages: [{ role: "user", content: "채팅 질문" }] });
  expect(draft.value).toBe("채팅 질문");
  expect(host.querySelector('[role="alert"]')?.textContent).toContain("설치되어 있지 않습니다");
});

it("모델 조회 실패 중에도 작성 폼과 포커스를 유지하고 새로고침 후 전송을 다시 활성화한다", async () => {
  const failure = Promise.withResolvers<never>();
  const modelRoster = vi.fn().mockResolvedValueOnce(["fixture:latest"])
    .mockReturnValueOnce(failure.promise).mockResolvedValueOnce(["fixture:latest"]);
  invoke.mockImplementation(async (command) => {
    if (command === "ai_models") return modelRoster();
    if (command === "ai_policy") return false;
    return [];
  });
  await render();
  await chooseModel();
  const draft = host.querySelector("textarea")!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(draft, "보존할 초안");
    draft.dispatchEvent(new InputEvent("input", { bubbles: true }));
  });
  await act(async () => button("모델 및 대상 새로고침")!.click());
  draft.focus();
  await act(async () => { failure.reject("설치된 로컬 모델 없음"); });
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("설치된 로컬 모델 없음");
  expect(chip("모델")?.getAttribute("aria-label")).toBe("모델: 없음");
  expect(chip("모델")?.disabled).toBe(true);
  expect(host.querySelector("textarea")).toBe(draft);
  expect(document.activeElement).toBe(draft);
  expect(draft.value).toBe("보존할 초안");
  expect(button("전송")?.disabled).toBe(true);
  await act(async () => button("모델 및 대상 새로고침")!.click());
  expect(host.querySelector('[role="alert"]')).toBeNull();
  expect(chip("모델")?.getAttribute("aria-label")).toBe("모델: fixture:latest");
  expect(host.querySelector("textarea")).toBe(draft);
  expect(draft.value).toBe("보존할 초안");
  expect(button("전송")?.disabled).toBe(false);
});

it("저장된 조회 대상의 세대가 바뀌면 이전 선택을 사용하지 않는다", async () => {
  invoke.mockImplementation(async (command) => {
    if (command === "ai_targets") return [{ ...target, label: "작업 셸" }];
    if (command === "ai_select") return true;
    if (command === "ai_policy") return false;
    return [];
  });
  await render();
  await chooseTarget();
  expect(chip("조회 대상")?.getAttribute("aria-label")).toBe("조회 대상: 작업 셸 (work)");
  invoke.mockImplementation(async (command) => command === "ai_targets"
    ? [{ ...target, generation: "9007199254740994", label: "작업 셸" }] : true);
  await render(1);
  expect(chip("조회 대상")?.getAttribute("aria-label")).toBe("조회 대상: 선택 안 함");
});

it("기본 채팅 화면에는 빈 선택기와 대기 문구 없이 정책 토글을 더보기 메뉴에 둔다", async () => {
  invoke.mockImplementation(async (command) => command === "ai_policy" ? false : []);
  await render(0, []);
  expect(host.querySelector("select, article")).toBeNull();
  expect(chip("조회 대상")).toBeUndefined();
  expect(host.querySelector('textarea[aria-label="메시지 입력"]')).not.toBeNull();
  expect(button("전송")?.disabled).toBe(true);
  expect(host.querySelector('[role="status"]')?.textContent).toBe("SSM 세션 없음");
  expect(host.textContent).not.toContain("대기");
  expect(button("모델 및 대상 새로고침")?.title).toBe("모델 및 대상 새로고침");
  expect(button("CLI 제안 가져오기")?.disabled).toBe(true);
  expect(button("더보기")?.getAttribute("aria-expanded")).toBe("false");
  const items = await openMenu(button("더보기"));
  expect(button("더보기")?.getAttribute("aria-expanded")).toBe("true");
  expect(items.map((item) => item.textContent)).toEqual(["조회 승인 차단"]);
  expect(items[0].disabled).toBe(false);
});

it("조회 승인 차단을 메뉴에서 켜고 끄며 저장 중에는 다시 누를 수 없다", async () => {
  const saving = Promise.withResolvers<null>();
  invoke.mockImplementation(async (command) => {
    if (command === "ai_policy") return false;
    if (command === "ai_set_policy") return saving.promise;
    return [];
  });
  await render();
  await togglePolicy();
  expect(invoke).toHaveBeenCalledWith("ai_set_policy", { blocked: true });
  expect(host.querySelector('[role="status"]')?.textContent).toBe("설정 저장 중");
  const busy = await openMenu(button("더보기"));
  expect(busy[0].disabled).toBe(true);
  await act(async () => {
    host.querySelector('[role="menu"]')!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  });
  await act(async () => saving.resolve(null));
  expect(host.querySelector(".a01-head-badge")?.textContent).toBe("승인 차단");
  invoke.mockImplementation(async (command) => command === "ai_set_policy" ? null : []);
  const items = await openMenu(button("더보기"));
  expect(items[0].textContent).toBe("조회 승인 차단 해제");
  await act(async () => items[0].click());
  expect(invoke).toHaveBeenCalledWith("ai_set_policy", { blocked: false });
  expect(host.querySelector(".a01-head-badge")).toBeNull();
});

const session = { id: "41", label: "개발 서버", region: "ap-northeast-2", node: "i-0123456789abcdef0" } satisfies SessionSelectionCandidate;
const sessionSelection = { id: session.id, region: session.region, node: session.node, selectionId: "9007199254740993", confirmed: false } satisfies SessionSelectionSnapshot;

it("열린 SSM 세션의 선택·확정·해제만 호출하고 집중 보기 전환 후에도 확정을 유지한다", async () => {
  invoke.mockImplementation(async (command) => {
    if (command === "ai_session_select") return sessionSelection;
    if (command === "ai_session_confirm") return { ...sessionSelection, confirmed: true };
    if (command === "ai_session_clear") return null;
    return command === "ai_policy" ? false : [];
  });
  await render(0);
  expect(chip("AI 작업 세션")).toBeUndefined();
  await render(0, [session]);
  const items = await openMenu(chip("AI 작업 세션"));
  expect(items.map((item) => item.textContent)).toEqual(["개발 서버 (세션 41)"]);
  invoke.mockClear();

  await act(async () => items[0].click());
  await act(async () => button("연결 확인")?.click());
  expect(host.querySelector(".workspace-chat [role=status]")?.textContent).toBe("연결 확인됨");
  expect(button("연결 확인")).toBeUndefined();

  const field = chip("AI 작업 세션")!;
  await render(0, [session], true);
  expect(field.closest("[hidden]")).not.toBeNull();
  await render(0, [session]);
  expect(chip("AI 작업 세션")).toBe(field);
  expect(field.dataset.value).toBe(session.id);
  expect(host.querySelector(".workspace-chat [role=status]")?.textContent).toBe("연결 확인됨");

  await act(async () => button("선택 해제")?.click());
  expect(field.dataset.value).toBe("");
  expect(invoke.mock.calls).toEqual([
    ["ai_session_select", { id: session.id }],
    ["ai_session_confirm", { id: session.id, selectionId: sessionSelection.selectionId }],
    ["ai_session_clear", { selectionId: sessionSelection.selectionId }],
  ]);
});

it("집중 보기로 전환하면 진행 중인 연결 확인을 해제한다", async () => {
  const confirmation = Promise.withResolvers<SessionSelectionSnapshot>();
  invoke.mockImplementation(async (command) => {
    if (command === "ai_session_select") return sessionSelection;
    if (command === "ai_session_confirm") return confirmation.promise;
    if (command === "ai_session_clear") return null;
    return command === "ai_policy" ? false : [];
  });
  await render(0, [session]);
  invoke.mockClear();
  await pick("AI 작업 세션", 0);
  await act(async () => button("연결 확인")?.click());

  await render(0, [session], true);
  await act(async () => confirmation.resolve({ ...sessionSelection, confirmed: true }));
  await render(0, [session]);

  expect(chip("AI 작업 세션")?.dataset.value).toBe("");
  expect(host.textContent).not.toContain("연결 확인됨");
  expect(button("연결 확인")).toBeUndefined();
  expect(invoke.mock.calls).toEqual([
    ["ai_session_select", { id: session.id }],
    ["ai_session_confirm", { id: session.id, selectionId: sessionSelection.selectionId }],
    ["ai_session_clear", { selectionId: sessionSelection.selectionId }],
  ]);
});

it.each([["승인", "approve", "승인됨"], ["거절", "reject", "거절됨"]])(
  "opens a CLI card without a model and records %s only after a click", async (label, decision, state) => {
    invoke.mockImplementation(async (command) => {
      if (command === "ai_decisions") return [];
      if (command === "ai_policy") return false;
      if (command === "ai_models") throw "설치된 로컬 모델 없음";
      if (command === "ai_targets") return [{ ...target, label: "관리 노드" }];
      if (command === "ai_select") return true;
      if (command === "ai_cli_review") return {
        id: "9", target, kind: "reviewable", node: "i-0123456789abcdef0",
        displayCommand: "uptime", interpretation: "가동 시간",
      };
      if (command === "ai_decide") return decision === "approve" ? "approved" : "rejected";
      throw new Error(command);
    });
    await render();
    expect(button("CLI 제안 가져오기")?.disabled).toBe(true);
    await chooseTarget();
    expect(button("승인")).toBeUndefined();
    expect(button("가동 시간 확인")).toBeUndefined();
    await act(async () => button("CLI 제안 가져오기")?.click());
    expect(host.querySelector("article")?.closest('[role="log"]')).not.toBeNull();
    expect(host.querySelector("article")?.textContent).toContain("i-0123456789abcdef0");
    await act(async () => button(label)?.click());
    expect(invoke).toHaveBeenCalledWith("ai_decide", { proposalId: "9", decision });
    expect(host.querySelector("article [role=status]")?.textContent).toBe(state);
    expect(invoke.mock.calls.map(([command]) => command)).toEqual([
      "ai_policy", "ai_models", "ai_targets", "ai_decisions", "ai_decisions", "ai_select", "ai_cli_review", "ai_decide", "ai_decisions",
    ]);
  });

it.each([false, true])("cancels before IPC starts or cancels only its own in-flight request (%s)", async (inFlight) => {
  const selection = Promise.withResolvers<boolean>();
  const late = Promise.withResolvers<object>();
  const fresh = { id: "8", target, kind: "reviewable", node: "내 PC", displayCommand: "uptime", interpretation: "새 제안" };
  invoke.mockImplementation(async (command) => {
    if (command === "ai_decisions") return [];
    if (command === "ai_policy") return false;
    if (command === "ai_models") return ["fixture:latest"];
    if (command === "ai_targets") return [{ ...target, label: "작업 셸" }];
    if (command === "ai_select") return selection.promise;
    if (command === "ai_cancel_proposal") return null;
    if (command === "ai_propose") {
      return inFlight && invoke.mock.calls.filter(([name]) => name === "ai_propose").length === 1 ? late.promise : fresh;
    }
    throw new Error(command);
  });
  await render();
  await chooseModel();
  await chooseTarget();
  if (inFlight) await act(async () => { selection.resolve(true); });
  await act(async () => button("가동 시간 확인")?.click());
  expect(host.querySelector('[role="status"]')?.textContent).toBe("제안 요청 중");
  expect(button("가동 시간 확인")?.getAttribute("aria-disabled")).toBe("true");
  button("요청 취소")?.focus();
  await act(async () => button("요청 취소")?.click());
  expect(button("요청 취소")).toBeUndefined();
  expect(document.activeElement).toBe(button("가동 시간 확인"));
  if (!inFlight) await act(async () => { selection.resolve(true); });
  const first = invoke.mock.calls.find(([command]) => command === "ai_propose");
  if (inFlight) expect(invoke).toHaveBeenCalledWith("ai_cancel_proposal", { requestId: first?.[1].requestId });
  else expect(first).toBeUndefined();
  await act(async () => button("가동 시간 확인")?.click());
  await act(async () => { late.resolve({ ...fresh, id: "7", interpretation: "취소한 제안" }); });
  expect([...host.querySelectorAll("article")].map((article) =>
    article.querySelector("[data-field=interpretation]")?.textContent)).toEqual(["새 제안"]);
});

it.each([false, true])("recovers a lost decision response without another write and keeps read-only history (%s)", async (loseRead) => {
  const proposal = { id: "7", target, kind: "reviewable", node: "내 PC", displayCommand: "uptime", interpretation: "가동 시간" };
  let written = false;
  let firstRead = true;
  invoke.mockImplementation(async (command) => {
    if (command === "ai_models") return ["fixture:latest"];
    if (command === "ai_policy") return false;
    if (command === "ai_targets") return [{ ...target, label: "작업 셸" }];
    if (command === "ai_select" || command === "ai_set_policy") return true;
    if (command === "ai_propose") return proposal;
    if (command === "ai_decisions") {
      if (!written) return [];
      if (firstRead) {
        firstRead = false;
        if (loseRead) throw Error("조회 응답 유실");
      }
      return [{ proposal, decision: "approved" }];
    }
    if (command === "ai_decide") { written = true; throw Error("응답 유실"); }
    throw Error(command);
  });
  await render();
  await chooseModel();
  await chooseTarget();
  await act(async () => button("가동 시간 확인")?.click());
  await act(async () => button("승인")?.click());
  if (loseRead) {
    expect(host.querySelector("article [role=status]")?.textContent).toBe("기록 실패");
    await act(async () => button("다시 조회")?.click());
  }
  expect(host.querySelector("article [role=status]")?.textContent).toBe("승인됨");
  expect(invoke.mock.calls.filter(([command]) => command === "ai_decide")).toHaveLength(1);
  await act(async () => button("가동 시간 확인")?.click());
  await togglePolicy();
  await clearTarget();
  expect(host.querySelectorAll("article")).toHaveLength(1);
  expect(host.querySelector("article [role=status]")?.textContent).toBe("승인됨");
  expect(host.querySelector("article button")).toBeNull();
});
