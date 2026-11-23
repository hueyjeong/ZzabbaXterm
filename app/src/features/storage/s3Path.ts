// 접두어 문자열을 화면 조각으로 나눌 때도 원본 문자를 그대로 둔다. `..`·중복 구분자 정리는 하지 않는다.

export type PrefixSegment = Readonly<{ label: string; prefix: string }>;

/** "a//b/c" → [a → "a/", "" → "a//", b → "a//b/", c → "a//b/c"] */
export function prefixSegments(prefix: string): PrefixSegment[] {
  const segments: PrefixSegment[] = [];
  let start = 0;
  for (let index = 0; index < prefix.length; index += 1) {
    if (prefix[index] !== "/") continue;
    segments.push({ label: prefix.slice(start, index), prefix: prefix.slice(0, index + 1) });
    start = index + 1;
  }
  if (start < prefix.length) segments.push({ label: prefix.slice(start), prefix });
  return segments;
}

/** 버킷 루트면 null이다. */
export function parentPrefix(prefix: string) {
  if (!prefix) return null;
  const segments = prefixSegments(prefix);
  return segments.length > 1 ? segments[segments.length - 2].prefix : "";
}

export function relativeName(full: string, base: string) {
  return base && full.startsWith(base) && full.length > base.length ? full.slice(base.length) : full;
}

/** 폴더 이름 끝의 구분자 하나만 표시에서 뺀다. */
export function folderLabel(relative: string) {
  return relative.length > 1 && relative.endsWith("/") ? relative.slice(0, -1) : relative;
}
