// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { ObjectTable } from "../storage/ObjectTable";
import { FileBrowser, type FileEntry, type VersionedFilesStatus } from "./FileBrowser";

const key = "  실험//../한글 +%2F.txt  ";
const entries: readonly FileEntry[] = [
  { kind: "folder", key },
  { kind: "file", key, bytes: 9 },
];
let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.unstubAllGlobals();
});

function button(text: string, scope = "button") {
  const node = Array.from(host.querySelectorAll<HTMLButtonElement>(scope)).find(node => node.textContent === text);
  if (!node) throw new Error(`버튼 없음: ${text}`);
  return node;
}

function click(node: HTMLElement) {
  return act(async () => node.click());
}

function press(node: HTMLElement, key: string, shiftKey = false) {
  return act(async () => { node.dispatchEvent(new KeyboardEvent("keydown", { key, shiftKey, bubbles: true, cancelable: true })); });
}

function context(node: HTMLElement) {
  const event = new MouseEvent("contextmenu", { button: 2, bubbles: true, cancelable: true });
  return act(async () => { node.dispatchEvent(event); }).then(() => expect(event.defaultPrevented).toBe(true));
}

it("같은 키의 폴더와 파일을 구별하고 목록·아이콘 전환 시 원본 키와 선택을 유지한다", async () => {
  const onOpenFolder = vi.fn();
  const onSelectFile = vi.fn();
  await act(async () => root.render(<FileBrowser {...{ entries, selectedKey: key, onOpenFolder, onSelectFile }} />));

  const selectedFile = () => button(key, 'button[aria-pressed="true"]');
  expect(host.querySelector("table")).not.toBeNull();
  expect(selectedFile()).toBeTruthy();
  await click(button("아이콘"));
  expect(host.querySelector("table")).toBeNull();
  expect(host.querySelector('ul[aria-label="파일 목록"] svg')).not.toBeNull();
  await click(selectedFile());
  await click(button(key, 'button:not([aria-pressed])'));
  expect(onSelectFile).toHaveBeenCalledExactlyOnceWith(key);
  expect(onOpenFolder).toHaveBeenCalledExactlyOnceWith(key);
  await click(button("목록"));
  expect(host.querySelector("table")).not.toBeNull();
  expect(selectedFile()).toBeTruthy();
});

it("우클릭·Shift+F10·메뉴 키는 편집 없이 메뉴를 열고 편집 항목만 원본 객체 키를 전달한다", async () => {
  const onOpenPrefix = vi.fn();
  const onSelectObject = vi.fn();
  const onEditObject = vi.fn();
  await act(async () => root.render(<ObjectTable rows={[{ kind: "object", key, bytes: null }]}
    selectedKey={null} {...{ onOpenPrefix, onSelectObject, onEditObject }} />));
  let file = button(key);
  await context(file);
  let edit = button("편집", 'button[role="menuitem"]');
  expect(document.activeElement).toBe(edit);
  expect(onSelectObject).not.toHaveBeenCalled();
  expect(onEditObject).not.toHaveBeenCalled();
  await press(edit, "Escape");
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(file);

  await press(file, "F10");
  expect(host.querySelector('[role="menu"]')).toBeNull();
  await press(file, "F10", true);
  await act(async () => document.body.click());
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(file);
  await context(file);

  await click(button("아이콘"));
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(button("아이콘"));
  file = button(key);
  await act(async () => file.focus());
  await press(file, "ContextMenu");
  edit = button("편집", 'button[role="menuitem"]');
  expect(document.activeElement).toBe(edit);
  await press(edit, "ArrowDown");
  expect(document.activeElement).toBe(button("선택", 'button[role="menuitem"]'));
  await press(button("선택", 'button[role="menuitem"]'), "ArrowUp");
  expect(document.activeElement).toBe(edit);
  expect(onEditObject).not.toHaveBeenCalled();
  await click(edit);
  expect(onEditObject).toHaveBeenCalledExactlyOnceWith(key);
  expect(onSelectObject).not.toHaveBeenCalled();
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(file);
});

