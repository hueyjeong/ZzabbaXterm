import { useImperativeHandle, useRef, useState, type Ref } from "react";
import { invoke } from "@tauri-apps/api/core";
import { S3StoragePanel } from "./S3StoragePanel";
import { closeStorage, storageActions } from "./storageIpc";
import type { CompatibleOpenRequest, CompatibleSession, CompatibleSettingsError, CompatibleSettingsInput, CompatibleSettingsView } from "./compatible-settings";
import "./storage.css";

type Result<T> = { ok: true; value: T } | { ok: false; error: CompatibleSettingsError };
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
} satisfies Record<CompatibleSettingsError, string>;

type Attempt = { id: string; closing: boolean; close?: Promise<void> };

export function CompatibleStorageTab({ closing, closeRef }: {
  closing: boolean;
  closeRef: Ref<{ confirmClose(): Promise<void> }>;
}) {
  const [input, setInput] = useState(empty);
  const [saved, setSaved] = useState<CompatibleSettingsView>();
  const [itemId, setItemId] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [session, setSession] = useState<CompatibleSession>();
  const attempt = useRef<Attempt | null>(null);

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
    if (values.endpoint !== undefined || values.serviceId !== undefined) setItemId("");
    setSaved(undefined);
    setInput({ ...input, ...values });
    setError("");
  }

  async function load() {
    if (closing || busy || attempt.current) return;
    setBusy(true);
    setError("");
    setSaved(undefined);
    setItemId("");
    try {
      const result = await invoke<Result<CompatibleSettingsView>>("storage_compatible_load", { request: { serviceId: input.serviceId } });
      if (result.ok) {
        const { origin, ...fields } = result.value;
        setInput({ ...fields, endpoint: origin });
        setSaved(result.value);
      } else setError(errors[result.error]);
    } catch {
      setError(unavailable);
    } finally {
      setBusy(false);
    }
  }

  async function open() {
    if (closing || busy || attempt.current) return;
    const target: CompatibleOpenRequest = saved ? { kind: "saved", expected: saved } : { kind: "select", settings: input, itemId };
    const current: Attempt = { id: crypto.randomUUID(), closing: false };
    attempt.current = current;
    setBusy(true);
    setError("");
    try {
      const result = await invoke<Result<CompatibleSession>>("storage_compatible_open", { request: { sessionId: current.id, target } });
      if (current.closing) return;
      if (!result.ok) {
        attempt.current = null;
        setError(errors[result.error]);
      } else {
        if (result.value.sessionId !== current.id) throw new Error(unavailable);
        setSession(result.value);
        setItemId("");
      }
    } catch {
      if (current.closing) return;
      setError(unavailable);
      try {
        await close(current);
      } catch {
        setError(`${unavailable} · 종료 확인 불가`);
      }
    } finally {
      setBusy(false);
    }
  }

  if (session) {
    const { target } = session;
    return <div inert={closing}><S3StoragePanel actions={storageActions} session={{
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
        {!saved && <label>키링 항목 ID<input required value={itemId} onChange={event => setItemId(event.target.value)} /></label>}
        <button type="submit">{saved ? "호환 스토리지 연결" : "설정 저장 및 연결"}</button>
      </fieldset>
    </form>
    {busy && <p role="status">호환 스토리지 요청 중</p>}
    {error && <p role="alert">{error}</p>}
  </section>;
}
