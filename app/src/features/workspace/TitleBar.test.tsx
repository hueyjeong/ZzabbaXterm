// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { TitleBar } from "./TitleBar";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), isTauri: () => false }));

let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
  act(() => root.render(<TitleBar focused={false} sidebar aiPanel newTabItems={[]} onNewTerminal={vi.fn()} onToggle={vi.fn()}
    onToggleSidebar={vi.fn()} onToggleAi={vi.fn()} onWindowError={vi.fn()} />));
});

afterEach(() => {
  act(() => root.unmount());
  document.body.replaceChildren();
  vi.unstubAllGlobals();
});

function pressTitleButton() {
  const button = host.querySelector<HTMLButtonElement>('button[aria-label="새 로컬 터미널"]')!;
  return !button.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 0 }));
}

it.each([
  ["원격 화면 키보드 입력칸", () => {
    const keyboard = document.body.appendChild(document.createElement("textarea"));
    keyboard.className = "remote-desktop-keyboard";
    return keyboard;
  }],
  ["터미널 화면", () => {
    const screen = document.body.appendChild(document.createElement("div"));
    screen.className = "terminal-screen";
    return screen.appendChild(document.createElement("textarea"));
  }],
])("%s의 키보드 포커스를 제목줄 단추 누름으로 빼앗지 않는다", (_, create) => {
  create().focus();
  expect(pressTitleButton()).toBe(true);
});

it("다른 요소에 포커스가 있으면 제목줄 단추 누름의 기본 동작을 막지 않는다", () => {
  document.body.appendChild(document.createElement("input")).focus();
  expect(pressTitleButton()).toBe(false);
});
