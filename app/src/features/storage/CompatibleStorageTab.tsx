import { useImperativeHandle, useRef, useState, type Ref } from "react";
import { invoke } from "@tauri-apps/api/core";
import { S3StoragePanel } from "./S3StoragePanel";
import { StorageProblem } from "./StorageStatus";
import type { S3Error } from "./s3";
import { closeStorage, storageActions } from "./storageIpc";
import type { CompatibleOpenRequest, CompatibleSavedSettings, CompatibleSession, CompatibleSettingsError, CompatibleSettingsInput } from "./compatible-settings";
import "./storage.css";

type Result<T, E = CompatibleSettingsError> = { ok: true; value: T } | { ok: false; error: E };
const empty: CompatibleSettingsInput = { serviceId: "", alias: "", endpoint: "", region: "", addressing: "path" };
const unavailable = "네이티브 요청 결과 수신 실패";
const errors = {
  invalid_endpoint: "유효하지 않은 HTTPS 출처", invalid_region: "유효하지 않은 서명 리전",
  invalid_service_id: "유효하지 않은 서비스 ID", invalid_item_id: "유효하지 않은 키링 항목 ID",
  invalid_alias: "유효하지 않은 별칭", invalid_file: "잘못된 호환 설정 파일",
  unsupported_version: "지원하지 않는 호환 설정 버전", not_found: "저장된 호환 설정 없음",
  io: "호환 설정 파일 오류", wrong_service: "저장된 서비스 또는 키링 연결이 다릅니다",
  cancelled: "키링 항목 사용 취소", key_missing: "지정한 키링 항목 없음", key_locked: "키링 잠김",
  key_unavailable: "키링을 사용할 수 없음", key_invalid: "잘못된 키링 항목",
  key_already_exists: "키링 항목이 이미 있습니다", key_outcome_unknown: "키링 등록 결과 확인 불가",
} satisfies Record<CompatibleSettingsError, string>;

type Attempt = { id: string; closing: boolean; close?: Promise<void> };

function origin(endpoint: string) {
  // 기존 참조 표시용 비교만 수행하며 출처 검증은 Rust에서 다시 한다.
  const match = /^https:\/\/(\[[0-9a-fA-F:.]+\]|[a-zA-Z0-9.-]+)(?::([0-9]+))?\/?$/.exec(endpoint);
  if (!match || match[0] !== endpoint) return;
  const port = Number(match[2] ?? 443);
  if (port < 1 || port > 65535) return;
  return `https://${match[1].toLowerCase()}${port === 443 ? "" : `:${port}`}`;
}

