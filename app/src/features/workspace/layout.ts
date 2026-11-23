// 중앙 탭 그룹 배치. 탭 수명(세션)과 분리된 순수 상태 전이만 둔다.
export type Group = Readonly<{ id: number; tabs: readonly number[]; active: number | null; size: number }>;
export type Layout = Readonly<{ groups: readonly Group[]; focused: number }>;
export type Side = "left" | "right";

export const MAX_GROUPS = 4;

export function initialLayout(tab?: number): Layout {
  return { groups: [{ id: 1, tabs: tab === undefined ? [] : [tab], active: tab ?? null, size: 1 }], focused: 1 };
}

export function groupOf(layout: Layout, tab: number) {
  return layout.groups.find(group => group.tabs.includes(tab));
}

export function focusedGroup(layout: Layout) {
  return layout.groups.find(group => group.id === layout.focused) ?? layout.groups[0];
}

/** 키보드 입력이 갈 탭: 포커스 그룹의 활성 탭이다. */
export function focusedTab(layout: Layout) {
  return focusedGroup(layout)?.active ?? null;
}

export function visibleTabs(layout: Layout) {
  return new Set(layout.groups.flatMap(group => group.active === null ? [] : [group.active]));
}

function nextGroupId(layout: Layout) {
  return Math.max(0, ...layout.groups.map(group => group.id)) + 1;
}

function withoutTab(group: Group, tab: number): Group {
  const index = group.tabs.indexOf(tab);
  if (index < 0) return group;
  const tabs = group.tabs.filter(id => id !== tab);
  const active = group.active !== tab ? group.active : tabs[index] ?? tabs[index - 1] ?? null;
  return { ...group, tabs, active };
}

/** 빈 그룹을 없애고 포커스를 이웃 그룹으로 옮긴다. 마지막 그룹은 빈 채로 남긴다. */
function prune(layout: Layout): Layout {
  const index = layout.groups.findIndex(group => group.id === layout.focused);
  const groups = layout.groups.filter(group => group.tabs.length > 0);
  if (!groups.length) {
    const first = layout.groups[0];
    return { groups: [{ ...first, tabs: [], active: null, size: 1 }], focused: first.id };
  }
  if (groups.some(group => group.id === layout.focused)) return { ...layout, groups };
  const before = layout.groups.slice(0, Math.max(0, index)).reverse().find(group => group.tabs.length > 0);
  return { groups, focused: (before ?? groups[0]).id };
}

/** 포커스 그룹(또는 지정한 그룹)에 탭을 추가하고 선택한다. */
export function addTab(layout: Layout, tab: number, groupId = layout.focused): Layout {
  const target = layout.groups.some(group => group.id === groupId) ? groupId : layout.groups[0].id;
  return {
    groups: layout.groups.map(group => group.id === target ? { ...group, tabs: [...group.tabs, tab], active: tab } : group),
    focused: target,
  };
}

export function selectTab(layout: Layout, tab: number): Layout {
  const owner = groupOf(layout, tab);
  if (!owner) return layout;
  return {
    groups: layout.groups.map(group => group.id === owner.id ? { ...group, active: tab } : group),
    focused: owner.id,
  };
}

export function removeTab(layout: Layout, tab: number): Layout {
  return prune({ ...layout, groups: layout.groups.map(group => withoutTab(group, tab)) });
}

/** 탭을 다른 그룹의 index 위치(생략하면 끝)로 옮긴다. 같은 그룹 안에서는 순서만 바꾼다. */
export function moveTab(layout: Layout, tab: number, groupId: number, index?: number): Layout {
  const source = groupOf(layout, tab);
  const target = layout.groups.find(group => group.id === groupId);
  if (!source || !target) return layout;
  const groups = layout.groups.map(group => {
    let next = group.id === source.id ? { ...withoutTab(group, tab), active: group.id === target.id ? group.active : withoutTab(group, tab).active } : group;
    if (group.id === target.id) {
      const tabs = next.tabs.filter(id => id !== tab);
      const at = Math.max(0, Math.min(index ?? tabs.length, tabs.length));
      next = { ...next, tabs: [...tabs.slice(0, at), tab, ...tabs.slice(at)], active: tab };
    }
    return next;
  });
  return prune({ groups, focused: target.id });
}

