import { editor, Uri } from "monaco-editor/editor";
import "monaco-editor/features/clipboard/register";
import "monaco-editor/features/find/register";
import "monaco-editor/features/diffEditor/register";
import EditorWorker from "monaco-editor/editor/editor.worker?worker";
import type { ApprovedEditorFile } from "./contract";

self.MonacoEnvironment = { getWorker: () => new EditorWorker() };

// ui/tokens.css와 같은 배경·강조색을 쓴다.
export const EDITOR_THEME = "zzabba-dark";
let themeDefined = false;
/** 편집기를 만들기 직전에 한 번 등록한다. 모듈 로드 시 등록하면 Monaco 서비스가 미리 초기화된다. */
export function defineEditorTheme() {
  if (themeDefined) return;
  themeDefined = true;
  editor.defineTheme(EDITOR_THEME, {
  base: "vs-dark", inherit: true, rules: [],
  colors: {
    "editor.background": "#0f1417", "editorGutter.background": "#0f1417", "editor.foreground": "#dbe4e0",
    "editorLineNumber.foreground": "#4d5a56", "editorLineNumber.activeForeground": "#8f9e99",
    "editor.lineHighlightBackground": "#151c20", "editor.selectionBackground": "#2f4a44",
    "editorCursor.foreground": "#5fd1a8", "editorWidget.background": "#20292e", "editorWidget.border": "#3a474e",
  },
  });
}

export function createFileModel(file: ApprovedEditorFile) {
  return editor.createModel(file.text, "plaintext", Uri.from({
    scheme: "zzabba-editor",
    path: "/file",
    query: file.handle,
  }));
}

export { editor };
