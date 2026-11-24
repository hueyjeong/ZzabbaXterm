/** 클립보드에 글자를 쓴다. 쓰기 실패는 무시한다. */
export function copyText(text: string): void {
  void navigator.clipboard?.writeText(text).catch(() => undefined);
}

/** 클립보드의 글자를 읽는다. 읽지 못하면 undefined다. */
export async function readText(): Promise<string | undefined> {
  try {
    return await navigator.clipboard?.readText();
  } catch {
    return undefined;
  }
}