export function CompatibleStorageTab({ closing, closeRef }: {
  closing: boolean;
  closeRef: Ref<{ confirmClose(): Promise<void> }>;
}) {
  const [input, setInput] = useState(empty);
  const [saved, setSaved] = useState<CompatibleSavedSettings>();
  const [itemId, setItemId] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | { connection: S3Error; sessionId: string }>("");
  const [session, setSession] = useState<CompatibleSession>();
  const attempt = useRef<Attempt | null>(null);
  const reusable = saved && input.serviceId === saved.serviceId && origin(input.endpoint) === saved.origin ? saved : undefined;

  function close(current: Attempt) {
    current.closing = true;
    return current.close ??= closeStorage(current.id).then(() => {
      if (attempt.current === current) attempt.current = null;
    }, error => {
      current.close = undefined;
      throw error;
    });
  }

  useImperativeHandle(closeRef, () => ({
    async confirmClose() {
      if (attempt.current) await close(attempt.current);
    },
  }), []);

  function change(values: Partial<CompatibleSettingsInput>) {
    const next = { ...input, ...values };
    const nextOrigin = origin(next.endpoint);
    if (next.serviceId !== input.serviceId ||
      (next.endpoint !== input.endpoint && (!nextOrigin || nextOrigin !== origin(input.endpoint)))) {
      setItemId("");
    }
    setInput(next);
    setError("");
  }

  async function load() {
    if (closing || busy || attempt.current) return;
    setBusy(true);
    setError("");
    setSaved(undefined);
    setItemId("");
    try {
      const result = await invoke<Result<CompatibleSavedSettings>>("storage_compatible_load", { request: { serviceId: input.serviceId } });
      if (result.ok) {
        const { serviceId, alias, origin, region, addressing } = result.value;
        setInput({ serviceId, alias, endpoint: origin, region, addressing });
        setSaved(result.value);
      } else setError(errors[result.error]);
    } catch {
      setError(unavailable);
    } finally {
      setBusy(false);
    }
  }

  async function recover(current: Attempt) {
    setError(unavailable);
    try {
      await close(current);
    } catch {
      setError(`${unavailable} · 종료 확인 불가`);
    }
  }

  async function enroll() {
    if (closing || busy || attempt.current) return;
    const current: Attempt = { id: crypto.randomUUID(), closing: false };
    attempt.current = current;
    setBusy(true);
    setError("");
    try {
      const result = await invoke<Result<{ itemId: string }, CompatibleSettingsError | { outcomeUnknown: { itemId: string } }>>(
        "storage_compatible_enroll", { request: { sessionId: current.id, settings: input } },
      );
      if (current.closing) return;
      if (attempt.current === current) attempt.current = null;
      if (result.ok) setItemId(result.value.itemId);
      else if (typeof result.error === "string") setError(errors[result.error]);
      else {
        setItemId(result.error.outcomeUnknown.itemId);
        setError(errors.key_outcome_unknown);
      }
    } catch {
      if (!current.closing) await recover(current);
    } finally {
      setBusy(false);
    }
  }

  async function open() {
    if (closing || busy || attempt.current) return;
    const target: CompatibleOpenRequest = !reusable ? { kind: "select", settings: input, itemId } :
      input.alias === reusable.alias && input.endpoint === reusable.origin && input.region === reusable.region && input.addressing === reusable.addressing
        ? { kind: "saved", expected: reusable }
        : { kind: "revise", expected: reusable, settings: input };
    const current: Attempt = { id: crypto.randomUUID(), closing: false };
    attempt.current = current;
    setBusy(true);
    setError("");
    try {
      const result = await invoke<Result<CompatibleSession, CompatibleSettingsError | { connection: S3Error }>>(
        "storage_compatible_open", { request: { sessionId: current.id, target } },
      );
      if (current.closing) return;
      if (!result.ok) {
        attempt.current = null;
        setError(typeof result.error === "string" ? errors[result.error] : { ...result.error, sessionId: current.id });
      } else {
        if (result.value.sessionId !== current.id) throw new Error(unavailable);
        setSession(result.value);
        setItemId("");
      }
    } catch {
      if (!current.closing) await recover(current);
    } finally {
      setBusy(false);
    }
  }

  if (session) {
    const { target } = session;
    return <div inert={closing}><S3StoragePanel initialBuckets={session.initialBuckets} actions={storageActions} session={{
      sessionId: session.sessionId, profile: target.alias, region: target.region, endpoint: target.origin,
      compatible: { serviceId: target.serviceId, addressing: target.addressing },
    }} /></div>;
  }

  return <section className="s3-storage" aria-label="S3 호환 HTTPS">
    <header className="storage-heading"><h2>S3 호환 HTTPS</h2></header>
    <form onSubmit={event => { event.preventDefault(); void open(); }}>
      <fieldset className="storage-controls" disabled={closing || busy || !!attempt.current}>
        <label>서비스 ID<input required value={input.serviceId} onChange={event => change({ serviceId: event.target.value })} /></label>
        <button type="button" disabled={!input.serviceId} onClick={() => void load()}>저장된 설정 불러오기</button>
        <label>별칭<input required value={input.alias} onChange={event => change({ alias: event.target.value })} /></label>
        <label>HTTPS 출처<input required type="url" value={input.endpoint} onChange={event => change({ endpoint: event.target.value })} /></label>
        <label>서명 리전<input required value={input.region} onChange={event => change({ region: event.target.value })} /></label>
        <label>주소 방식<select value={input.addressing} onChange={event => change({ addressing: event.target.value === "path" ? "path" : "virtual_hosted" })}>
          <option value="path">경로</option><option value="virtual_hosted">가상 호스트</option>
        </select></label>
        {!reusable && <>
          <label>키링 항목 ID<input required value={itemId} onChange={event => setItemId(event.target.value)} /></label>
          <button type="button" disabled={!input.serviceId || !input.alias || !input.endpoint || !input.region} onClick={() => void enroll()}>키 등록</button>
        </>}
        <button type="submit">{reusable ? "호환 스토리지 연결" : "설정 저장 및 연결"}</button>
      </fieldset>
    </form>
    {busy && <p role="status">호환 스토리지 요청 중</p>}
    {typeof error === "string" ? error && <p role="alert">{error}</p> :
      <StorageProblem problem={error.connection} session={{ sessionId: error.sessionId, profile: input.alias, region: input.region, endpoint: input.endpoint,
        compatible: { serviceId: input.serviceId, addressing: input.addressing } }} />}
  </section>;
}
