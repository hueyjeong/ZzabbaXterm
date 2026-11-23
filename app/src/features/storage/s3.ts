import type { ApprovedEditorFile, EditorTransport } from "../editor/contract";
import type { CompatibleAddressing } from "./compatible-settings";

export type S3Location = Readonly<{ bucket: string; prefix: string; region: string }>;

export type S3Session = Readonly<{
  sessionId: string;
  profile: string;
  region: string;
  endpoint: string;
  location?: S3Location;
  compatible?: Readonly<{ serviceId: string; addressing: CompatibleAddressing }>;
}>;

export type BucketRow = Readonly<{ name: string; region: string }>;
export type ObjectRow =
  | Readonly<{ kind: "prefix"; prefix: string }>
  | Readonly<{ kind: "object"; key: string; bytes: number | null; lastModified?: string; storageClass?: string; rowId?: string }>;

export type StorageEditorOpen = Readonly<{
  file: ApprovedEditorFile;
  transport: EditorTransport;
  close(): Promise<void>;
}>;

type Page<T> = Readonly<{ rows: readonly T[]; nextToken: string | null }>;
type Stage = "profile" | "identity" | "listBuckets" | "headBucket" | "listObjects" | "uploadSource" | "headObject" | "putObject" |
  "getObject" | "downloadBody" | "downloadWrite" | "downloadCommit";
type Failure =
  | Readonly<{
    kind: "invalidSsoProfile" | "retestRequired" | "accountMismatch" | "invalidRegion" | "invalidBucket" | "invalidResponse" |
    "credentials" | "ssoLoginRequired" | "loginFailed" | "request" | "timedOut" | "bodyRead" | "checksum" | "destination" | "tls"
  }>
  | Readonly<{ kind: "bucketRegionMismatch"; region: string }>
  | Readonly<{ kind: "service"; status: number; code: string | null }>
  | Readonly<{ kind: "redirect"; status: number; code: string | null }>
  | Readonly<{ kind: "signingRegion"; status: number; code: string }>
  | Readonly<{ kind: "localIo"; errorKind: string }>
  | Readonly<{ kind: "recoveryUnconfirmed"; recoveryPath: string }>
  | Readonly<{ kind: "length"; expected: number | null }>;

export type S3Error = Readonly<{ stage: Stage; failure: Failure; bytes: number }>;
export type S3Result<T> = Readonly<{ ok: true; value: T }> | Readonly<{ ok: false; error: S3Error }>;
export type Downloaded = Readonly<{ bytes: number; integrity: "sdkChecksumVerified" | "lengthOnly" }>;

type ObjectTarget = Readonly<{ sessionId: string; bucket: string; region: string; key: string }>;
export type S3FileSelection = ObjectTarget & Readonly<{ direction: "upload" | "download" }>;
export type LocalFileApproval = Readonly<{ kind: "approved"; approvalId: string }> |
  Readonly<{ kind: "cancelled" | "denied" }>;
type ApprovedTransfer = ObjectTarget & Readonly<{ approvalId: string }>;
type BucketPageRequest = Readonly<{ sessionId: string; token: string | null }>;

// 세션과 파일 승인 ID의 발급 및 검증은 네이티브 경계에서 수행한다.
export type S3Actions = Readonly<{
  listBuckets(request: BucketPageRequest): Promise<S3Result<Page<BucketRow>>>;
  listObjects(request: BucketPageRequest & Readonly<{ bucket: string; prefix: string; region: string }>): Promise<S3Result<Page<ObjectRow>>>;
  selectLocalFile(request: S3FileSelection): Promise<LocalFileApproval>;
  upload(request: ApprovedTransfer): Promise<S3Result<number>>;
  download(request: ApprovedTransfer): Promise<S3Result<Downloaded>>;
}>;

// 로컬 창 경로는 네이티브 폴더 대화상자로 승인한 루트 기준 상대 경로("a/b")다. 빈 문자열이 루트다.
export type LocalRoot = Readonly<{ rootId: string; label: string }>;
export type LocalRootApproval = (Readonly<{ kind: "approved" }> & LocalRoot) | Readonly<{ kind: "cancelled" | "denied" | "restricted" }>;
export type LocalEntry = Readonly<{ name: string; kind: "folder" | "file" | "link"; bytes: number | null; modified: number | null }>;
export type LocalListing = Readonly<{ entries: readonly LocalEntry[]; truncated: boolean }>;
export type LocalFailure =
  | Readonly<{ kind: "denied" | "invalidPath" | "link" | "notFound" | "rootChanged" }>
  | Readonly<{ kind: "localIo"; errorKind: string }>;
export type LocalResult<T> = Readonly<{ ok: true; value: T }> | Readonly<{ ok: false; error: LocalFailure }>;
export type LocalSelection = ObjectTarget & Readonly<{ rootId: string; path: string; direction: "upload" | "download" }>;
/** 로컬 창 전송 승인 결과다. rejected는 로컬 경로 원인, failed는 S3 존재 확인 오류다. */
export type LocalSelectResult = LocalFileApproval | Readonly<{ kind: "rejected"; reason: LocalFailure }> | Readonly<{ kind: "failed"; error: S3Error }>;

export type LocalFileActions = Readonly<{
  openRoot(request: Readonly<{ sessionId: string }>): Promise<LocalRootApproval>;
  list(request: Readonly<{ sessionId: string; rootId: string; path: string }>): Promise<LocalResult<LocalListing>>;
  /** 업로드는 path가 파일, 다운로드는 path가 폴더이고 이름은 키의 마지막 구간이다. */
  select(request: LocalSelection): Promise<LocalSelectResult>;
}>;
