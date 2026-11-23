import { Ban, ChevronDown, CircleCheck, CircleX, Download, Hourglass, LoaderCircle, ShieldX, Upload, type LucideIcon } from "lucide-react";
import { exactBytes, formatBytes } from "../files/FileBrowser";
import { IconButton } from "../ui/controls";
import type { Downloaded, S3Error, S3Session } from "./s3";

export type Problem = S3Error | "unavailable";
export type Transfer = Readonly<{
  id: string; direction: "upload" | "download"; bucket: string; region: string; key: string; size: number | null; local?: string;
}> & (
  | Readonly<{ state: "selecting" | "running" | "cancelled" | "denied" }>
  | Readonly<{ state: "failed"; problem: Problem }>
  | Readonly<{ state: "uploaded"; bytes: number }>
  | Readonly<{ state: "downloaded"; result: Downloaded }>
);

const stages = {
  profile: "SSO 프로필", listBuckets: "버킷 목록", headBucket: "버킷 확인", listObjects: "객체 목록", uploadSource: "업로드 원본",
  identity: "AWS 신원 확인",
  putObject: "객체 업로드", getObject: "객체 받기", downloadBody: "다운로드 본문",
  downloadWrite: "다운로드 파일 쓰기", downloadCommit: "다운로드 파일 확정",
} satisfies Record<S3Error["stage"], string>;

const failures = {
  invalidSsoProfile: "유효하지 않은 SSO 프로필", invalidRegion: "유효하지 않은 리전", invalidBucket: "유효하지 않은 버킷",
  retestRequired: "저장된 연결을 다시 시험하고 저장해야 합니다",
  accountMismatch: "SSO 프로필의 승인 계정과 AWS 신원이 다릅니다",
  invalidResponse: "잘못된 서비스 응답", credentials: "SSO 자격 증명 오류", ssoLoginRequired: "같은 SSO 프로필로 다시 로그인 필요",
  request: "요청 실패", timedOut: "응답 시간 초과", loginFailed: "SSO 재로그인 실패", bodyRead: "본문 읽기 실패", checksum: "체크섬 검증 실패",
  destination: "승인한 HTTPS 주소 방식과 요청 주소가 다릅니다", tls: "HTTPS 인증서 검증 실패",
};

function describe(problem: Problem, compatible: boolean) {
  if (problem === "unavailable") return { detail: "네이티브 요청 결과 수신 실패" };
  const { failure, stage, bytes } = problem;
  let message: string;
  let recoveryPath: string | undefined;
  switch (failure.kind) {
    case "redirect": message = `리디렉션 차단 (${failure.status}${failure.code ? ` ${failure.code}` : ""})`; break;
    case "signingRegion": message = `서명 리전 오류 (${failure.status} ${failure.code})`; break;
    case "service": {
      const { status, code } = failure;
      const expired = code === "ExpiredToken" || code === "ExpiredTokenException";
      const category = expired ? (compatible ? "호환 서비스 자격 증명 만료" : "AWS 임시 자격 증명 만료") :
        code === "AccessDenied" || code === "AccessDeniedException" || (code === null && status === 403) ? "권한 거부" : "서비스 오류";
      message = `${category} (${status}${code ? ` ${code}` : ""})`;
      break;
    }
    case "credentials": message = compatible ? "호환 서비스 자격 증명 오류" : failures.credentials; break;
    case "localIo": message = `로컬 파일 오류 (${failure.errorKind})`; break;
    case "recoveryUnconfirmed":
      message = "복구 확인 실패";
      recoveryPath = failure.recoveryPath;
      break;
    case "length": message = failure.expected === null ? "예상 길이 없음" : `길이 불일치 (예상 ${failure.expected} 바이트)`; break;
    case "bucketRegionMismatch": message = `버킷 리전: ${failure.region}`; break;
    default: message = failures[failure.kind];
  }
  return { detail: `${stages[stage]}: ${message} · ${bytes} 바이트`, recoveryPath };
}