it("폴더와 편집 콜백이 없는 파일 메뉴에는 편집 항목이 없다", async () => {
  const onOpenFolder = vi.fn();
  const onSelectFile = vi.fn();
  const onEditFile = vi.fn();
  await act(async () => root.render(<FileBrowser {...{ entries, selectedKey: null, onOpenFolder, onSelectFile, onEditFile }} />));
  await context(button(key, 'button:not([aria-pressed])'));
  expect(host.querySelector('[role="menu"]')?.textContent).toBe("열기");
  expect(onOpenFolder).not.toHaveBeenCalled();
  await click(button("열기", 'button[role="menuitem"]'));
  expect(onOpenFolder).toHaveBeenCalledExactlyOnceWith(key);
  expect(onEditFile).not.toHaveBeenCalled();

  await act(async () => root.render(<FileBrowser {...{ entries, selectedKey: null, onOpenFolder, onSelectFile }} />));
  await context(button(key, 'button[aria-pressed="false"]'));
  expect(host.querySelector('[role="menu"]')?.textContent).toBe("선택");
  expect(onSelectFile).not.toHaveBeenCalled();
});

it("표시 이름이 있는 파일은 불투명 키를 노출하지 않고 편집에 원본 키를 전달한다", async () => {
  const onEditFile = vi.fn();
  const name = "한글 파일.txt";
  await act(async () => root.render(<FileBrowser entries={[{ kind: "file", key, name, bytes: 9 }]} selectedKey={key}
    onOpenFolder={vi.fn()} onSelectFile={vi.fn()} onEditFile={onEditFile} />));
  const file = button(name, 'button[aria-pressed="true"]');
  expect(file.title).toBe(name);
  expect(host.textContent).not.toContain(key);
  await context(file);
  await click(button("편집", 'button[role="menuitem"]'));
  expect(onEditFile).toHaveBeenCalledExactlyOnceWith(key);
});

it("목록이 교체되면 키가 같아도 이전 메뉴를 닫고 외부 컨트롤의 포커스를 유지한다", async () => {
  const onEditFile = vi.fn();
  const props = { selectedKey: null, onOpenFolder: vi.fn(), onSelectFile: vi.fn(), onEditFile };
  const render = (rows: readonly FileEntry[]) => act(async () => root.render(<>
    <FileBrowser entries={rows} {...props} />
    <button type="button">다른 컨트롤</button>
  </>));
  await render(entries);
  const file = button(key, 'button[aria-pressed="false"]');
  await context(file);
  const outside = button("다른 컨트롤");
  await act(async () => { outside.focus(); outside.click(); });
  expect(document.activeElement).toBe(outside);
  expect(host.querySelector('[role="menu"]')).toBeNull();

  await context(file);
  await render([...entries]);
  expect(document.activeElement).toBe(file);
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(onEditFile).not.toHaveBeenCalled();
});

it("이전 버전 상태는 호출자가 전달한 상태와 전체 수만 표시한다", async () => {
  const props = { entries, selectedKey: null, onOpenFolder: vi.fn(), onSelectFile: vi.fn() };
  const render = (versionedFiles?: VersionedFilesStatus) => act(async () => root.render(<FileBrowser {...props} versionedFiles={versionedFiles} />));
  await render();
  expect(host.textContent).not.toContain("이전 버전이 있는 파일");
  for (const [status, detail] of [
    [{ kind: "loading" }, "확인 중"],
    [{ kind: "unsupported" }, "지원 안 함"],
    [{ kind: "denied" }, "권한 거부"],
    [{ kind: "unavailable" }, "확인 실패"],
    [{ kind: "count", count: 0 }, "0개"],
    [{ kind: "count", count: 43 }, "43개"],
  ] as const) {
    await render(status);
    expect(host.querySelector('[aria-live="polite"]')?.textContent).toBe(`이전 버전이 있는 파일: ${detail}`);
  }
});
