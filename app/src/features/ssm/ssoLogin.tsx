import { invoke } from "@tauri-apps/api/core";
import { LogIn } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { IconButton } from "../ui/controls";

/** Rust `start_error(CliError::SsoLoginRequired)`가 만드는 문자열이다. */
export const SSO_LOGIN_REQUIRED = "SSM: SSO 재로그인 필요";

export const ssoExpired = (message: string | null | undefined) => !!message?.includes(SSO_LOGIN_REQUIRED);

export interface SsoLoginIpc {
  login(targetId: string, requestId: string): Promise<"valid" | "loggedIn">;
  cancel(requestId: string): Promise<unknown>;
}

export const ssoLoginIpc: SsoLoginIpc = {
  login: (targetId, requestId) => invoke("ssm_target_relogin", { targetId, requestId }),
  cancel: (requestId) => invoke("ssm_target_relogin_cancel", { requestId }),
};

export type SsoLoginState = { kind: "idle" } | { kind: "pending" } | { kind: "failed"; error: string };

export type SsoLogin = Readonly<{ state: SsoLoginState; start(): void }>;

/** 저장 대상의 SSO 로그인을 실행하고, 성공하면 onSuccess로 같은 대상을 다시 연다. 언마운트하면 취소한다. */
export function useSsoLogin(targetId: string | undefined, onSuccess: () => void, ipc: SsoLoginIpc = ssoLoginIpc): SsoLogin {
  const [state, setState] = useState<SsoLoginState>({ kind: "idle" });
  const running = useRef<string | undefined>(undefined);
  const success = useRef(onSuccess);
  useLayoutEffect(() => { success.current = onSuccess; });
  useEffect(() => () => {
    const requestId = running.current;
    running.current = undefined;
    if (requestId) void ipc.cancel(requestId).catch(() => { });
  }, [ipc]);

  function start() {
    if (!targetId || running.current) return;
    const requestId = crypto.randomUUID();
    running.current = requestId;
    setState({ kind: "pending" });
    ipc.login(targetId, requestId).then(() => {
      if (running.current !== requestId) return;
      running.current = undefined;
      setState({ kind: "idle" });
      success.current();
    }, (error: unknown) => {
      if (running.current !== requestId) return;
      running.current = undefined;
      setState({ kind: "failed", error: String(error) });
    });
  }

  return { state, start };
}

export function SsoLoginButton({ login, disabled, onStart }: { readonly login: SsoLogin; readonly disabled?: boolean; readonly onStart?: () => void }) {
  return <IconButton icon={LogIn} label="SSO 로그인" small disabled={disabled || login.state.kind === "pending"}
    onClick={() => { onStart?.(); login.start(); }} />;
}

export function SsoLoginStatus({ login }: { readonly login: SsoLogin }) {
  const { state } = login;
  return <>
    {state.kind === "pending" && <span role="status">SSO 로그인 중</span>}
    {state.kind === "failed" && <span role="alert">SSO 로그인 실패: {state.error}</span>}
  </>;
}

export function SsoLoginAction({ login, disabled }: { readonly login: SsoLogin; readonly disabled?: boolean }) {
  return <><SsoLoginButton login={login} disabled={disabled} /><SsoLoginStatus login={login} /></>;
}
