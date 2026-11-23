// @vitest-environment jsdom
import { act, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { FileBrowser, VersionedFiles, type EntryRef, type FileEntry, type VersionedFilesStatus } from "./FileBrowser";
import { contextMenu, control, dblclick, fileRow, keyDown, menuItem, pointerDown } from "./testing";

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

type Props = ComponentProps<typeof FileBrowser>;
function render(props: Partial<Props> & Pick<Props, "entries">, extra?: React.ReactNode) {
  return act(async () => root.render(<>
    <FileBrowser selected={null} onSelect={vi.fn()} onOpen={vi.fn()} {...props} />
    {extra}
  </>));
}

function rows() {
  return Array.from(host.querySelectorAll<HTMLElement>('[role="row"][aria-selected], [role="option"]'));
}

function click(node: HTMLElement) {
  return act(async () => node.click());
}

it("같은 키의 폴더와 파일을 구별하고 목록·아이콘 전환 시 원본 키와 선택을 유지한다", async () => {
  const onOpen = vi.fn();
  const onSelect = vi.fn();
  const selected: EntryRef = { kind: "file", key };
  await render({ entries, selected, onOpen, onSelect, view: "list" });

  const selectedRow = () => host.querySelector<HTMLElement>('[aria-selected="true"]')!;
  expect(host.querySelector('[role="grid"]')).not.toBeNull();
  expect(rows().map(row => [row.dataset.kind, row.title, row.getAttribute("aria-selected")])).toEqual([
    ["folder", key, "false"], ["file", key, "true"],
  ]);
  await render({ entries, selected, onOpen, onSelect, view: "icons" });
  expect(host.querySelector('[role="grid"]')).toBeNull();
  expect(host.querySelector('[role="listbox"][aria-label="파일 목록"] svg')).not.toBeNull();
  expect(selectedRow().dataset.kind).toBe("file");
  await click(selectedRow());
  await dblclick(host.querySelector<HTMLElement>('[role="option"][data-kind="folder"]')!);
  expect(onSelect.mock.calls.map(([entry]) => [entry.kind, entry.key])).toEqual([["file", key]]);
  expect(onOpen).toHaveBeenCalledExactlyOnceWith(entries[0]);
  await render({ entries, selected, onOpen, onSelect, view: "list" });
  expect(host.querySelector('[role="grid"]')).not.toBeNull();
  expect(selectedRow().dataset.kind).toBe("file");
});

it("우클릭·Shift+F10·메뉴 키는 선택 없이 메뉴를 열고 메뉴 항목만 원본 키를 전달한다", async () => {
  const onSelect = vi.fn();
  const onEdit = vi.fn();
  const props = {
    entries: [{ kind: "file", key, bytes: null }] as const, onSelect,
    menuItems: (entry: FileEntry) => [{ label: "편집", onSelect: () => onEdit(entry.key) }, { label: "다운로드", onSelect: vi.fn() }],
  };
  const outside = <button type="button">아이콘</button>;
  await render(props, outside);
  let file = fileRow(host, key);
  await contextMenu(file);
  let edit = menuItem(host, "편집");
  expect(document.activeElement).toBe(edit);
  expect(onSelect).not.toHaveBeenCalled();
  expect(onEdit).not.toHaveBeenCalled();
  await keyDown(edit, "Escape");
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(file);

  await keyDown(file, "F10");
  expect(host.querySelector('[role="menu"]')).toBeNull();
  await keyDown(file, "F10", { shiftKey: true });
  expect(host.querySelector('[role="menu"]')).not.toBeNull();
  await pointerDown(document.body);
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(file);
  await contextMenu(file);

  const toggle = control(host, "아이콘");
  await pointerDown(toggle);
  await act(async () => toggle.focus());
  await render({ ...props, view: "icons" }, outside);
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(toggle);
  file = fileRow(host, key);
  await act(async () => file.focus());
  await keyDown(file, "ContextMenu");
  edit = menuItem(host, "편집");
  expect(document.activeElement).toBe(edit);
  await keyDown(edit, "ArrowDown");
  expect(document.activeElement).toBe(menuItem(host, "다운로드"));
  await keyDown(menuItem(host, "다운로드"), "ArrowUp");
  expect(document.activeElement).toBe(edit);
  expect(onEdit).not.toHaveBeenCalled();
  await click(edit);
  expect(onEdit).toHaveBeenCalledExactlyOnceWith(key);
  expect(onSelect).not.toHaveBeenCalled();
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(file);
});

it("메뉴 항목이 없는 행은 메뉴를 열지 않고 폴더의 열기 항목은 원본 키를 전달한다", async () => {
  const onOpen = vi.fn();
  const onSelect = vi.fn();
  await render({
    entries, onOpen, onSelect,
    menuItems: entry => entry.kind === "folder" ? [{ label: "열기", onSelect: () => onOpen(entry) }] : [],
  });
  const [folder, file] = rows();
  await contextMenu(folder);
  expect(host.querySelector('[role="menu"]')?.textContent).toBe("열기");
  expect(onOpen).not.toHaveBeenCalled();
  await click(menuItem(host, "열기"));
  expect(onOpen).toHaveBeenCalledExactlyOnceWith(entries[0]);

  const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2 });
  await act(async () => { file.dispatchEvent(event); });
  expect(event.defaultPrevented).toBe(true);
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(onSelect).not.toHaveBeenCalled();
});

