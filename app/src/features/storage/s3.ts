import type { ApprovedEditorFile, EditorTransport } from "../editor/contract";

export type S3Location = Readonly<{ bucket: string; prefix: string; region: string }>;

export type S3Session = Readonly<{
  sessionId: string;
  profile: string;
  region: string;
  endpoint: string;
  location?: S3Location;
}>;

export type BucketRow = Readonly<{ name: string; region: string }>;
export type ObjectRow =
  | Readonly<{ kind: "prefix"; prefix: string }>
  | Readonly<{ kind: "object"; key: string; bytes: number | null; rowId?: string }>;

export type StorageEditorOpen = Readonly<{
  file: ApprovedEditorFile;
  transport: EditorTransport;
  close(): Promise<void>;
}>;

type Page<T> = Readonly<{ rows: readonly T[]; nextToken: string | null }>;
type Stage = "profile" | "identity" | "listBuckets" | "headBucket" | "listObjects" | "uploadSource" | "putObject" |
  "getObject" | "downloadBody" | "downloadWrite" | "downloadCommit";
type Failure =
  | Readonly<{
    kind: "invalidSsoProfile" | "retestRequired" | "accountMismatch" | "invalidRegion" | "invalidBucket" | "invalidResponse" |
    "credentials" | "ssoLoginRequired" | "loginFailed" | "request" | "timedOut" | "bodyRead" | "checksum"
  }>
  | Readonly<{ kind: "bucketRegionMismatch"; region: string }>
  | Readonly<{ kind: "service"; status: number; code: string | null }>
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
