// 파일 목록 시험이 함께 쓰는 DOM 도우미다.
import { act } from "react";

export function accessibleName(node: Element) {
  return node.getAttribute("aria-label") ?? node.textContent ?? "";
}

/** 단추·트리 항목·메뉴 항목을 접근성 이름으로 찾는다. */
export function control(host: ParentNode, name: string, selector = 'button, [role="treeitem"], [role="menuitem"]') {
  const node = Array.from(host.querySelectorAll<HTMLElement>(selector)).find(node => accessibleName(node) === name);
  if (!node) throw new Error(`컨트롤 없음: ${name}`);
  return node;
}

export function findControl(host: ParentNode, name: string, selector?: string) {
  try {
    return control(host, name, selector);
  } catch {
    return undefined;
  }
}

/** 목록 행 또는 아이콘 항목을 툴팁(전체 키 또는 표시 이름)으로 찾는다. */
export function fileRow(host: ParentNode, title: string) {
  const node = Array.from(host.querySelectorAll<HTMLElement>('[role="row"][aria-selected], [role="option"]')).find(node => node.title === title);
  if (!node) throw new Error(`행 없음: ${title}`);
  return node;
}

export function menuItem(host: ParentNode, name: string) {
  return control(host, name, '[role="menuitem"]');
}

export function dblclick(node: HTMLElement) {
  return act(async () => { node.dispatchEvent(new MouseEvent("dblclick", { bubbles: true, cancelable: true })); });
}

export function contextMenu(node: HTMLElement) {
  return act(async () => { node.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2 })); });
}

export function keyDown(node: HTMLElement, key: string, init: KeyboardEventInit = {}) {
  return act(async () => { node.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init })); });
}

export function pointerDown(node: Element) {
  return act(async () => { node.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true, cancelable: true })); });
}

export function typeInto(input: HTMLInputElement, value: string) {
  return act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}
