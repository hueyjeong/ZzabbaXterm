import type { editor } from "monaco-editor/editor";

const newline = /\r\n|\r|\n/g;
const endings: Record<string, string> = { "\n": "0", "\r\n": "1", "\r": "2" };
const textEndings = ["\n", "\r\n", "\r"];

export function trackLineEndings(text: string, version: number) {
  // false는 Monaco 이벤트만으로 원래 개행을 대응시킬 수 없는 버전이다.
  type Separators = string | null | false;
  let separators: Separators = null;
  let history: Map<number, Separators> | undefined;

  function reset(raw: string, version: number) {
    const original = raw.match(newline);
    separators = original && (original.includes("\r") || original.some((eol) => eol !== original[0]))
      ? original.map((eol) => endings[eol]).join("") : null;
    history = separators === null ? undefined : new Map([[version, separators]]);
  }

  reset(text, version);
  return {
    reset,
    change(event: editor.IModelContentChangedEvent, version: number) {
      if (!history) return;
      if (event.isUndoing || event.isRedoing) {
        const restored = history.get(version);
        if (restored !== undefined) {
          separators = restored;
          return;
        }
        separators = false;
      }
      if (event.isFlush) separators = false;
      if (event.isEolChange && separators !== false) separators = null;
      if (typeof separators === "string") {
        // Monaco는 기존 좌표 기준으로 뒤쪽 변경부터 전달한다.
        for (const { range, text } of event.changes) {
          const start = range.startLineNumber - 1;
          const end = range.endLineNumber - 1;
          let inserted = text.match(newline)?.map((eol) => endings[eol]).join("") ?? "";
          if (start !== end && inserted) {
            const removed = separators.slice(start, end);
            const uniform = inserted[0];
            // 양쪽에 개행이 하나뿐이면 대응 위치가 유일하다.
            if (removed.length === 1 && inserted.length === 1) inserted = removed;
            else if (removed !== uniform.repeat(removed.length) || inserted !== uniform.repeat(inserted.length)) {
              separators = false;
              break;
            }
          }
          if (start !== end || inserted) separators = separators.slice(0, start) + inserted + separators.slice(end);
        }
      }
      history.set(version, separators);
    },
    serialize(normalized: string) {
      const original = separators;
      if (original === null) return normalized;
      if (original === false) throw new Error("원래 줄바꿈을 확인할 수 없어 저장할 수 없습니다");
      let index = 0;
      const text = normalized.replace(newline, (eol) => textEndings[Number(original[index++])] ?? eol);
      if (index !== original.length || (original.includes("20") && text.match(newline)?.length !== index)) {
        throw new Error("원래 줄바꿈을 복원할 수 없습니다");
      }
      return text;
    },
  };
}
