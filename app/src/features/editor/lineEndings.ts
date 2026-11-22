import type { editor } from "monaco-editor/editor";

const newline = /\r\n|\r|\n/g;
const endings: Record<string, string> = { "\n": "0", "\r\n": "1", "\r": "2" };
const textEndings = ["\n", "\r\n", "\r"];

export function trackLineEndings(text: string, version: number) {
  let separators: string | null = null;
  let history: Map<number, string | null> | undefined;

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
      if (event.isFlush) {
        reset("", version);
        return;
      }
      if (!history) return;
      if (event.isUndoing || event.isRedoing) {
        const restored = history.get(version);
        if (restored !== undefined) {
          separators = restored;
          return;
        }
      }
      if (event.isEolChange) separators = null;
      if (separators !== null) {
        // Monaco는 기존 좌표 기준으로 뒤쪽 변경부터 전달한다.
        for (const { range, text } of event.changes) {
          const inserted = text.match(newline)?.map((eol) => endings[eol]).join("") ?? "";
          if (range.startLineNumber !== range.endLineNumber || inserted) {
            separators = separators.slice(0, range.startLineNumber - 1) + inserted + separators.slice(range.endLineNumber - 1);
          }
        }
      }
      history.set(version, separators);
    },
    serialize(normalized: string) {
      const original = separators;
      if (original === null) return normalized;
      let index = 0;
      const text = normalized.replace(newline, (eol) => textEndings[Number(original[index++])] ?? eol);
      if (index !== original.length || (original.includes("20") && text.match(newline)?.length !== index)) {
        throw new Error("원래 줄바꿈을 복원할 수 없습니다");
      }
      return text;
    },
  };
}
