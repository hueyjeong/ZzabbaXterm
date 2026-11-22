import { useImperativeHandle, useLayoutEffect, useRef, useState, type Ref } from "react";
import type { ApprovedEditorFile, ConfirmEditorDiscard, EditorFileHandle, EditorTransport } from "./contract";
import { connectEditor, type EditorConnection, type EditorState } from "./editor";
import { createFileModel, editor } from "./monaco";
import "./editor.css";

export interface EditorTabActions {
  setActive(active: boolean): void;
  requestClose(): Promise<void>;
}

export interface EditorTabProps {
  readonly ref?: Ref<EditorTabActions>;
  readonly file: ApprovedEditorFile;
  readonly active: boolean;
  readonly focusRequested?: boolean;
  readonly transport: EditorTransport;
  readonly confirmDiscard: ConfirmEditorDiscard;
  /** 호출자가 해당 탭을 실제로 제거한다. 비활성 탭도 마운트 상태를 유지한다. */
  readonly onClose: (handle: EditorFileHandle) => void;
}

function statusText({ busy, dirty, status }: EditorState) {
  if (busy === "save") return "저장 중";
  if (busy) return "읽는 중";
  switch (status.kind) {
    case "conflict": {
      const label = status.originalExists === undefined ? "원본 비교 불가" : status.originalExists ? "원본 변경" : "원본 없음";
      return status.message ? `${label}: ${status.message}` : label;
    }
    case "unknown": return `저장 결과 미확인: ${status.message}`;
    case "disconnected": return `연결 끊김: ${status.message}`;
    case "denied": return `파일 접근 거부: ${status.message}`;
    case "failed": return `작업 실패: ${status.message}`;
    case "saved": return dirty ? "미저장" : "저장됨";
    default: return dirty ? "미저장" : "";
  }
}

const options = {
  automaticLayout: true,
  minimap: { enabled: false },
  fontFamily: '"Cascadia Mono", "DejaVu Sans Mono", monospace',
  fontSize: 14,
  scrollBeyondLastLine: false,
  theme: "vs-dark",
};

/** 파일 핸들로 키를 지정하고, 같은 핸들은 기존 탭을 선택한다. */
export function EditorTab(props: EditorTabProps) {
  return <EditorTabContent key={props.file.handle} {...props} />;
}

function EditorTabContent({ ref, file, active, focusRequested, transport, confirmDiscard, onClose }: EditorTabProps) {
  const host = useRef<HTMLDivElement>(null);
  const diffHost = useRef<HTMLDivElement>(null);
  const mono = useRef<editor.IStandaloneCodeEditor | null>(null);
  const diff = useRef<editor.IStandaloneDiffEditor | null>(null);
  const connection = useRef<EditorConnection | null>(null);
  const [binding] = useState(() => ({ file, transport }));
  const [state, setState] = useState<EditorState>({ dirty: false, saving: false, busy: null, status: { kind: "ready" }, comparison: undefined, needsReload: false });
  const comparison = state.comparison;
  const showingDiff = comparison != null;

  useImperativeHandle(ref, () => ({
    setActive(value) { connection.current?.setActive(value); },
    async requestClose() { await connection.current?.requestClose(confirmDiscard, onClose); },
  }), [confirmDiscard, onClose]);

  useLayoutEffect(() => {
    if (!host.current) return;
    const model = createFileModel(binding.file);
    const view = editor.create(host.current, { ...options, model, ariaLabel: binding.file.label });
    const buffer = connectEditor(binding.file, model, binding.transport, setState, false);
    mono.current = view;
    connection.current = buffer;
    return () => {
      connection.current = null;
      mono.current = null;
      buffer.dispose();
      view.dispose();
      diff.current?.setModel(null);
      model.dispose();
    };
  }, [binding]);

  useLayoutEffect(() => {
    const modified = mono.current?.getModel();
    if (comparison == null || !diffHost.current || !modified) return;
    const original = editor.createModel(comparison, "plaintext");
    const view = editor.createDiffEditor(diffHost.current, {
      ...options, originalEditable: false, readOnly: true,
      renderSideBySide: true, useInlineViewWhenSpaceIsLimited: false, enableSplitViewResizing: false,
      originalAriaLabel: "현재 원본", modifiedAriaLabel: "내 편집본",
    });
    view.setModel({ original, modified });
    diff.current = view;
    return () => {
      diff.current = null;
      view.dispose();
      original.dispose();
    };
  }, [comparison]);

  useLayoutEffect(() => {
    connection.current?.setActive(active);
    if (!active) return;
    if (showingDiff) diff.current?.layout();
    else mono.current?.layout();
  }, [active, showingDiff]);

  useLayoutEffect(() => {
    if (!active || !focusRequested) return;
    if (showingDiff) diff.current?.getModifiedEditor().focus();
    else mono.current?.focus();
  }, [active, focusRequested, showingDiff]);

  return (
    <section className="editor-tab" aria-label={file.label} hidden={!active} onKeyDownCapture={(event) => {
      if (!active || event.nativeEvent.isComposing || !(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey || event.key.toLowerCase() !== "s") return;
      event.preventDefault();
      event.stopPropagation();
      void connection.current?.save();
    }}>
      <header className="editor-toolbar">
        <button type="button" disabled={!active || state.saving || Boolean(state.busy) || !state.dirty || state.needsReload} onClick={() => void connection.current?.save()}>저장</button>
        <button type="button" disabled={!active || Boolean(state.busy)} onClick={() => void connection.current?.compare()}>원본과 비교</button>
        <button type="button" disabled={!active || state.saving || Boolean(state.busy)} onClick={() => void connection.current?.reload(confirmDiscard)}>다시 읽기</button>
        {comparison !== undefined && <button type="button" onClick={() => connection.current?.dismissComparison()}>비교 닫기</button>}
        <output role="status">
          {[statusText(state), !state.busy && state.needsReload && "다시 읽기 필요"].filter(Boolean).join(" · ")}
        </output>
      </header>
      <div className="editor-surface" ref={host} hidden={showingDiff} />
      {showingDiff && <>
        <div className="editor-diff-labels"><span>현재 원본</span><span>내 편집본</span></div>
        <div className="editor-surface" ref={diffHost} />
      </>}
    </section>
  );
}