it("표시 이름이 있는 파일은 불투명 키를 노출하지 않고 메뉴에 원본 키를 전달한다", async () => {
  const onEdit = vi.fn();
  const name = "한글 파일.txt";
  await render({
    entries: [{ kind: "file", key, name, bytes: 9 }], selected: { kind: "file", key },
    menuItems: entry => [{ label: "편집", onSelect: () => onEdit(entry.key) }],
  });
  const file = fileRow(host, name);
  expect(file.getAttribute("aria-selected")).toBe("true");
  expect(host.textContent).not.toContain(key);
  expect(host.innerHTML).not.toContain(key.trim());
  await contextMenu(file);
  await click(menuItem(host, "편집"));
  expect(onEdit).toHaveBeenCalledExactlyOnceWith(key);
});

it("목록이 교체되면 키가 같아도 이전 메뉴를 닫고 외부 컨트롤의 포커스를 유지한다", async () => {
  const onEdit = vi.fn();
  const props = { menuItems: () => [{ label: "편집", onSelect: onEdit }] };
  const extra = <button type="button">다른 컨트롤</button>;
  await render({ entries, ...props }, extra);
  const file = rows()[1];
  await contextMenu(file);
  const outside = control(host, "다른 컨트롤");
  await pointerDown(outside);
  await act(async () => { outside.focus(); outside.click(); });
  expect(document.activeElement).toBe(outside);
  expect(host.querySelector('[role="menu"]')).toBeNull();

  await contextMenu(file);
  await render({ entries: [...entries], ...props }, extra);
  expect(document.activeElement).toBe(file);
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(onEdit).not.toHaveBeenCalled();
});

it("이전 버전 상태는 호출자가 전달한 상태와 전체 수만 표시한다", async () => {
  await render({ entries });
  expect(host.textContent).not.toContain("이전 버전이 있는 파일");
  for (const [status, detail] of [
    [{ kind: "loading" }, "확인 중"],
    [{ kind: "unsupported" }, "지원 안 함"],
    [{ kind: "denied" }, "권한 거부"],
    [{ kind: "unavailable" }, "확인 실패"],
    [{ kind: "count", count: 0 }, "0개"],
    [{ kind: "count", count: 43 }, "43개"],
  ] as const) {
    await act(async () => root.render(<VersionedFiles status={status satisfies VersionedFilesStatus} />));
    expect(host.querySelector('[aria-live="polite"]')?.textContent).toBe(`이전 버전이 있는 파일: ${detail}`);
  }
});

const listed: readonly FileEntry[] = [
  { kind: "file", key: "b/file10.txt", name: "file10.txt", bytes: 2048, modified: "2026-01-02T00:00:00Z", storageClass: "STANDARD" },
  { kind: "folder", key: "b/zeta/", name: "zeta" },
  { kind: "file", key: "b/file2.txt", name: "file2.txt", bytes: null, modified: "2026-03-01T00:00:00Z", storageClass: "GLACIER" },
  { kind: "file", key: "b/archive.zip", name: "archive.zip", bytes: 5, modified: "2025-12-31T00:00:00Z" },
  { kind: "folder", key: "b/alpha/", name: "alpha" },
];
const names = () => rows().map(row => row.querySelector(".file-name")?.textContent);

