import { describe, expect, it } from "vitest";
import {
  addTab, addTabInSplit, focusedTab, initialLayout, MAX_GROUPS, mergeGroups, moveTab, removeTab, resizeGroups, selectTab,
  splitTab, visibleTabs, type Layout,
} from "./layout";

const tabsOf = (layout: Layout) => layout.groups.map(group => group.tabs);

describe("중앙 탭 그룹", () => {
  it("탭을 오른쪽 새 그룹으로 분리하면 각 그룹이 자기 탭 줄과 활성 탭을 갖는다", () => {
    let layout = addTab(addTab(initialLayout(1), 2), 3);
    layout = splitTab(layout, 3, layout.groups[0].id, "right");
    expect(tabsOf(layout)).toEqual([[1, 2], [3]]);
    expect(layout.groups.map(group => group.active)).toEqual([2, 3]);
    expect(focusedTab(layout)).toBe(3);
    expect(visibleTabs(layout)).toEqual(new Set([2, 3]));
    expect(layout.groups[0].size + layout.groups[1].size).toBeCloseTo(1);
  });

  it("탭이 하나뿐인 그룹은 자기 옆으로 분리하지 않는다", () => {
    const layout = initialLayout(1);
    expect(splitTab(layout, 1, layout.groups[0].id)).toBe(layout);
  });

  it("다른 그룹으로 옮기면 빈 그룹은 사라지고 포커스는 대상 그룹으로 간다", () => {
    let layout = splitTab(addTab(initialLayout(1), 2), 2, 1);
    layout = moveTab(layout, 2, 1, 0);
    expect(tabsOf(layout)).toEqual([[2, 1]]);
    expect(focusedTab(layout)).toBe(2);
  });

  it("같은 그룹 안에서 놓은 자리의 바로 앞으로 옮긴다", () => {
    const layout = addTab(addTab(initialLayout(1), 2), 3);
    expect(tabsOf(moveTab(layout, 1, 1, 2))).toEqual([[2, 1, 3]]);
    expect(tabsOf(moveTab(layout, 1, 1, 1))).toEqual([[1, 2, 3]]);
    expect(tabsOf(moveTab(layout, 1, 1, 3))).toEqual([[2, 3, 1]]);
    expect(tabsOf(moveTab(layout, 3, 1, 0))).toEqual([[3, 1, 2]]);
  });

  it("탭 선택은 해당 그룹만 바꾸고 다른 그룹의 활성 탭은 유지한다", () => {
    let layout = splitTab(addTab(addTab(initialLayout(1), 2), 3), 3, 1);
    layout = selectTab(layout, 1);
    expect(layout.groups.map(group => group.active)).toEqual([1, 3]);
    expect(focusedTab(layout)).toBe(1);
  });

  it("포커스 그룹의 마지막 탭을 닫으면 왼쪽 그룹으로 포커스가 돌아간다", () => {
    let layout = splitTab(addTab(initialLayout(1), 2), 2, 1);
    layout = removeTab(layout, 2);
    expect(tabsOf(layout)).toEqual([[1]]);
    expect(focusedTab(layout)).toBe(1);
    layout = removeTab(layout, 1);
    expect(tabsOf(layout)).toEqual([[]]);
    expect(focusedTab(layout)).toBeNull();
  });

  it("닫은 활성 탭 대신 같은 위치의 다음 탭, 없으면 이전 탭을 선택한다", () => {
    let layout = selectTab(addTab(addTab(initialLayout(1), 2), 3), 2);
    layout = removeTab(layout, 2);
    expect(focusedTab(layout)).toBe(3);
    layout = removeTab(layout, 3);
    expect(focusedTab(layout)).toBe(1);
  });

  it("새 그룹은 상한까지만 만들고 상한에서는 같은 그룹에 연다", () => {
    let layout = initialLayout(1);
    for (let tab = 2; tab <= MAX_GROUPS + 1; tab++) layout = addTabInSplit(layout, tab, layout.focused);
    expect(layout.groups).toHaveLength(MAX_GROUPS);
    expect(tabsOf(layout)[MAX_GROUPS - 1]).toEqual([MAX_GROUPS, MAX_GROUPS + 1]);
  });

  it("경계 비율은 두 그룹 합 안에서만 바뀌고 15–85%로 제한한다", () => {
    let layout = addTabInSplit(initialLayout(1), 2, 1);
    layout = resizeGroups(layout, 0, 0.95);
    expect(layout.groups[0].size).toBeCloseTo(0.85);
    expect(layout.groups[1].size).toBeCloseTo(0.15);
  });

  it("그룹 합치기는 그룹 순서대로 탭을 모으고 포커스 탭을 유지한다", () => {
    let layout = splitTab(addTab(addTab(initialLayout(1), 2), 3), 2, 1, "left");
    layout = selectTab(layout, 3);
    layout = mergeGroups(layout);
    expect(tabsOf(layout)).toEqual([[2, 1, 3]]);
    expect(focusedTab(layout)).toBe(3);
  });
});
