import { useLayoutEffect, useRef, useState } from "react";
import { ElevationApproval } from "./ElevationApproval";
import { parseArguments, validateElevationRequest, type ElevationRequest } from "./elevation";

/** 승격 실행은 실행 파일과 인자를 구조로 받는다. 셸 문자열을 다시 해석하지 않는다. */
export function ElevationDialog({ context, onClose, onSubmit }: {
  readonly context?: Readonly<{ targetId?: string; target?: string; account?: string }>;
  readonly onClose: () => void;
  readonly onSubmit: (request: ElevationRequest) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [program, setProgram] = useState("/usr/bin/id");
  const [argumentsText, setArgumentsText] = useState("-u");
  const [item, setItem] = useState("");
  const [error, setError] = useState<string>();
  const submitted = useRef(false);
  useLayoutEffect(() => {
    const modal = dialog.current;
    modal?.showModal();
    return () => modal?.close();
  }, []);
  const request: ElevationRequest = {
    program: program.trim(),
    args: parseArguments(argumentsText),
    item: item.trim(),
    ...context,
  };
  const approve = () => {
    if (submitted.current) return;
    const invalid = validateElevationRequest(request);
    if (invalid) {
      setError(invalid);
      return;
    }
    setError(undefined);
    submitted.current = true;
    onSubmit(request);
  };
  return <dialog ref={dialog} className="connection-wizard" aria-label="sudo 승격 실행" onCancel={(event) => {
    event.preventDefault();
    onClose();
  }}>
    <form method="dialog" onSubmit={(event) => { event.preventDefault(); approve(); }}>
      <h2>{context?.targetId ? "원격 sudo 승격 실행" : "sudo 승격 실행"}</h2>
      <label>실행 파일 절대 경로
        <input value={program} onChange={(event) => setProgram(event.target.value)} />
      </label>
      <label>인자 (한 줄에 하나)
        <textarea rows={4} value={argumentsText} onChange={(event) => setArgumentsText(event.target.value)} />
      </label>
      <label>저장 암호 항목 ID
        <input value={item} onChange={(event) => setItem(event.target.value)} />
      </label>
      {error && <p role="alert">{error}</p>}
      <ElevationApproval request={request} onApprove={approve} onCancel={onClose} />
    </form>
  </dialog>;
}
