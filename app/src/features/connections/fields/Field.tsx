import type { ReactNode } from "react";

export type FieldProps = Readonly<{
  /** 필드 아래에 표시할 오류다. */
  error(field: string): string | undefined;
  /** 필드를 떠날 때 건드린 필드로 표시한다. */
  touch(field: string): void;
}>;

export function Field({ label, error, children }: Readonly<{ label: string; error?: string; children: ReactNode }>) {
  return <div className="editor-field">
    <label>{label} {children}</label>
    {error && <div role="alert" className="field-error">{error}</div>}
  </div>;
}
