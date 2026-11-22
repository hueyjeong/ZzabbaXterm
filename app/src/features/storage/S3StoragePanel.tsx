import { useState } from "react";
import { ObjectTable } from "./ObjectTable";
import type { BucketRow, ObjectRow, S3Actions, S3FileSelection, S3Session } from "./s3";
import { StorageProblem, TransferStatus, type Problem, type Transfer } from "./StorageStatus";
import "./storage.css";

type Location = { bucket: string; prefix: string; region: string };
type Listing = { nextToken: string | null } & (
  | { kind: "buckets"; rows: readonly BucketRow[] }
  | { kind: "objects"; location: Location; rows: readonly ObjectRow[] }
);
type View = {
  input?: Location;
  confirmed: boolean;
  listing?: Listing;
  loading: boolean;
  problem?: Problem;
  selectedKey: string | null;
  uploadKey: string;
  transfer?: Transfer;
  invalidUploadPaste?: boolean;
  expired?: boolean;
};
const initial: View = { confirmed: false, loading: false, selectedKey: null, uploadKey: "" };

type Props = Readonly<{ session: S3Session; actions: S3Actions; onRelogin?: () => void }>;

export function S3StoragePanel({ session, actions, onRelogin }: Props) {
  const [views, setViews] = useState(() => new Map<string, View>());
  const { sessionId } = session;
  const view = views.get(sessionId) ?? initial;
  const input = view.input ?? { bucket: "", prefix: "", region: session.region };

  function update(change: (current: View) => Partial<View>) {
    setViews(current => {
      const previous = current.get(sessionId) ?? initial;
      return new Map(current).set(sessionId, { ...previous, ...change(previous) });
    });
  }

  async function load(location: Location | null, token: string | null = null) {
    if (view.loading) return;
    update(() => ({ loading: true, problem: undefined }));
    try {
      let listing: Listing;
      if (location) {
        const result = await actions.listObjects({ sessionId, ...location, token });
        if (!result.ok) {
          update(() => ({ loading: false, problem: result.error, ...(result.error.failure.kind === "ssoLoginRequired" ? { expired: true } : {}) }));
          return;
        }
        const previous = view.listing;
        const rows = token !== null && previous?.kind === "objects" ? [...previous.rows, ...result.value.rows] : result.value.rows;
        listing = { kind: "objects", location, rows, nextToken: result.value.nextToken };
      } else {
        const result = await actions.listBuckets({ sessionId, token });
        if (!result.ok) {
          update(() => ({ loading: false, problem: result.error, ...(result.error.failure.kind === "ssoLoginRequired" ? { expired: true } : {}) }));
          return;
        }
        const previous = view.listing;
        const rows = token !== null && previous?.kind === "buckets" ? [...previous.rows, ...result.value.rows] : result.value.rows;
        listing = { kind: "buckets", rows, nextToken: result.value.nextToken };
      }
      update(current => {
        const previous = current.listing;
        const changed = listing.kind === "objects" && (previous?.kind !== "objects" ||
          previous.location.bucket !== listing.location.bucket || previous.location.prefix !== listing.location.prefix ||
          previous.location.region !== listing.location.region);
        return {
          loading: false,
          listing,
          ...(token === null ? { confirmed: listing.kind === "objects" } : {}),
          ...(changed ? { selectedKey: null } : {}),
        };
      });
    } catch {
      update(() => ({ loading: false, problem: "unavailable" }));
    }
  }

  function open(location: Location) {
    update(() => ({ input: location, confirmed: false }));
    void load(location);
  }

  const listing = view.listing;
  const location = listing?.kind === "objects" ? listing.location : null;
  const transferring = view.transfer?.state === "selecting" || view.transfer?.state === "running";
  const expired = view.expired || (view.transfer?.state === "failed" && view.transfer.problem !== "unavailable" && view.transfer.problem.failure.kind === "ssoLoginRequired");
  const detectedRegion = view.problem && view.problem !== "unavailable" && view.problem.failure.kind === "bucketRegionMismatch" ? view.problem.failure.region : null;
  const canTransfer = view.confirmed && !view.loading && !transferring && !expired;
  const selectedKey = listing?.kind === "objects" && listing.rows.some(row => row.kind === "object" && row.key === view.selectedKey)
    ? view.selectedKey : null;

  async function transfer(direction: S3FileSelection["direction"], key: string | null) {
    if (!canTransfer || !location || key === null || key === "") return;
    const target = { sessionId, bucket: location.bucket, region: location.region, key };
    const display = { direction, bucket: location.bucket, region: location.region, key };
    update(() => ({ transfer: { ...display, state: "selecting" } }));
    try {
      const selection = await actions.selectLocalFile({ ...target, direction });
      if (selection.kind !== "approved") {
        update(() => ({ transfer: { ...display, state: selection.kind } }));
        return;
      }
      const request = { ...target, approvalId: selection.approvalId };
      update(() => ({ transfer: { ...display, state: "running" } }));
      if (direction === "upload") {
        const result = await actions.upload(request);
        update(() => ({
          transfer: result.ok
            ? { ...display, state: "uploaded", bytes: result.value }
            : { ...display, state: "failed", problem: result.error }
        }));
      } else {
        const result = await actions.download(request);
        update(() => ({
          transfer: result.ok
            ? { ...display, state: "downloaded", result: result.value }
            : { ...display, state: "failed", problem: result.error }
        }));
      }
    } catch {
      update(() => ({ transfer: { ...display, state: "failed", problem: "unavailable" } }));
    }
  }

  return <section className="s3-storage" aria-label="AWS S3">
    <header className="storage-heading">
      <h2>AWS S3</h2><span>{session.profile} · {session.region}</span>
    </header>
    <form className="storage-controls" onSubmit={event => { event.preventDefault(); if (input.bucket && input.region && !view.loading && !transferring) open(input); }}>
      {([["버킷", "bucket"], ["접두어", "prefix"], ["리전", "region"]] as const).map(([label, field]) =>
        <label key={field}>{label}<input value={input[field]} disabled={view.loading || transferring} onChange={event => {
          const value = event.target.value;
          update(() => ({ input: { ...input, [field]: value }, confirmed: false }));
        }} /></label>)}
      <button type="submit" disabled={!input.bucket || !input.region || view.loading || transferring}>버킷 열기</button>
      <button type="button" disabled={view.loading || transferring} onClick={() => void load(null)}>버킷 목록</button>
    </form>
    {view.problem && <StorageProblem problem={view.problem} session={session} />}
    {detectedRegion && <button type="button" disabled={view.loading || transferring} onClick={() => {
      update(() => ({ input: { ...input, region: detectedRegion }, confirmed: false, problem: undefined }));
    }}>{detectedRegion} 선택</button>}
    {view.loading && <p role="status">목록 요청 중</p>}
    {listing?.kind === "buckets" && <ul className="storage-buckets" aria-label="버킷 목록">
      {listing.rows.map(({ name, region }) => <li key={name}>
        <button type="button" disabled={view.loading || transferring} onClick={() => open({ bucket: name, prefix: "", region })}>{name}</button> <span>{region}</span>
      </li>)}
    </ul>}
    {listing?.kind === "objects" && <>
      <div className="storage-location"><strong>{listing.location.bucket}</strong> / <code>{listing.location.prefix}</code> · {listing.location.region}</div>
      <ObjectTable rows={listing.rows} selectedKey={selectedKey} onOpenPrefix={prefix => {
        if (!view.loading && !transferring) open({ ...listing.location, prefix });
      }} onSelectObject={key => update(() => ({ selectedKey: key }))} />
    </>}
    {listing?.rows.length === 0 && <p>목록이 비어 있습니다.</p>}
    {listing && listing.nextToken !== null && <button type="button" disabled={view.loading || transferring || (!!location && !view.confirmed)} onClick={() => void load(location, listing.nextToken)}>다음 페이지</button>}
    {location && <form className="storage-controls" onSubmit={event => { event.preventDefault(); void transfer("upload", view.uploadKey); }}>
      <label>업로드 전체 키<input value={view.uploadKey} onChange={event => {
        const uploadKey = event.target.value;
        update(() => ({ uploadKey, invalidUploadPaste: false }));
      }} onPaste={event => {
        if (!/[\r\n]/.test(event.clipboardData.getData("text/plain"))) return;
        event.preventDefault();
        update(() => ({ invalidUploadPaste: true }));
      }} /></label>
      <button type="submit" disabled={!view.uploadKey || !canTransfer}>업로드</button>
      <button type="button" disabled={selectedKey === null || !canTransfer} onClick={() => void transfer("download", selectedKey)}>다운로드</button>
    </form>}
    {view.invalidUploadPaste && <p role="alert">줄바꿈이 포함된 키는 붙여넣을 수 없습니다</p>}
    {view.transfer && <TransferStatus session={session} transfer={view.transfer} />}
    {expired && onRelogin && <button type="button" disabled={view.loading || transferring} onClick={onRelogin}>SSO 재로그인</button>}
  </section>;
}
