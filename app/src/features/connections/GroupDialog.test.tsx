// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { GroupDraft, SavedConnections } from "./connectionIpc";
import { GroupDialog, type GroupRequest } from "./GroupDialog";
import { groupChildren, groupContents, groupDepth, groupOptions, subtreeHeight } from "./groupTree";

let host: HTMLDivElement;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  Object.assign(HTMLDialogElement.prototype, {
    showModal(this: HTMLDialogElement) { this.setAttribute("open", ""); },
    close(this: HTMLDialogElement) { this.removeAttribute("open"); },
  });
  host = document.body.appendChild(document.createElement("div"));
});

afterEach(() => {
  host.remove();
  vi.unstubAllGlobals();
});

// a → b → c 3단계와 별도 최상위 d, 그리고 1단계부터 8단계까지 이어진 l1 → … → l8이다.
const chain = Array.from({ length: 8 }, (_, index) => ({
  revision: 1, profile: { id: `l${index + 1}`, displayName: `단계 ${index + 1}`, ...(index ? { parentId: `l${index}` } : {}) },
}));
const groups: SavedConnections["groups"] = [
  { revision: 3, profile: { id: "a", displayName: "가" } },
  { revision: 1, profile: { id: "b", displayName: "나", parentId: "a" } },
  { revision: 1, profile: { id: "c", displayName: "다", parentId: "b" } },
  { revision: 1, profile: { id: "d", displayName: "라" } },
  ...chain,
];

async function render(request: GroupRequest, save = vi.fn(async (_draft: GroupDraft): Promise<unknown> => ({ revision: 1 }))) {
  const onClose = vi.fn();
  const root = createRoot(host);
  await act(async () => root.render(<GroupDialog request={request} groups={groups} save={save} onClose={onClose} />));
  const dialog = host.querySelector<HTMLDialogElement>("dialog")!;
  const name = dialog.querySelector<HTMLInputElement>("input")!;
  const parent = dialog.querySelector<HTMLSelectElement>("select")!;
  const button = (label: string) => [...dialog.querySelectorAll<HTMLButtonElement>("button")].find(item => item.textContent === label)!;
  const errors = () => [...dialog.querySelectorAll(".field-error, [role=alert]")].map(item => item.textContent);
  return {
    dialog, name, parent, save, onClose, button, errors,
    async type(value: string) {
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(name, value);
        name.dispatchEvent(new Event("input", { bubbles: true }));
      });
    },
    async choose(value: string) {
      await act(async () => {
        parent.value = value;
        parent.dispatchEvent(new Event("change", { bubbles: true }));
      });
    },
    async submit() { await act(async () => { button("저장").click(); }); },
    unmount: () => act(() => root.unmount()),
  };
}

it("새 그룹은 이름을 넣어야 저장되고 부모 기본값을 넣어 개정 null로 저장한다", async () => {
  const view = await render({ parentId: "b" });
  try {
    expect(view.dialog.getAttribute("aria-label")).toBe("그룹 추가");
    expect(document.activeElement).toBe(view.name);
    expect(view.parent.value).toBe("b");
    expect([...view.dialog.querySelectorAll("button")].map(item => item.textContent)).toEqual(["저장", "취소"]);
    expect(view.button("저장").disabled).toBe(true);
    await view.type("  운영  ");
    await view.submit();
    expect(view.save).toHaveBeenCalledExactlyOnceWith({
      version: 2, revision: null, group: { id: expect.any(String), displayName: "운영", parentId: "b" },
    });
    expect(view.onClose).toHaveBeenCalledExactlyOnceWith(true);
  } finally {
    view.unmount();
  }
});

it("최상위를 고르면 parentId를 빼고 저장한다", async () => {
  const view = await render({});
  try {
    expect(view.parent.value).toBe("");
    expect(view.parent.options[0].textContent).toBe("최상위");
    await view.type("새 그룹");
    await view.submit();
    expect(view.save.mock.calls[0][0]).toEqual({ version: 2, revision: null, group: { id: expect.any(String), displayName: "새 그룹" } });
  } finally {
    view.unmount();
  }
});

it("그룹 편집은 자기 자신과 하위를 상위 후보에서 빼고 상위를 바꾸면 개정과 함께 이동한다", async () => {
  const view = await render({ group: groups[0] });
  try {
    expect(view.dialog.getAttribute("aria-label")).toBe("그룹 편집");
    expect(view.name.value).toBe("가");
    const ids = [...view.parent.options].map(option => option.value);
    expect(ids).not.toContain("a");
    expect(ids).not.toContain("b");
    expect(ids).not.toContain("c");
    expect(ids.slice(0, 2)).toEqual(["", "d"]);
    await view.type("가나");
    await view.choose("d");
    await view.submit();
    expect(view.save).toHaveBeenCalledExactlyOnceWith({ version: 2, revision: 3, group: { id: "a", displayName: "가나", parentId: "d" } });
  } finally {
    view.unmount();
  }
});

