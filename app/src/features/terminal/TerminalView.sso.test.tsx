// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { SsoLoginIpc } from "../ssm/ssoLogin";
import { TerminalView } from "./TerminalView";
import type { LocalTerminalIpc } from "./session";

vi.hoisted(() => {
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(null);
});

let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("matchMedia", () => ({ matches: false, addListener() { }, removeListener() { } }));
  vi.stubGlobal("ResizeObserver", class { observe() { } disconnect() { } });
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(800);
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(400);
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => { root.unmount(); });
  host.remove();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function terminalIpc(open: LocalTerminalIpc["open"]): LocalTerminalIpc {
  return {
    open, input: vi.fn().mockResolvedValue(undefined), start: vi.fn().mockResolvedValue(undefined),
    ack: vi.fn().mockResolvedValue(undefined), resize: vi.fn().mockResolvedValue(undefined), close: async () => null,
  };
}

const button = (label: string) => host.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`);
const texts = (role: string) => [...host.querySelectorAll(`[role="${role}"]`)].map(node => node.textContent);

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

it("SSO 만료로 열지 못한 저장 대상은 로그인 뒤 새 세션을 연다", async () => {
  const open = vi.fn<LocalTerminalIpc["open"]>()
    .mockRejectedValueOnce("SSM: SSO 재로그인 필요")
    .mockResolvedValue("ssm-session");
  const pending = deferred<"loggedIn">();
  const sso: SsoLoginIpc = { login: vi.fn().mockReturnValue(pending.promise), cancel: vi.fn().mockResolvedValue(null) };
  const onStatus = vi.fn();
  await act(async () => {
    root.render(<TerminalView active focusRequested={false} label="SSM 셸" targetId="ssm-node" remote reconnect
      ipc={terminalIpc(open)} ssoIpc={sso} onStatus={onStatus} />);
  });
  expect(texts("status")).toEqual(["열기 실패: SSM: SSO 재로그인 필요"]);
  expect(button("다시 연결")).toBeNull();

  await act(async () => { button("SSO 로그인")!.click(); });
  expect(sso.login).toHaveBeenCalledWith("ssm-node", expect.stringMatching(/^[0-9a-f-]{36}$/));
  expect(texts("status")).toContain("SSO 로그인 중");
  expect(button("SSO 로그인")!.disabled).toBe(true);
  expect(open).toHaveBeenCalledTimes(1);

  await act(async () => { pending.resolve("loggedIn"); });
  await vi.waitFor(() => expect(open).toHaveBeenCalledTimes(2));
  expect(open.mock.calls[1][2]).toBe("ssm-node");
  expect(onStatus).toHaveBeenCalledWith({ kind: "opening" });
  expect(button("SSO 로그인")).toBeNull();
  expect(texts("status")).toEqual(["여는 중"]);
  expect(sso.cancel).not.toHaveBeenCalled();
});

it("SSO 로그인 실패는 실제 오류를 보이고 다시 열지 않는다", async () => {
  const open = vi.fn<LocalTerminalIpc["open"]>().mockRejectedValue("SSM: SSO 재로그인 필요");
  const sso: SsoLoginIpc = {
    login: vi.fn().mockRejectedValue("SSM: AWS 프로필의 신원이 바뀌었습니다"), cancel: vi.fn().mockResolvedValue(null),
  };
  await act(async () => {
    root.render(<TerminalView active focusRequested={false} label="SSM 셸" targetId="ssm-node" remote reconnect
      ipc={terminalIpc(open)} ssoIpc={sso} />);
  });
  await act(async () => { button("SSO 로그인")!.click(); });
  expect(texts("alert")).toEqual(["SSO 로그인 실패: SSM: AWS 프로필의 신원이 바뀌었습니다"]);
  expect(button("SSO 로그인")!.disabled).toBe(false);
  expect(open).toHaveBeenCalledTimes(1);
});

it("로그인 중에 탭을 닫으면 같은 요청을 취소한다", async () => {
  const open = vi.fn<LocalTerminalIpc["open"]>().mockRejectedValue("SSM: SSO 재로그인 필요");
  const sso: SsoLoginIpc = { login: vi.fn().mockReturnValue(new Promise(() => { })), cancel: vi.fn().mockResolvedValue(null) };
  await act(async () => {
    root.render(<TerminalView active focusRequested={false} label="SSM 셸" targetId="ssm-node" remote
      ipc={terminalIpc(open)} ssoIpc={sso} />);
  });
  await act(async () => { button("SSO 로그인")!.click(); });
  await act(async () => { root.render(<></>); });
  expect(sso.cancel).toHaveBeenCalledWith(vi.mocked(sso.login).mock.calls[0][1]);
});

it("닫히는 탭은 진행 중인 로그인을 취소하고 로그인이 끝나도 다시 열지 않는다", async () => {
  const open = vi.fn<LocalTerminalIpc["open"]>().mockRejectedValue("SSM: SSO 재로그인 필요");
  const pending = deferred<"loggedIn">();
  const sso: SsoLoginIpc = { login: vi.fn().mockReturnValue(pending.promise), cancel: vi.fn().mockResolvedValue(null) };
  const ipc = terminalIpc(open);
  const render = (closing: boolean) => root.render(<TerminalView active focusRequested={false} label="SSM 셸" targetId="ssm-node"
    remote ipc={ipc} ssoIpc={sso} closing={closing} />);
  await act(async () => { render(false); });
  await act(async () => { button("SSO 로그인")!.click(); });
  await act(async () => { render(true); });
  expect(sso.cancel).toHaveBeenCalledWith(vi.mocked(sso.login).mock.calls[0][1]);
  await act(async () => { pending.resolve("loggedIn"); });
  expect(open).toHaveBeenCalledTimes(1);
});

it("SSO 외 사유로 실패한 탭은 다시 연결하고 저장 대상이 없으면 SSO 로그인을 보이지 않는다", async () => {
  const open = vi.fn<LocalTerminalIpc["open"]>()
    .mockRejectedValueOnce("SSM: SSO 재로그인 필요")
    .mockRejectedValueOnce("SSM: 노드 연결 불가")
    .mockResolvedValue("local");
  const sso: SsoLoginIpc = { login: vi.fn(), cancel: vi.fn() };
  const ipc = terminalIpc(open);
  await act(async () => {
    root.render(<TerminalView active focusRequested={false} label="셸" reconnect ipc={ipc} ssoIpc={sso} />);
  });
  expect(button("SSO 로그인")).toBeNull();
  await act(async () => { button("다시 연결")!.click(); });
  await vi.waitFor(() => expect(texts("status")).toEqual(["열기 실패: SSM: 노드 연결 불가"]));
  await act(async () => { button("다시 연결")!.click(); });
  expect(open).toHaveBeenCalledTimes(3);
  expect(button("다시 연결")).toBeNull();
  expect(sso.login).not.toHaveBeenCalled();
});
