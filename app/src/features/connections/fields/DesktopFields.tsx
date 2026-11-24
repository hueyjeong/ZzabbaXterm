import type { ReactNode } from "react";
import type { SavedTarget } from "../connectionIpc";
import { desktopHostUsed, type DesktopInput, type DesktopKind } from "../profiles";
import { Field, type FieldProps } from "./Field";

type Props = FieldProps & Readonly<{
  kind: DesktopKind;
  desktop: DesktopInput;
  edit(change: Partial<DesktopInput>, field?: string): void;
  /** "SSH 세션" 후보다. 직접 전송이고 Agent 또는 키 파일 인증인 저장된 SSH 세션만 받는다. */
  sshTargets: readonly SavedTarget[];
  /** 연결 경로가 AWS SSM 경유일 때 경로 아래에 놓을 노드 선택이다. */
  ssm?: ReactNode;
}>;

export const routeLabels = { direct: "직접 연결", ssh: "SSH 터널 경유", ssm: "AWS SSM 경유" } as const;

export function DesktopFields({ kind, desktop, edit, sshTargets, ssm, error, touch }: Props) {
  const { route } = desktop;
  const routeTarget = route.kind === "ssh" ? route.targetId : "";
  return <>
    <Field label="연결 경로">
      <select value={route.kind} onChange={event => {
        const next = event.currentTarget.value;
        // VNC 터널 경로는 터널 끝 호스트의 서비스만 열어 호스트를 비운다.
        const host = desktopHostUsed(kind, next as DesktopInput["route"]["kind"]) ? desktop.host : "";
        if (next === "direct") edit({ host, route: { kind: next } });
        if (next === "ssh") edit({ host, route: { kind: next, targetId: sshTargets[0]?.id ?? "" } });
        if (next === "ssm") edit({ host, route: { kind: next } });
      }}>
        {Object.entries(routeLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}
      </select>
    </Field>
    {route.kind === "ssh" && <Field label="SSH 세션" error={error("desktop.route")}>
      <select value={routeTarget} onBlur={() => touch("desktop.route")}
        onChange={event => edit({ route: { kind: "ssh", targetId: event.currentTarget.value } }, "desktop.route")}>
        <option value="">{sshTargets.length ? "SSH 세션 선택" : "저장된 SSH 세션 없음"}</option>
        {sshTargets.map(profile => <option key={profile.id} value={profile.id}>{profile.displayName}</option>)}
        {routeTarget && !sshTargets.some(profile => profile.id === routeTarget) && <option value={routeTarget}>{routeTarget}</option>}
      </select>
    </Field>}
    {route.kind === "ssm" && ssm}
    {route.kind !== "ssh" && error("desktop.route") && <div className="field-error">{error("desktop.route")}</div>}
    {desktopHostUsed(kind, route.kind) && <Field label="호스트" error={error("desktop.host")}>
      <input value={desktop.host} maxLength={253} autoComplete="off" spellCheck={false} onBlur={() => touch("desktop.host")}
        onChange={event => edit({ host: event.target.value }, "desktop.host")} />
    </Field>}
    <Field label="포트" error={error("desktop.port")}>
      <input inputMode="numeric" value={desktop.port} onBlur={() => touch("desktop.port")}
        onChange={event => edit({ port: event.target.value }, "desktop.port")} />
    </Field>
    {kind === "rdp" && <label className="editor-check"><input type="checkbox" checked={desktop.tlsOnly}
      onChange={event => edit({ tlsOnly: event.target.checked })} /> TLS 전용 허용</label>}
  </>;
}
