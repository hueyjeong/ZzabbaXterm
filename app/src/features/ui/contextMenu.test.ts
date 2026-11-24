// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { suppressDefaultContextMenu } from "./contextMenu";

afterEach(() => { document.body.replaceChildren(); });

function rightClickOn(element: Element, init: MouseEventInit = {}) {
  const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, ...init });
  element.addEventListener("contextmenu", (event) => suppressDefaultContextMenu(event as MouseEvent));
  element.dispatchEvent(event);
  return event.defaultPrevented;
}

it("앱 화면의 기본 메뉴는 막고 입력칸·편집기·Shift 우클릭은 남긴다", () => {
  const panel = document.body.appendChild(document.createElement("div"));
  expect(rightClickOn(panel)).toBe(true);
  expect(rightClickOn(document.body.appendChild(document.createElement("input")))).toBe(false);
  expect(rightClickOn(document.body.appendChild(document.createElement("textarea")))).toBe(false);
  const checkbox = document.body.appendChild(document.createElement("input"));
  checkbox.type = "checkbox";
  expect(rightClickOn(checkbox)).toBe(true);
  const xterm = document.body.appendChild(document.createElement("textarea"));
  xterm.className = "xterm-helper-textarea";
  expect(rightClickOn(xterm)).toBe(true);
  const monaco = document.body.appendChild(document.createElement("div"));
  monaco.className = "monaco-editor";
  expect(rightClickOn(monaco.appendChild(document.createElement("span")))).toBe(false);
  expect(rightClickOn(document.body.appendChild(document.createElement("div")), { shiftKey: true })).toBe(false);
});