it("열 머리로 이름·크기·수정 시각·클래스를 정렬하고 폴더를 항상 먼저 둔다", async () => {
  await render({ entries: listed, columns: ["size", "modified", "class"] });
  const header = (label: string) => control(host, label, '[role="columnheader"] button');
  expect(names()).toEqual(["alpha", "zeta", "archive.zip", "file2.txt", "file10.txt"]);
  expect(header("이름").parentElement?.getAttribute("aria-sort")).toBe("ascending");
  await click(header("이름"));
  expect(names()).toEqual(["zeta", "alpha", "file10.txt", "file2.txt", "archive.zip"]);
  expect(header("이름").parentElement?.getAttribute("aria-sort")).toBe("descending");
  await click(header("크기"));
  expect(names()).toEqual(["alpha", "zeta", "archive.zip", "file10.txt", "file2.txt"]);
  expect(header("이름").parentElement?.getAttribute("aria-sort")).toBe("none");
  await click(header("수정 시각"));
  expect(names()).toEqual(["alpha", "zeta", "archive.zip", "file10.txt", "file2.txt"]);
  await click(header("수정 시각"));
  expect(names()).toEqual(["zeta", "alpha", "file2.txt", "file10.txt", "archive.zip"]);
  await click(header("스토리지 클래스"));
  expect(names()).toEqual(["alpha", "zeta", "file2.txt", "file10.txt", "archive.zip"]);
  const size = fileRow(host, "file10.txt").querySelector<HTMLElement>('[data-column="size"]')!;
  expect([size.textContent, size.title]).toEqual(["2.0 KB", "2,048 바이트"]);
});

it("화살표·Home·End로 선택을 옮기고 Enter로 열며 Backspace로 상위로 간다", async () => {
  let selected: EntryRef | null = null;
  const onOpen = vi.fn();
  const onParent = vi.fn();
  const onSelect = vi.fn((entry: FileEntry) => { selected = { kind: entry.kind, key: entry.key }; });
  const draw = () => render({ entries: listed, selected, onSelect, onOpen, onParent });
  await draw();
  expect(rows().map(row => row.tabIndex)).toEqual([0, -1, -1, -1, -1]);
  const press = async (key: string) => {
    await keyDown(document.activeElement as HTMLElement, key);
    await draw();
  };
  await act(async () => rows()[0].focus());
  await press("ArrowDown");
  expect(selected).toEqual({ kind: "folder", key: "b/zeta/" });
  expect(document.activeElement).toBe(fileRow(host, "zeta"));
  await press("End");
  expect(selected).toEqual({ kind: "file", key: "b/file10.txt" });
  await press("ArrowDown");
  expect(selected).toEqual({ kind: "file", key: "b/file10.txt" });
  await press("ArrowUp");
  expect(selected).toEqual({ kind: "file", key: "b/file2.txt" });
  expect(rows().map(row => row.tabIndex)).toEqual([-1, -1, -1, 0, -1]);
  await press("Enter");
  expect(onOpen).toHaveBeenCalledExactlyOnceWith(listed[2]);
  await press("Home");
  expect(selected).toEqual({ kind: "folder", key: "b/alpha/" });
  await press("Backspace");
  expect(onParent).toHaveBeenCalledTimes(1);
  expect(onOpen).toHaveBeenCalledTimes(1);
});

it("좁은 모드는 이름과 알려진 크기만 표시하고 빈 목록은 상태 문구를 표시한다", async () => {
  await render({ entries: listed, compact: true, columns: ["size", "modified", "class"] });
  expect(Array.from(host.querySelectorAll('[role="columnheader"]')).map(node => node.textContent)).toEqual(["이름", "크기"]);
  await render({ entries: [{ kind: "file", key: "id", name: "이름.txt", bytes: null }], compact: true });
  expect(Array.from(host.querySelectorAll('[role="columnheader"]')).map(node => node.textContent)).toEqual(["이름"]);
  await render({ entries: [], empty: "빈 폴더" });
  expect(host.querySelector('[role="status"]')?.textContent).toBe("빈 폴더");
});