/** 탭을 기준 그룹의 왼쪽·오른쪽에 새 그룹으로 분리한다. 그룹 수가 상한이면 그대로 둔다. */
export function splitTab(layout: Layout, tab: number, besideGroup: number, side: Side = "right"): Layout {
  const source = groupOf(layout, tab);
  const anchor = layout.groups.findIndex(group => group.id === besideGroup);
  if (!source || anchor < 0 || layout.groups.length >= MAX_GROUPS) return layout;
  if (source.id === besideGroup && source.tabs.length === 1) return layout;
  const id = nextGroupId(layout);
  const anchorSize = layout.groups[anchor].size;
  const created: Group = { id, tabs: [tab], active: tab, size: anchorSize / 2 };
  const groups = layout.groups.map(group => {
    const next = withoutTab(group, tab);
    return group.id === besideGroup ? { ...next, size: anchorSize / 2 } : next;
  });
  const at = side === "right" ? anchor + 1 : anchor;
  return prune({ groups: [...groups.slice(0, at), created, ...groups.slice(at)], focused: id });
}

/** 새 탭을 기준 그룹 옆의 새 그룹에 연다(같은 대상 복제·새 터미널). */
export function addTabInSplit(layout: Layout, tab: number, besideGroup: number, side: Side = "right"): Layout {
  if (layout.groups.length >= MAX_GROUPS) return addTab(layout, tab, besideGroup);
  const anchor = layout.groups.findIndex(group => group.id === besideGroup);
  if (anchor < 0) return addTab(layout, tab);
  const id = nextGroupId(layout);
  const anchorSize = layout.groups[anchor].size;
  const groups = layout.groups.map(group => group.id === besideGroup ? { ...group, size: anchorSize / 2 } : group);
  const at = side === "right" ? anchor + 1 : anchor;
  return { groups: [...groups.slice(0, at), { id, tabs: [tab], active: tab, size: anchorSize / 2 }, ...groups.slice(at)], focused: id };
}

/** 그룹 경계 index(왼쪽 그룹 index)의 비율을 바꾼다. ratio는 두 그룹 합에서 왼쪽이 차지하는 비율이다. */
export function resizeGroups(layout: Layout, boundary: number, ratio: number): Layout {
  const left = layout.groups[boundary];
  const right = layout.groups[boundary + 1];
  if (!left || !right) return layout;
  const total = left.size + right.size;
  const clamped = Math.min(0.85, Math.max(0.15, ratio));
  return {
    ...layout,
    groups: layout.groups.map((group, index) => index === boundary ? { ...group, size: total * clamped }
      : index === boundary + 1 ? { ...group, size: total * (1 - clamped) } : group),
  };
}

/** 모든 그룹을 하나로 합친다. 탭 순서는 그룹 순서를 따른다. */
export function mergeGroups(layout: Layout): Layout {
  const tabs = layout.groups.flatMap(group => group.tabs);
  const first = layout.groups[0];
  return { groups: [{ id: first.id, tabs, active: focusedTab(layout) ?? tabs[0] ?? null, size: 1 }], focused: first.id };
}

/** 선택을 바꾸지 않고 그룹 끝에 탭을 붙인다(부착 대기 중인 미리보기 탭). 빈 그룹이면 그 탭이 활성이 된다. */
export function appendTab(layout: Layout, tab: number, groupId = layout.focused): Layout {
  const target = layout.groups.some(group => group.id === groupId) ? groupId : layout.groups[0].id;
  return {
    ...layout,
    groups: layout.groups.map(group => group.id === target ? { ...group, tabs: [...group.tabs, tab], active: group.active ?? tab } : group),
  };
}
