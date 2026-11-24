import { cloneElement, useId, type ReactElement } from "react";

export type FieldProps = Readonly<{
  /** 필드 아래에 표시할 오류다. */
  error(field: string): string | undefined;
  /** 필드를 떠날 때 건드린 필드로 표시한다. */
  touch(field: string): void;
}>;

type Control = ReactElement<{ "aria-invalid"?: boolean; "aria-describedby"?: string }>;

/** 입력 하나와 그 아래 오류다. 오류가 있으면 입력의 aria-invalid와 aria-describedby를 오류 텍스트에 잇는다. */
export function Field({ label, error, children }: Readonly<{ label: string; error?: string; children: Control }>) {
  const errorId = useId();
  return <div className="editor-field">
    <label>{label} {error ? cloneElement(children, { "aria-invalid": true, "aria-describedby": errorId }) : children}</label>
    {error && <div id={errorId} className="field-error">{error}</div>}
  </div>;
}
