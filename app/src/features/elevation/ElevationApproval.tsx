import { approvalFields, type ElevationRequest } from "./elevation";

/** 승인 카드는 받은 요청의 인자 경계를 그대로 보여 주고 버튼 입력만 돌려준다. */
export function ElevationApproval({ request, onApprove, onCancel }: {
  readonly request: ElevationRequest;
  readonly onApprove: () => void;
  readonly onCancel: () => void;
}) {
  return <div className="elevation-approval" role="group" aria-label="sudo 승격 승인">
    <dl>
      {approvalFields(request).map(({ label, value }) =>
        <div key={label}><dt>{label}</dt><dd>{value === "" ? "∅" : value}</dd></div>)}
    </dl>
    <div className="connection-actions">
      <button type="button" onClick={onApprove}>승인</button>
      <button type="button" onClick={onCancel}>취소</button>
    </div>
  </div>;
}
