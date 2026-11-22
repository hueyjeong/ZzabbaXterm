import type {
  ApprovedEditorFile, ConfirmEditorDiscard, EditorFileError, EditorTransport,
} from "./contract";
import type { editor } from "monaco-editor/editor";
import { trackLineEndings } from "./lineEndings";

interface EditorBuffer {
  getValue(eol?: undefined, preserveBOM?: boolean): string;
  setValue(text: string): void;
  getVersionId(): number;
  getAlternativeVersionId(): number;
  onDidChangeContent(listener: (event: editor.IModelContentChangedEvent) => void): { dispose(): void };
}

export type EditorStatus =
  | { readonly kind: "ready" | "saved" | "compare" }
  | { readonly kind: "conflict"; readonly originalExists: boolean | undefined; readonly message?: string | undefined }
  | { readonly kind: "unknown"; readonly message: string }
  | EditorFileError;

export interface EditorState {
  readonly dirty: boolean;
  readonly needsReload: boolean;
  readonly saving: boolean;
  readonly busy: "save" | "compare" | "reload" | null;
  readonly status: EditorStatus;
  /** null은 삭제 확인, undefined는 비교 화면을 표시할 원본이 없음을 뜻한다. */
  readonly comparison: string | null | undefined;
}

export interface EditorConnection {
  setActive(active: boolean): void;
  save(): Promise<void>;
  compare(): Promise<void>;
  reload(confirm: ConfirmEditorDiscard): Promise<void>;
  dismissComparison(): void;
  requestClose(confirm: ConfirmEditorDiscard, close: (handle: ApprovedEditorFile["handle"]) => void): Promise<void>;
  dispose(): void;
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

export function connectEditor(
  file: ApprovedEditorFile,
  model: EditorBuffer,
  transport: EditorTransport,
  publish: (state: EditorState) => void,
  active = true,
): EditorConnection {
  let savedVersion = model.getAlternativeVersionId();
  const lineEndings = trackLineEndings(file.text, savedVersion);
  let status: EditorStatus = { kind: "ready" };
  let comparison: EditorState["comparison"];
  let busy: EditorState["busy"] = null;
  let uncertain = false;
  let needsReload = false;
  let saving = false;
  let closing = false;
  let disposed = false;
  let generation = 0;
  const dirty = () => uncertain || savedVersion !== model.getAlternativeVersionId();
  const update = () => {
    if (active && !disposed) publish({ dirty: dirty(), saving, busy, status, comparison, needsReload: needsReload || uncertain });
  };
  const content = model.onDidChangeContent((event) => {
    lineEndings.change(event, model.getAlternativeVersionId());
    update();
  });
  const unsubscribe = transport.subscribeDisconnect?.(file.handle, (message) => {
    if (disposed) return;
    uncertain ||= busy === "save";
    comparison = undefined;
    status = { kind: uncertain ? "unknown" : "disconnected", message };
    generation++;
    if (busy !== "reload") busy = null;
    update();
  });
  const current = (id: number) => active && !disposed && generation === id;
  function start(action: NonNullable<EditorState["busy"]>) {
    const id = ++generation;
    busy = action;
    update();
    return id;
  }
  function finish() {
    busy = null;
    update();
  }
  async function confirmDiscard(confirm: ConfirmEditorDiscard, reason: "reload" | "close", id: number) {
    try {
      return await confirm({ handle: file.handle, label: file.label, reason });
    } catch (error) {
      if (current(id)) {
        status = { kind: "failed", message: errorMessage(error) };
        update();
      }
      return false;
    }
  }

  function dispose() {
    if (disposed) return;
    disposed = true;
    generation++;
    content.dispose();
    unsubscribe?.();
  }

  update();
  return {
    setActive(value) {
      if (active === value || disposed) return;
      active = value;
      generation++;
      if (busy === "compare") busy = null;
      update();
    },
    async save() {
      if (!active || disposed || busy || saving || closing || needsReload || uncertain || !dirty()) return;
      const version = model.getAlternativeVersionId();
      let text: string;
      try {
        text = lineEndings.serialize(model.getValue(undefined, true));
      } catch (error) {
        comparison = undefined;
        status = { kind: "failed", message: errorMessage(error) };
        update();
        return;
      }
      saving = true;
      const id = start("save");
      try {
        const result = await transport.save(file.handle, text);
        if (!current(id)) return;
        if (result.kind === "saved") {
          savedVersion = version;
          uncertain = false;
          comparison = undefined;
          status = { kind: "saved" };
        } else if (result.kind === "conflict") {
          comparison = result.current;
          status = {
            kind: "conflict",
            originalExists: result.current === undefined ? undefined : result.current !== null,
            message: result.message,
          };
        } else {
          comparison = undefined;
          uncertain ||= result.kind === "unknown";
          status = result;
        }
      } catch (error) {
        if (current(id)) {
          comparison = undefined;
          uncertain = true;
          status = { kind: "unknown", message: errorMessage(error) };
        }
      } finally {
        saving = false;
        if (current(id) || busy === "save") {
          if (!current(id) && !disposed) {
            comparison = undefined;
            uncertain = true;
            status = { kind: "unknown", message: "탭이 바뀌었습니다" };
          }
          finish();
        } else update();
      }
    },
    async compare() {
      if (!active || disposed || busy || closing) return;
      comparison = undefined;
      const id = start("compare");
      try {
        const result = await transport.compare(file.handle);
        if (!current(id)) return;
        if (result.kind === "read") {
          comparison = result.text;
          status = { kind: "compare" };
        } else {
          status = result;
        }
      } catch (error) {
        if (current(id)) status = { kind: "failed", message: errorMessage(error) };
      } finally {
        if (current(id)) finish();
      }
    },
    async reload(confirm: ConfirmEditorDiscard) {
      if (!active || disposed || busy || saving || closing) return;
      const version = model.getVersionId();
      const id = start("reload");
      try {
        if (!await confirmDiscard(confirm, "reload", id) || !current(id) || model.getVersionId() !== version) return;
        // Rust 기준을 갱신한 뒤 응답이 무효화되면 다시 승인받기 전에는 저장을 막는다.
        const previousNeedsReload = needsReload;
        needsReload = true;
        comparison = undefined;
        const result = await transport.reload(file.handle);
        if (!current(id) || model.getVersionId() !== version) {
          if (status.kind !== "unknown" && status.kind !== "disconnected") {
            status = { kind: "failed", message: "다시 읽기가 필요합니다" };
          }
          return;
        }
        if (result.kind !== "read") {
          needsReload = previousNeedsReload;
          status = result;
          return;
        }
        model.setValue(result.text);
        needsReload = false;
        savedVersion = model.getAlternativeVersionId();
        lineEndings.reset(result.text, savedVersion);
        uncertain = false;
        comparison = undefined;
        status = { kind: "ready" };
      } catch (error) {
        if (current(id)) status = { kind: "failed", message: errorMessage(error) };
      } finally {
        finish();
      }
    },
    dismissComparison() {
      if (!active || disposed || closing) return;
      comparison = undefined;
      update();
    },
    async requestClose(confirm: ConfirmEditorDiscard, close: (handle: ApprovedEditorFile["handle"]) => void) {
      if (disposed || closing || saving || busy === "reload") return;
      closing = true;
      const version = model.getVersionId();
      const id = generation;
      try {
        if (dirty() && !await confirmDiscard(confirm, "close", id)) return;
        if (!disposed && generation === id && model.getVersionId() === version) {
          dispose();
          close(file.handle);
        }
      } finally {
        closing = false;
      }
    },
    dispose,
  };
}
