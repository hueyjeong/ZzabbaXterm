declare const approvedFile: unique symbol;

/** Rust가 발급한 불투명 핸들. 경로나 파일명으로 만들지 않는다. */
export type EditorFileHandle = string & { readonly [approvedFile]: true };

export interface ApprovedEditorFile {
  readonly handle: EditorFileHandle;
  readonly label: string;
  readonly text: string;
}

export type EditorFileError =
  | { readonly kind: "denied" | "disconnected" | "failed"; readonly message: string };

export type EditorReadResult =
  | { readonly kind: "read"; readonly text: string }
  | EditorFileError;

export type EditorSaveResult =
  | { readonly kind: "saved" }
  /** null은 삭제 확인, undefined는 현재 원본 미확인이다. */
  | { readonly kind: "conflict"; readonly current?: string | null | undefined; readonly message?: string | undefined }
  | { readonly kind: "unknown"; readonly message: string }
  | EditorFileError;

export interface EditorTransport {
  /** 비교는 저장 기준을 변경하지 않는다. 다시 읽기가 성공할 때 저장 기준을 갱신한다. */
  compare(handle: EditorFileHandle): Promise<EditorReadResult>;
  reload(handle: EditorFileHandle): Promise<EditorReadResult>;
  /** saved는 Rust가 최종 교체 성공을 확인한 뒤에만 반환한다. */
  /** 교체 여부를 모르면 단절 원인이더라도 unknown을 반환한다. */
  save(handle: EditorFileHandle, text: string): Promise<EditorSaveResult>;
  /** 단절 알림을 제공하는 전송은 탭이 닫힐 때 호출할 구독 해제 함수를 반환한다. */
  subscribeDisconnect?(handle: EditorFileHandle, notify: (message: string) => void): () => void;
}

export type ConfirmEditorDiscard = (request: {
  readonly handle: EditorFileHandle;
  readonly label: string;
  readonly reason: "reload" | "close";
}) => Promise<boolean>;
