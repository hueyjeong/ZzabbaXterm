// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { QuickOpen, score, type QuickItem } from "./QuickOpen";

let host: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.body.appendChild(document.createElement("div"));
});
afterEach(() => {
  host.remove();
  vi.unstubAllGlobals();
});

it("부분 문자열과 순서대로 흩어진 글자를 찾고 앞부분 일치를 먼저 둔다", () => {
  expect(score("데모 EC2", "ec2")).toBeGreaterThan(score("빌드 서버 ec2-user", "ec2"));
  expect(score("빌드 서버", "빌서")).toBeGreaterThan(0);
  expect(score("빌드 서버", "서빌")).toBe(0);
});

it("화살표로 고르고 Enter로 한 항목만 실행하며 Escape는 아무것도 실행하지 않는다", async () => {
  const runs = { a: vi.fn(), b: vi.fn(), c: vi.fn() };
  const items: QuickItem[] = [
    { id: "a", section: "연결", label: "데모 EC2", icon: null, run: runs.a },
    { id: "b", section: "연결", label: "빌드 서버", icon: null, run: runs.b },
    { id: "c", section: "명령", label: "로컬 터미널", icon: null, run: runs.c },
  ];
  const onClose = vi.fn();
  const root = createRoot(host);
  await act(async () => { root.render(<QuickOpen items={items} onClose={onClose} />); });
  const field = host.querySelector<HTMLInputElement>('[role="combobox"]')!;
  expect(document.activeElement).toBe(field);
  const press = (key: string) => act(async () => { field.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true })); });
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(field, "서버");
    field.dispatchEvent(new Event("input", { bubbles: true }));
  });
  expect([...host.querySelectorAll('[role="option"]')].map(option => option.textContent)).toEqual(["빌드 서버"]);
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(field, "");
    field.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await press("ArrowDown");
  await press("ArrowDown");
  expect(host.querySelector('[aria-selected="true"]')?.textContent).toBe("로컬 터미널");
  await press("Enter");
  expect(runs.c).toHaveBeenCalledTimes(1);
  expect(runs.a).not.toHaveBeenCalled();
  expect(onClose).toHaveBeenCalledTimes(1);
  await press("Escape");
  expect(onClose).toHaveBeenCalledTimes(2);
  expect(runs.a).not.toHaveBeenCalled();
  await act(async () => root.unmount());
});
