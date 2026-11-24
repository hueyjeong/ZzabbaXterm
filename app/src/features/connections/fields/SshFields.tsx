import type { ReactNode } from "react";
import type { SshAuth, SshInput } from "../profiles";
import { Field, type FieldProps } from "./Field";

type Props = FieldProps & Readonly<{
  ssh: SshInput;
  edit(change: Partial<SshInput>, field?: string): void;
  storedAuth: boolean;
  /** 연결 경로가 AWS SSM 경유일 때 경로 아래에 놓을 노드 선택이다. */
  ssm?: ReactNode;
}>;

const authLabels = {
  agent: "SSH Agent",
  keyFile: "키 파일",
  manualPassword: "직접 입력",
  storedPassword: "저장된 인증 사용",
} satisfies Record<SshAuth["kind"], string>;

export function SshFields({ ssh, edit, storedAuth, ssm, error, touch }: Props) {
  const { auth } = ssh;
  const text = (field: keyof SshInput & ("host" | "port" | "account" | "hostKeyFingerprint"), label: string, numeric?: true) =>
    <Field label={label} error={error(`ssh.${field}`)}>
      <input value={ssh[field]} inputMode={numeric && "numeric"} spellCheck={false}
        onChange={event => edit({ [field]: event.target.value }, `ssh.${field}`)} onBlur={() => touch(`ssh.${field}`)} />
    </Field>;
  return <>
    {text("host", "호스트")}
    {text("port", "포트", true)}
    {text("account", "계정")}
    <Field label="연결 경로">
      <select value={ssh.transport.kind} onChange={event => {
        edit({ transport: event.currentTarget.value === "ssm" ? { kind: "ssm" } : { kind: "direct" } });
      }}>
        <option value="direct">직접 연결</option>
        <option value="ssm">AWS SSM 경유</option>
      </select>
    </Field>
    {ssh.transport.kind === "ssm" && ssm}
    <Field label="인증 방식" error={error("ssh.auth")}>
      <select value={auth?.kind ?? ""} onBlur={() => touch("ssh.auth")} onChange={event => {
        switch (event.currentTarget.value) {
          case "agent": edit({ auth: { kind: "agent", publicKeyPath: "" } }, "ssh.auth"); break;
          case "keyFile": edit({ auth: { kind: "keyFile", privateKeyPath: "" } }, "ssh.auth"); break;
          case "manualPassword": edit({ auth: { kind: "manualPassword" } }, "ssh.auth"); break;
          case "storedPassword": edit({ auth: { kind: "storedPassword", itemId: "" } }, "ssh.auth"); break;
          default: edit({ auth: null }, "ssh.auth");
        }
      }}>
        <option value="">인증 방식 선택</option>
        {Object.entries(authLabels).map(([value, label]) => <option key={value} value={value} disabled={value === "storedPassword" && !storedAuth}>{label}</option>)}
      </select>
    </Field>
    {auth?.kind === "agent" && <Field label="Agent 공개 키 파일 경로" error={error("ssh.auth.keyPath")}>
      <input value={auth.publicKeyPath} spellCheck={false} onBlur={() => touch("ssh.auth.keyPath")}
        onChange={event => edit({ auth: { kind: "agent", publicKeyPath: event.target.value } }, "ssh.auth.keyPath")} />
    </Field>}
    {auth?.kind === "keyFile" && <Field label="개인 키 파일 경로" error={error("ssh.auth.keyPath")}>
      <input value={auth.privateKeyPath} spellCheck={false} onBlur={() => touch("ssh.auth.keyPath")}
        onChange={event => edit({ auth: { kind: "keyFile", privateKeyPath: event.target.value } }, "ssh.auth.keyPath")} />
    </Field>}
    {auth?.kind === "storedPassword" && <Field label="저장 암호 항목 ID" error={error("ssh.auth.itemId")}>
      <input value={auth.itemId} spellCheck={false} onBlur={() => touch("ssh.auth.itemId")}
        onChange={event => edit({ auth: { kind: "storedPassword", itemId: event.target.value } }, "ssh.auth.itemId")} />
    </Field>}
    {text("hostKeyFingerprint", "호스트 키 SHA256 지문 (선택)")}
  </>;
}
