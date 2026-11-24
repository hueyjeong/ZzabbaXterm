import type { ExpectedEntry, SavedConnections, SavedGroup } from "./connectionIpc";

type Groups = SavedConnections["groups"];

/** 최상위를 1단계로 센 그룹 중첩 최대 깊이다. Rust `MAX_GROUP_DEPTH`와 같다. */
export const MAX_GROUP_DEPTH = 8;
// select의 option은 일반 공백을 접으므로 NBSP로 들여 쓴다.
const INDENT = " ".repeat(3);

/** 부모 ID별 하위 그룹이다. 없는 부모를 가리키는 그룹은 최상위(undefined)에 둔다. */
export function groupChildren(groups: Groups) {
  const known = new Set(groups.map(({ profile }) => profile.id));
  const children = new Map<string | undefined, SavedGroup[]>();
  for (const { profile } of groups) {
    const parent = profile.parentId && known.has(profile.parentId) ? profile.parentId : undefined;
    children.set(parent, [...children.get(parent) ?? [], profile]);
  }
  return children;
}

/**
 * 중첩 그룹을 부모 아래에 깊이만큼 들여 쓴 선택지로 펼친다. 부모가 없는 그룹은 최상위에 둔다.
 * 이름이 겹치면 트리처럼 ID 앞 8자를 붙인다. exclude를 주면 그 그룹과 하위 그룹을 뺀다.
 */
export function groupOptions(groups: Groups, exclude?: string) {
  const names = new Map<string, number>();
  for (const { profile } of groups) names.set(profile.displayName, (names.get(profile.displayName) ?? 0) + 1);
  const children = groupChildren(groups);
  const options: { id: string; label: string }[] = [];
  const seen = new Set<string>();
  const walk = (parent: string | undefined, depth: number) => {
    for (const group of children.get(parent) ?? []) {
      if (seen.has(group.id) || group.id === exclude) continue;
      seen.add(group.id);
      const suffix = (names.get(group.displayName) ?? 0) > 1 ? ` ${group.id.slice(0, 8)}` : "";
      options.push({ id: group.id, label: INDENT.repeat(depth) + group.displayName + suffix });
      walk(group.id, depth + 1);
    }
  };
  walk(undefined, 0);
  return options;
}

/** 최상위를 1로 센 그룹의 깊이다. 순환이면 지금까지 센 값에서 멈춘다. */
export function groupDepth(groups: Groups, id: string) {
  const parents = new Map(groups.map(({ profile }) => [profile.id, profile.parentId]));
  const seen = new Set<string>();
  for (let current: string | undefined = id; current && parents.has(current) && !seen.has(current); current = parents.get(current)) {
    seen.add(current);
  }
  return seen.size;
}

/** 그룹 자신을 1로 센 하위 그룹 트리의 높이다. */
export function subtreeHeight(groups: Groups, id: string) {
  const children = groupChildren(groups);
  const seen = new Set<string>();
  const height = (current: string): number => {
    if (seen.has(current)) return 0;
    seen.add(current);
    return 1 + Math.max(0, ...(children.get(current) ?? []).map(({ id }) => height(id)));
  };
  return height(id);
}

/** 하위 포함 그룹 삭제에서 함께 지워질 하위 그룹과 세션이다. 루트 그룹은 넣지 않는다. */
export function groupContents({ groups, targets }: SavedConnections, id: string) {
  const removed = new Set([id]);
  for (let before = 0; before !== removed.size;) {
    before = removed.size;
    for (const { profile } of groups) if (profile.parentId && removed.has(profile.parentId)) removed.add(profile.id);
  }
  const subgroups = groups.filter(({ profile }) => profile.id !== id && removed.has(profile.id));
  const sessions = targets.filter(({ profile }) => profile.groupId !== undefined && removed.has(profile.groupId));
  const expected: ExpectedEntry[] = [
    ...subgroups.map(({ profile, revision }) => ({ kind: "group" as const, id: profile.id, revision })),
    ...sessions.map(({ profile, revision }) => ({ kind: "target" as const, id: profile.id, revision })),
  ];
  return { groups: subgroups.length, targets: sessions.length, expected };
}
