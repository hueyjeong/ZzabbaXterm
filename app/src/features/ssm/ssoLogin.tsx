import { invoke } from "@tauri-apps/api/core";
import { LogIn } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
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

export type SsoLogin = Readonly<{ state: SsoLoginState; start(): void; cancel(): void }>;

/**
 * 저장 대상의 SSO 로그인을 실행하고, 성공하면 시작할 때의 대상 ID로 onSuccess를 부른다.
 * cancel이나 언마운트는 진행 중인 로그인을 취소하고 onSuccess를 부르지 않는다.
 */
export function useSsoLogin(targetId: string | undefined, onSuccess: (targetId: string) => void, ipc: SsoLoginIpc = ssoLoginIpc): SsoLogin {
  const [state, setState] = useState<SsoLoginState>({ kind: "idle" });
  const running = useRef<string | undefined>(undefined);
  const success = useRef(onSuccess);
  useLayoutEffect(() => { success.current = onSuccess; });
  const cancel = useCallback(() => {
    const requestId = running.current;
    running.current = undefined;
    if (!requestId) return;
    setState({ kind: "idle" });
    void ipc.cancel(requestId).catch(() => { });
  }, [ipc]);
  useEffect(() => () => {
    const requestId = running.current;
    running.current = undefined;
    if (requestId) void ipc.cancel(requestId).catch(() => { });
  }, [ipc]);

  function start() {
    if (!targetId || running.current) return;
    const target = targetId;
    const requestId = crypto.randomUUID();
    running.current = requestId;
    setState({ kind: "pending" });
    ipc.login(target, requestId).then(() => {
      if (running.current !== requestId) return;
      running.current = undefined;
      setState({ kind: "idle" });
      success.current(target);
    }, (error: unknown) => {
      if (running.current !== requestId) return;
      running.current = undefined;
      setState({ kind: "failed", error: String(error) });
    });
  }

  return { state, start, cancel };
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

export function SsoLoginAction({ login, disabled, onStart }: { readonly login: SsoLogin; readonly disabled?: boolean; readonly onStart?: () => void }) {
  return <><SsoLoginButton login={login} disabled={disabled} onStart={onStart} /><SsoLoginStatus login={login} /></>;
}
