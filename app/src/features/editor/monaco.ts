import { editor, Uri } from "monaco-editor/editor";
import "monaco-editor/features/clipboard/register";
import "monaco-editor/features/find/register";
import "monaco-editor/features/diffEditor/register";
import EditorWorker from "monaco-editor/editor/editor.worker?worker";
import type { ApprovedEditorFile } from "./contract";

self.MonacoEnvironment = { getWorker: () => new EditorWorker() };

export function createFileModel(file: ApprovedEditorFile) {
  return editor.createModel(file.text, "plaintext", Uri.from({
    scheme: "zzabba-editor",
    path: "/file",
    query: file.handle,
  }));
}

export { editor };