it("8단계를 넘는 추가나 이동은 상위 그룹 아래에 오류를 보이고 저장을 막는다", async () => {
  const add = await render({ parentId: "l8" });
  try {
    await add.type("아홉");
    expect(add.errors()).toEqual(["최대 8단계 초과"]);
    expect(add.parent.getAttribute("aria-invalid")).toBe("true");
    expect(add.button("저장").disabled).toBe(true);
    await add.choose("l7");
    expect(add.errors()).toEqual([]);
    expect(add.button("저장").disabled).toBe(false);
  } finally {
    add.unmount();
  }
  // 높이 3인 가 → 나 → 다를 6단계 아래로 옮기면 9단계가 된다.
  const move = await render({ group: groups[0] });
  try {
    await move.choose("l6");
    expect(move.errors()).toEqual(["최대 8단계 초과"]);
    expect(move.button("저장").disabled).toBe(true);
    await move.choose("l5");
    expect(move.button("저장").disabled).toBe(false);
  } finally {
    move.unmount();
  }
});

it("Rust 오류는 이름·상위 그룹 필드 아래나 상태 줄에 짧게 보이고 입력을 유지한다", async () => {
  const replies: unknown[] = [
    { kind: "invalid", field: "displayName" }, { kind: "invalid", field: "parentId" }, { kind: "conflict" }, { kind: "failed", message: "프로필 저장: Io" },
  ];
  const save = vi.fn(async (_draft: GroupDraft): Promise<unknown> => { throw replies.shift(); });
  const view = await render({ parentId: "a" }, save);
  try {
    await view.type("이름");
    await view.submit();
    expect(view.errors()).toEqual(["이름 형식 오류"]);
    expect(view.name.getAttribute("aria-invalid")).toBe("true");
    await view.submit();
    expect(view.errors()).toEqual(["상위 그룹 오류"]);
    await view.submit();
    expect(view.errors()).toEqual(["다른 작업이 그룹을 변경했습니다"]);
    await view.submit();
    expect(view.errors()).toEqual(["프로필 저장: Io"]);
    expect(view.name.value).toBe("이름");
    expect(view.onClose).not.toHaveBeenCalled();
  } finally {
    view.unmount();
  }
});

it("취소와 Escape는 저장하지 않고 닫는다", async () => {
  const view = await render({});
  try {
    await act(async () => { view.button("취소").click(); });
    await act(async () => { view.dialog.dispatchEvent(new Event("cancel", { cancelable: true })); });
    expect(view.onClose.mock.calls).toEqual([[false], [false]]);
    expect(view.save).not.toHaveBeenCalled();
  } finally {
    view.unmount();
  }
});

it("그룹 깊이와 하위 높이, 하위 포함 삭제 집합을 계산한다", () => {
  expect(groupDepth(groups, "c")).toBe(3);
  expect(groupDepth(groups, "l8")).toBe(8);
  expect(subtreeHeight(groups, "a")).toBe(3);
  expect(subtreeHeight(groups, "d")).toBe(1);
  const target = (id: string, groupId?: string) => ({
    revision: 2, profile: { id, displayName: id, ...(groupId ? { groupId } : {}), defaultMethod: "local" as const, local: { shell: "bash" as const, startDirectory: "/" } },
  });
  const contents = groupContents({ version: 2, groups, targets: [target("in-a", "a"), target("in-c", "c"), target("in-d", "d"), target("top")] }, "a");
  expect(contents).toEqual({
    groups: 2, targets: 2,
    expected: [
      { kind: "group", id: "b", revision: 1 }, { kind: "group", id: "c", revision: 1 },
      { kind: "target", id: "in-a", revision: 2 }, { kind: "target", id: "in-c", revision: 2 },
    ],
  });
});

it("없는 부모를 가리키는 고아 그룹은 최상위로 다루고 순환 데이터에서도 계산이 끝난다", () => {
  const orphan: SavedConnections["groups"] = [
    { revision: 1, profile: { id: "top", displayName: "위" } },
    { revision: 1, profile: { id: "orphan", displayName: "고아", parentId: "missing" } },
    { revision: 1, profile: { id: "child", displayName: "아래", parentId: "orphan" } },
  ];
  expect(groupChildren(orphan).get(undefined)?.map(({ id }) => id)).toEqual(["top", "orphan"]);
  expect(groupOptions(orphan).map(({ id }) => id)).toEqual(["top", "orphan", "child"]);
  expect(groupDepth(orphan, "orphan")).toBe(1);
  expect(groupDepth(orphan, "child")).toBe(2);
  expect(subtreeHeight(orphan, "orphan")).toBe(2);

  // x → y → x 순환은 최상위에서 닿지 않으므로 선택지에 없다. 깊이와 높이, 삭제 집합은 한 바퀴에서 멈춘다.
  const cycle: SavedConnections["groups"] = [
    { revision: 1, profile: { id: "top", displayName: "위" } },
    { revision: 2, profile: { id: "x", displayName: "엑스", parentId: "y" } },
    { revision: 3, profile: { id: "y", displayName: "와이", parentId: "x" } },
  ];
  expect(groupOptions(cycle).map(({ id }) => id)).toEqual(["top"]);
  expect(groupDepth(cycle, "x")).toBe(2);
  expect(subtreeHeight(cycle, "x")).toBe(2);
  expect(groupContents({ version: 2, groups: cycle, targets: [] }, "x").expected).toEqual([{ kind: "group", id: "y", revision: 3 }]);
});
