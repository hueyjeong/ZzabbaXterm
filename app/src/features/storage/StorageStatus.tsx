import type { Downloaded, S3Error, S3Session } from "./s3";

export type Problem = S3Error | "unavailable";
export type Transfer = Readonly<{ direction: "upload" | "download"; bucket: string; region: string; key: string }> & (
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

export function StorageProblem({ problem, session }: { problem: Problem; session?: S3Session }) {
  let detail = "네이티브 요청 결과 수신 실패";
  let recoveryPath: string | undefined;
  if (problem !== "unavailable") {
    const { failure, stage, bytes } = problem;
    let message: string;
    switch (failure.kind) {
      case "redirect": message = `리디렉션 차단 (${failure.status}${failure.code ? ` ${failure.code}` : ""})`; break;
      case "signingRegion": message = `서명 리전 오류 (${failure.status} ${failure.code})`; break;
      case "service": {
        const { status, code } = failure;
        const expired = code === "ExpiredToken" || code === "ExpiredTokenException";
        const category = expired ? (session?.compatible ? "호환 서비스 자격 증명 만료" : "AWS 임시 자격 증명 만료") :
          code === "AccessDenied" || code === "AccessDeniedException" || (code === null && status === 403) ? "권한 거부" : "서비스 오류";
        message = `${category} (${status}${code ? ` ${code}` : ""})`;
        break;
      }
      case "credentials": message = session?.compatible ? "호환 서비스 자격 증명 오류" : failures.credentials; break;
      case "localIo": message = `로컬 파일 오류 (${failure.errorKind})`; break;
      case "recoveryUnconfirmed":
        message = "복구 확인 실패";
        recoveryPath = failure.recoveryPath;
        break;
      case "length": message = failure.expected === null ? "예상 길이 없음" : `길이 불일치 (예상 ${failure.expected} 바이트)`; break;
      case "bucketRegionMismatch": message = `버킷 리전: ${failure.region}`; break;
      default: message = failures[failure.kind];
    }
    detail = `${stages[stage]}: ${message} · ${bytes} 바이트`;
  }
  return <p role="alert">{detail}{recoveryPath !== undefined && <> · 보존 파일: <code>{recoveryPath}</code></>}{session && ` · ${session.profile} · ${session.region} · ${session.endpoint}`}</p>;
}

export function TransferStatus({ transfer, session }: { transfer: Transfer; session: S3Session }) {
  const action = transfer.direction === "upload" ? "업로드" : "다운로드";
  let status: string;
  switch (transfer.state) {
    case "selecting": status = `${action} 파일 선택 대기`; break;
    case "running": status = `${action} 진행 중`; break;
    case "cancelled": status = `${action} 파일 선택 취소`; break;
    case "denied": status = `${action} 파일 승인 거부`; break;
    case "failed": status = `${action} 미완료`; break;
    case "uploaded": status = `업로드 완료: ${transfer.bytes} 바이트`; break;
    case "downloaded":
      status = `다운로드 완료: ${transfer.result.bytes} 바이트 · ${transfer.result.integrity === "sdkChecksumVerified" ? "SDK 체크섬 검증됨" : "길이만 확인"}`;
      break;
  }
  return <div className="storage-transfer">
    <p role="status">{status} · {transfer.bucket} / <code>{transfer.key}</code> · {transfer.region}</p>
    {transfer.state === "failed" && <StorageProblem problem={transfer.problem} session={{ ...session, region: transfer.region }} />}
  </div>;
}
