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
    if (command === "ai_models") return ["fixture:latest"];
    if (command === "ai_targets") return [{ ...target, label: "작업 셸" }];
    if (command === "ai_select") return selected.promise;
    if (command === "ai_propose") return { id: "7", target, kind: "reviewable", node: "내 PC", displayCommand: "uptime", interpretation: "가동 시간" };
    if (command === "ai_decide") return "recorded";
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
  expect(invoke).toHaveBeenCalledWith("ai_propose", { model: "fixture:latest", target, operation: "uptime" });
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
