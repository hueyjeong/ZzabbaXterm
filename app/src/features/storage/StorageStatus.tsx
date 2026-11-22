import type { Downloaded, S3Error, S3Session } from "./s3";

export type Problem = S3Error | "unavailable";
export type Transfer = Readonly<{ direction: "upload" | "download"; bucket: string; key: string }> & (
  | Readonly<{ state: "selecting" | "running" | "cancelled" | "denied" }>
  | Readonly<{ state: "failed"; problem: Problem }>
  | Readonly<{ state: "uploaded"; bytes: number }>
  | Readonly<{ state: "downloaded"; result: Downloaded }>
);

const stages = {
  profile: "SSO 프로필", listBuckets: "버킷 목록", listObjects: "객체 목록", uploadSource: "업로드 원본",
  putObject: "객체 업로드", getObject: "객체 받기", downloadBody: "다운로드 본문",
  downloadWrite: "다운로드 파일 쓰기", downloadCommit: "다운로드 파일 확정",
} satisfies Record<S3Error["stage"], string>;

const failures = {
  invalidSsoProfile: "유효하지 않은 SSO 프로필", invalidRegion: "유효하지 않은 리전", invalidBucket: "유효하지 않은 버킷",
  invalidResponse: "잘못된 서비스 응답", credentials: "SSO 자격 증명 오류", ssoLoginRequired: "같은 SSO 프로필로 다시 로그인 필요",
  request: "요청 실패", bodyRead: "본문 읽기 실패", checksum: "체크섬 검증 실패",
};

export function StorageProblem({ problem, session }: { problem: Problem; session?: S3Session }) {
  let detail = "네이티브 요청 결과 수신 실패";
  if (problem !== "unavailable") {
    const { failure, stage, bytes } = problem;
    let message: string;
    switch (failure.kind) {
      case "service": {
        const { status, code } = failure;
        const category = code === "ExpiredToken" || code === "ExpiredTokenException" ? "AWS 임시 자격 증명 만료" :
          code === "AccessDenied" || code === "AccessDeniedException" || (code === null && status === 403) ? "권한 거부" : "서비스 오류";
        message = `${category} (${status}${code ? ` ${code}` : ""})`;
        break;
      }
      case "localIo": message = `로컬 파일 오류 (${failure.errorKind})`; break;
      case "length": message = failure.expected === null ? "예상 길이 없음" : `길이 불일치 (예상 ${failure.expected} 바이트)`; break;
      default: message = failures[failure.kind];
    }
    detail = `${stages[stage]}: ${message} · ${bytes} 바이트`;
  }
  return <p role="alert">{detail}{session && ` · ${session.profile} · ${session.region} · ${session.endpoint}`}</p>;
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
    <p role="status">{status} · {transfer.bucket} / <code>{transfer.key}</code></p>
    {transfer.state === "failed" && <StorageProblem problem={transfer.problem} session={session} />}
  </div>;
}
