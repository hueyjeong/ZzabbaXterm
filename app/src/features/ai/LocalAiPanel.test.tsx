// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { AiTopTabs } from "./AiPanel";
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

const render = (profilesVersion = 0, topTabs?: AiTopTabs, sessionCandidates?: readonly SessionSelectionCandidate[], focused = false) =>
  act(async () => root.render(<LocalAiPanel profilesVersion={profilesVersion} topTabs={topTabs}
    sessionCandidates={sessionCandidates} focused={focused} />));
const selects = () => [...host.querySelectorAll("select")];
const button = (name: string) => [...host.querySelectorAll("button")].find((item) => item.textContent === name);
const proposalSettings = () => host.querySelector("details")!;
const toggleProposalSettings = () => act(async () => proposalSettings().querySelector("summary")!.click());
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
  await toggleProposalSettings();
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
  await select(0, 1);
  const draft = host.querySelector("textarea")!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(draft, "보존할 초안");
    draft.dispatchEvent(new InputEvent("input", { bubbles: true }));
  });
  await act(async () => button("새로고침")!.click());
  draft.focus();
  await act(async () => { failure.reject("설치된 로컬 모델 없음"); });
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("설치된 로컬 모델 없음");
  expect(host.querySelector("select")).toBeNull();
  expect(host.querySelector("textarea")).toBe(draft);
  expect(document.activeElement).toBe(draft);
  expect(draft.value).toBe("보존할 초안");
  expect(button("전송")?.disabled).toBe(true);
  await act(async () => button("새로고침")!.click());
  expect(host.querySelector('[role="alert"]')).toBeNull();
  expect(selects()[0].value).toBe("fixture:latest");
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
  await toggleProposalSettings();
  await select(0, 1);
  invoke.mockImplementation(async (command) => command === "ai_targets"
    ? [{ ...target, generation: "9007199254740994", label: "작업 셸" }] : true);
  await render(1);
  expect(selects()[0].value).toBe("");
});

it("기본 채팅 화면에는 빈 선택기와 대기 문구 없이 조회 설정을 접어 둔다", async () => {
  invoke.mockImplementation(async (command) => command === "ai_policy" ? false : []);
  await render(0, filesTab, []);
  expect(host.querySelector("select, article")).toBeNull();
  expect(host.querySelector('textarea[aria-label="메시지 입력"]')).not.toBeNull();
  expect(button("전송")?.disabled).toBe(true);
  expect(host.querySelector('[role="status"]')?.textContent).toBe("선택할 SSM 세션 없음");
  expect(host.textContent).not.toContain("대기");
  expect(button("새로고침")?.getAttribute("aria-label")).toBe("모델 및 대상 새로고침");
  expect(proposalSettings().open).toBe(false);
  expect(proposalSettings().querySelector("summary")?.textContent).toBe("작업 제안");
  await toggleProposalSettings();
  expect(proposalSettings().open).toBe(true);
  expect(proposalSettings().querySelector('input[type="checkbox"]')).not.toBeNull();
});

it("SFTP 탭 버튼이 없는 동안에도 Workspace가 주입한 패널을 유지한다", async () => {
  invoke.mockImplementation(async (command) => command === "ai_policy" ? false : []);
  const content = <input aria-label="SFTP 경로" />;
  const item = { id: "ssh-1", label: "SFTP", content };
  const onSelect = vi.fn();
  await render(0, { item, selectedId: item.id, onSelect });
  const input = host.querySelector<HTMLInputElement>('[aria-label="SFTP 경로"]')!;
  const panel = input.parentElement!;
  input.value = "/srv";

  await render(0, { item: { ...item, id: "ssh-2" }, selectedId: "ssh-2", onSelect });
  expect(host.querySelector('[aria-label="SFTP 경로"]')).toBe(input);
  await render(0, { item: { ...item, hidden: true }, selectedId: item.id, onSelect });
  expect(host.querySelector('[role="tablist"]')).toBeNull();
  expect(host.querySelector(".workspace-chat h2")?.textContent).toBe("AI 채팅");
  expect(host.querySelector('[aria-label="SFTP 경로"]')).toBe(input);
  expect(panel.hidden && panel.hasAttribute("inert")).toBe(true);

  await render(0, { item, selectedId: item.id, onSelect });
  expect(host.querySelector('[aria-label="SFTP 경로"]')).toBe(input);
  expect(panel.hidden || panel.hasAttribute("inert")).toBe(false);
  expect(input.value).toBe("/srv");
  expect(onSelect).not.toHaveBeenCalled();
});