/** 오류 한 줄이다. inline이면 전송 큐 칸처럼 알림 역할 없이 같은 문장만 표시한다. */
export function StorageProblem({ problem, session, inline, compatible }: { problem: Problem; session?: S3Session; inline?: boolean; compatible?: boolean }) {
  const { detail, recoveryPath } = describe(problem, compatible ?? !!session?.compatible);
  const content = <>{detail}{recoveryPath !== undefined && <> · 보존 파일: <code>{recoveryPath}</code></>}
    {session && ` · ${session.profile} · ${session.region} · ${session.endpoint}`}</>;
  return inline ? <span className="storage-problem">{content}</span> : <p role="alert" className="storage-problem">{content}</p>;
}

const states: Record<Transfer["state"], { label: string; icon: LucideIcon }> = {
  selecting: { label: "선택 중", icon: Hourglass },
  running: { label: "전송 중", icon: LoaderCircle },
  cancelled: { label: "취소", icon: Ban },
  denied: { label: "승인 거부", icon: ShieldX },
  failed: { label: "실패", icon: CircleX },
  uploaded: { label: "완료", icon: CircleCheck },
  downloaded: { label: "완료", icon: CircleCheck },
};

function result(transfer: Transfer, session: S3Session) {
  switch (transfer.state) {
    case "uploaded": return `${transfer.bytes} 바이트`;
    case "downloaded":
      return `${transfer.result.bytes} 바이트 · ${transfer.result.integrity === "sdkChecksumVerified" ? "SDK 체크섬 검증됨" : "길이만 확인"}`;
    case "failed": return <StorageProblem inline problem={transfer.problem} compatible={!!session.compatible} />;
    default: return "";
  }
}

export function TransferQueue({ transfers, session, open, onToggle }: {
  transfers: readonly Transfer[];
  session: S3Session;
  open: boolean;
  onToggle(): void;
}) {
  const active = transfers.filter(({ state }) => state === "selecting" || state === "running").length;
  const failed = transfers.filter(({ state }) => state === "failed").length;
  return <section className="transfer-queue" aria-label="전송 대기열" data-open={open ? "" : undefined}>
    <header className="transfer-queue-bar">
      <IconButton small icon={ChevronDown} label={open ? "전송 대기열 접기" : "전송 대기열 펼치기"} aria-expanded={open} onClick={onToggle} />
      <h3>전송</h3>
      <span className="transfer-count">{transfers.length}</span>
      {active > 0 && <span className="transfer-pill" data-state="running">{active}개 진행</span>}
      {failed > 0 && <span className="transfer-pill" data-state="failed">{failed}개 실패</span>}
    </header>
    {open && <div className="transfer-queue-body">
      {transfers.length ? <table className="transfer-table" aria-live="polite">
        <thead><tr>
          <th scope="col"><span className="sr-only">방향</span></th><th scope="col">키</th><th scope="col">크기</th>
          <th scope="col">상태</th><th scope="col">결과</th>
        </tr></thead>
        <tbody>{transfers.map(transfer => {
          const { label, icon: StateIcon } = states[transfer.state];
          const Direction = transfer.direction === "upload" ? Upload : Download;
          const size = transfer.state === "uploaded" ? transfer.bytes : transfer.state === "downloaded" ? transfer.result.bytes : transfer.size;
          return <tr key={transfer.id} data-state={transfer.state}>
            <td className="transfer-direction" data-direction={transfer.direction}>
              <Direction aria-label={transfer.direction === "upload" ? "업로드" : "다운로드"} role="img" />
            </td>
            <td className="transfer-key" title={`${transfer.bucket} · ${transfer.region}${transfer.local ? ` · ${transfer.local}` : ""}`}><code>{transfer.key}</code></td>
            <td className="transfer-size" title={size === null ? undefined : exactBytes(size)}>{size === null ? "" : formatBytes(size)}</td>
            <td className="transfer-state"><StateIcon aria-hidden="true" />{label}</td>
            <td className="transfer-result">{result(transfer, session)}</td>
          </tr>;
        })}</tbody>
      </table> : <p className="transfer-empty">전송 없음</p>}
    </div>}
  </section>;
}
