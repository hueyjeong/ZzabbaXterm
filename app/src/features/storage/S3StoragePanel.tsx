import { useState } from "react";
import { ObjectTable } from "./ObjectTable";
import type { ObjectRow, S3Actions, S3FileSelection, S3Session } from "./s3";
import { StorageProblem, TransferStatus, type Problem, type Transfer } from "./StorageStatus";
import "./storage.css";

type Location = { bucket: string; prefix: string };
type Listing = { nextToken: string | null } & (
  | { kind: "buckets"; rows: readonly string[] }
  | { kind: "objects"; location: Location; rows: readonly ObjectRow[] }
);
type View = {
  bucket: string;
  prefix: string;
  listing?: Listing;
  loading: boolean;
  problem?: Problem;
  selectedKey: string | null;
  uploadKey: string;
  transfer?: Transfer;
};
const initial: View = { bucket: "", prefix: "", loading: false, selectedKey: null, uploadKey: "" };

type Props = Readonly<{ session: S3Session; actions: S3Actions }>;

export function S3StoragePanel({ session, actions }: Props) {
  const [views, setViews] = useState(() => new Map<string, View>());
  const { sessionId } = session;
  const view = views.get(sessionId) ?? initial;

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
          update(() => ({ loading: false, problem: result.error }));
          return;
        }
        const previous = view.listing;
        const rows = token !== null && previous?.kind === "objects" ? [...previous.rows, ...result.value.rows] : result.value.rows;
        listing = { kind: "objects", location, rows, nextToken: result.value.nextToken };
      } else {
        const result = await actions.listBuckets({ sessionId, token });
        if (!result.ok) {
          update(() => ({ loading: false, problem: result.error }));
          return;
        }
        const previous = view.listing;
        const rows = token !== null && previous?.kind === "buckets" ? [...previous.rows, ...result.value.rows] : result.value.rows;
        listing = { kind: "buckets", rows, nextToken: result.value.nextToken };
      }
      update(current => {
        const previous = current.listing;
        const changed = listing.kind === "objects" && (previous?.kind !== "objects" ||
          previous.location.bucket !== listing.location.bucket || previous.location.prefix !== listing.location.prefix);
        return {
          loading: false,
          listing,
          ...(changed ? { selectedKey: null } : {}),
        };
      });
    } catch {
      update(() => ({ loading: false, problem: "unavailable" }));
    }
  }

  function open(location: Location) {
    update(() => location);
    void load(location);
  }

  const listing = view.listing;
  const location = listing?.kind === "objects" ? listing.location : null;
  const transferring = view.transfer?.state === "selecting" || view.transfer?.state === "running";
  const selectedKey = listing?.kind === "objects" && listing.rows.some(row => row.kind === "object" && row.key === view.selectedKey)
    ? view.selectedKey : null;

  async function transfer(direction: S3FileSelection["direction"], key: string | null) {
    if (!location || key === null || key === "" || transferring) return;
    const target = { sessionId, bucket: location.bucket, key };
    const display = { direction, bucket: location.bucket, key };
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
    <form className="storage-controls" onSubmit={event => { event.preventDefault(); if (view.bucket) open({ bucket: view.bucket, prefix: view.prefix }); }}>
      <label>버킷<input value={view.bucket} onChange={event => {
        const bucket = event.target.value;
        update(() => ({ bucket }));
      }} /></label>
      <label>접두어<input value={view.prefix} onChange={event => {
        const prefix = event.target.value;
        update(() => ({ prefix }));
      }} /></label>
      <button type="submit" disabled={!view.bucket || view.loading}>버킷 열기</button>
      <button type="button" disabled={view.loading} onClick={() => void load(null)}>버킷 목록</button>
    </form>
    {view.problem && <StorageProblem problem={view.problem} session={session} />}
    {view.loading && <p role="status">목록 요청 중</p>}
    {listing?.kind === "buckets" && <ul className="storage-buckets" aria-label="버킷 목록">
      {listing.rows.map(bucket => <li key={bucket}>
        <button type="button" disabled={view.loading} onClick={() => open({ bucket, prefix: "" })}>{bucket}</button>
      </li>)}
    </ul>}
    {listing?.kind === "objects" && <>
      <div className="storage-location"><strong>{listing.location.bucket}</strong> / <code>{listing.location.prefix}</code></div>
      <ObjectTable rows={listing.rows} selectedKey={selectedKey} onOpenPrefix={prefix => {
        if (!view.loading) open({ bucket: listing.location.bucket, prefix });
      }} onSelectObject={key => update(() => ({ selectedKey: key }))} />
    </>}
    {listing?.rows.length === 0 && <p>목록이 비어 있습니다.</p>}
    {listing && listing.nextToken !== null && <button type="button" disabled={view.loading} onClick={() => void load(location, listing.nextToken)}>다음 페이지</button>}
    {location && <form className="storage-controls" onSubmit={event => { event.preventDefault(); void transfer("upload", view.uploadKey); }}>
      <label>업로드 전체 키<input value={view.uploadKey} onChange={event => {
        const uploadKey = event.target.value;
        update(() => ({ uploadKey }));
      }} /></label>
      <button type="submit" disabled={!view.uploadKey || transferring}>업로드</button>
      <button type="button" disabled={selectedKey === null || transferring} onClick={() => void transfer("download", selectedKey)}>다운로드</button>
    </form>}
    {view.transfer && <TransferStatus session={session} transfer={view.transfer} />}
  </section>;
}