const session = { id: "41", label: "개발 서버", region: "ap-northeast-2", node: "i-0123456789abcdef0" } satisfies SessionSelectionCandidate;
const sessionSelection = { id: session.id, region: session.region, node: session.node, selectionId: "9007199254740993", confirmed: false } satisfies SessionSelectionSnapshot;
const filesTab = { item: { id: "files", label: "파일", content: <input aria-label="SFTP 경로" /> },
  selectedId: null, onSelect: vi.fn() } satisfies AiTopTabs;

it("열린 SSM 세션의 선택·확정·해제만 호출하고 파일과 집중 보기 전환 후에도 확정을 유지한다", async () => {
  invoke.mockImplementation(async (command) => {
    if (command === "ai_session_select") return sessionSelection;
    if (command === "ai_session_confirm") return { ...sessionSelection, confirmed: true };
    if (command === "ai_session_clear") return null;
    return command === "ai_policy" ? false : [];
  });
  await render(0, filesTab);
  expect(selects().flatMap((field) => [...field.options]).some((option) => option.value === session.id)).toBe(false);
  await render(0, filesTab, [session]);
  expect(host.querySelector(".workspace-chat select option[value='41']")?.textContent).toBe("개발 서버 (세션 41)");
  invoke.mockClear();

  await select(0, 1);
  await act(async () => button("연결 확인")?.click());
  expect(host.querySelector(".workspace-chat [role=status]")?.textContent).toBe("연결 확인됨");

  const field = selects()[0];
  await render(0, { ...filesTab, selectedId: filesTab.item.id }, [session]);
  expect(field.closest<HTMLElement>(".workspace-chat")?.hidden).toBe(true);
  await render(0, filesTab, [session], true);
  expect(field.closest("[hidden]")).not.toBeNull();
  await render(0, filesTab, [session]);
  expect(selects()[0]).toBe(field);
  expect(field.value).toBe(session.id);
  expect(host.querySelector(".workspace-chat [role=status]")?.textContent).toBe("연결 확인됨");

  await act(async () => button("선택 해제")?.click());
  expect(field.value).toBe("");
  expect(invoke.mock.calls).toEqual([
    ["ai_session_select", { id: session.id }],
    ["ai_session_confirm", { id: session.id, selectionId: sessionSelection.selectionId }],
    ["ai_session_clear", { selectionId: sessionSelection.selectionId }],
  ]);
});

it.each(["파일", "집중 보기"])("%s로 전환하면 진행 중인 연결 확인을 해제한다", async (view) => {
  const confirmation = Promise.withResolvers<SessionSelectionSnapshot>();
  invoke.mockImplementation(async (command) => {
    if (command === "ai_session_select") return sessionSelection;
    if (command === "ai_session_confirm") return confirmation.promise;
    if (command === "ai_session_clear") return null;
    return command === "ai_policy" ? false : [];
  });
  await render(0, filesTab, [session]);
  invoke.mockClear();
  await select(0, 1);
  await act(async () => button("연결 확인")?.click());

  const tabs = view === "파일" ? { ...filesTab, selectedId: filesTab.item.id } : filesTab;
  await render(0, tabs, [session], view === "집중 보기");
  await act(async () => confirmation.resolve({ ...sessionSelection, confirmed: true }));
  await render(0, filesTab, [session]);

  expect(selects()[0].value).toBe("");
  expect(host.textContent).not.toContain("연결 확인됨");
  expect(button("연결 확인")).toBeUndefined();
  expect(invoke.mock.calls).toEqual([
    ["ai_session_select", { id: session.id }],
    ["ai_session_confirm", { id: session.id, selectionId: sessionSelection.selectionId }],
    ["ai_session_clear", { selectionId: sessionSelection.selectionId }],
  ]);
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
    await toggleProposalSettings();
    await select(0, 1);
    expect(button("승인")).toBeUndefined();
    await act(async () => button("CLI 제안 열기")?.click());
    await toggleProposalSettings();
    expect(host.querySelector("article")?.closest("details")).toBeNull();
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
  await toggleProposalSettings();
  await select(0, 1);
  await select(1, 1);
  if (inFlight) await act(async () => { selection.resolve(true); });
  await act(async () => button("가동 시간 조회 제안")?.click());
  if (inFlight) {
    await toggleProposalSettings();
    expect(proposalSettings().open).toBe(false);
    expect(button("요청 취소")?.closest("details")).toBeNull();
    expect(host.querySelector('[role="status"]')?.textContent).toBe("제안 요청 중");
  }
  button("요청 취소")?.focus();
  await act(async () => button("요청 취소")?.click());
  expect(document.activeElement).toBe(inFlight ? proposalSettings().querySelector("summary") : button("가동 시간 조회 제안"));
  if (inFlight) await toggleProposalSettings();
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
  await toggleProposalSettings();
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
