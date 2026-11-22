// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { LocalAiPanel } from "./LocalAiPanel";

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

const render = (profilesVersion = 0) => act(async () => root.render(<LocalAiPanel profilesVersion={profilesVersion} />));
const selects = () => [...host.querySelectorAll("select")];
const button = (name: string) => [...host.querySelectorAll("button")].find((item) => item.textContent === name);
async function select(index: number, option: number) {
  const field = selects()[index];
  await act(async () => {
    field.selectedIndex = option;
    field.dispatchEvent(new Event("change", { bubbles: true }));
  });
}

it("passes the exact saved generation through selection and keeps decisions separate from chat", async () => {
  const selected = Promise.withResolvers<boolean>();
  invoke.mockImplementation(async (command) => {
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
  await select(0, 1);
  await select(1, 1);
  await act(async () => button("가동 시간 조회 제안")?.click());
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

it("exposes model discovery failure and refreshes target properties after a saved profile changes", async () => {
  invoke.mockImplementation(async (command) => {
    if (command === "ai_decisions") return [];
    if (command === "ai_models") throw "설치된 로컬 모델 없음";
    if (command === "ai_targets") return [{ ...target, label: "작업 셸" }];
    throw new Error(command);
  });
  await render();
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("설치된 로컬 모델 없음");
  expect(button("전송")?.hasAttribute("disabled")).toBe(true);
  await select(1, 1);
  invoke.mockImplementation(async (command) => command === "ai_targets" ? [{ ...target, generation: "9007199254740994", label: "작업 셸" }] : true);
  await render(1);
  expect(selects()[1].value).toBe("");
});

it.each([["승인", "approve", "승인 기록됨"], ["거절", "reject", "거절 기록됨"]])(
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
    await select(1, 1);
    expect(button("승인")).toBeUndefined();
    await act(async () => button("CLI 제안 열기")?.click());
    expect(host.querySelector("article")?.textContent).toContain("i-0123456789abcdef0");
    await act(async () => button(label)?.click());
    expect(invoke).toHaveBeenCalledWith("ai_decide", { proposalId: "9", decision });
    expect(host.querySelector("article")?.textContent).toContain(state);
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
  await select(0, 1);
  await select(1, 1);
  if (inFlight) await act(async () => { selection.resolve(true); });
  await act(async () => button("가동 시간 조회 제안")?.click());
  button("요청 취소")?.focus();
  await act(async () => button("요청 취소")?.click());
  expect(document.activeElement).toBe(button("가동 시간 조회 제안"));
  if (!inFlight) await act(async () => { selection.resolve(true); });
  const first = invoke.mock.calls.find(([command]) => command === "ai_propose");
  if (inFlight) expect(invoke).toHaveBeenCalledWith("ai_cancel_proposal", { requestId: first?.[1].requestId });
  else expect(first).toBeUndefined();
  await act(async () => button("가동 시간 조회 제안")?.click());
  await act(async () => { late.resolve({ ...fresh, id: "7", interpretation: "취소한 제안" }); });
  expect([...host.querySelectorAll("article")].map((article) => article.querySelector("dd:last-child")?.textContent)).toEqual(["새 제안"]);
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
  await select(0, 1);
  await select(1, 1);
  await act(async () => button("가동 시간 조회 제안")?.click());
  await act(async () => button("승인")?.click());
  if (loseRead) {
    expect(host.querySelector("article [role=status]")?.textContent).toBe("결정 기록 실패");
    await act(async () => button("결정 다시 조회")?.click());
  }
  expect(host.querySelector("article [role=status]")?.textContent).toBe("승인 기록됨");
  expect(invoke.mock.calls.filter(([command]) => command === "ai_decide")).toHaveLength(1);
  await act(async () => button("가동 시간 조회 제안")?.click());
  await act(async () => host.querySelector<HTMLInputElement>('input[type="checkbox"]')?.click());
  await select(1, 0);
  expect(host.querySelectorAll("article")).toHaveLength(1);
  expect(host.querySelector("article [role=status]")?.textContent).toBe("승인 기록됨");
  expect(host.querySelector("article button")).toBeNull();
});
