import { describe, expect, it, vi } from "vitest";
import type { DesktopRoute } from "../connections/profiles";
import type { TunnelRequest, TunnelStatus } from "../tunnels/tunnelIpc";
import { DesktopRouteError, openDesktopProfile, type DesktopProfile } from "./desktopProfile";

const ssm = { kind: "ssm", profile: "company-sso", region: "ap-northeast-2", instanceId: "i-0123456789abcdef0" } as const;
const rdp = (route: DesktopRoute): DesktopProfile => ({
  id: "rdp-desktop", displayName: "윈도 서버", defaultMethod: "rdp", rdp: { host: "win.example.test", port: 3389, tlsOnly: true, route },
});
const tunnel = (state: TunnelStatus["state"], extra: Partial<TunnelStatus> = {}): TunnelStatus => ({
  id: "7", endpoint: "deploy@dev.example.test:22", route: "-L 127.0.0.1:50000 → 127.0.0.1:3389", owners: [], state, error: null, ...extra,
});

function fakeIpc(statuses: TunnelStatus[][], start: string | null = "7") {
  return {
    open: vi.fn(async () => "route-tab"),
    start: vi.fn(async (_tab: string, _request: TunnelRequest) => start),
    status: vi.fn(async () => statuses.shift() ?? []),
    close: vi.fn(async () => null),
  };
}

describe("저장 RDP·VNC 프로필 열기", () => {
  it("직접 경로는 터널 없이 미리 채울 값만 돌려준다", async () => {
    const ipc = fakeIpc([]);
    const opened = await openDesktopProfile(rdp({ kind: "direct" }), { ipc });
    expect(opened).toMatchObject({ kind: "rdp", label: "윈도 서버", preset: { host: "win.example.test", port: 3389, tlsOnly: true } });
    expect(opened.source).toBeUndefined();
    await opened.close();
    expect(ipc.open).not.toHaveBeenCalled();
  });

  it("RDP SSM 경로는 프로필 자신을 경로 대상으로 TLS DNS 이름의 원격 호스트 전달을 연다", async () => {
    const desktop = { protocol: "rdp" as const, targetId: "rdp-desktop", destination: { host: "win.example.test", port: 3389 } };
    const route = "SSM 127.0.0.1:50002 → i-0123456789abcdef0 → win.example.test:3389";
    const ipc = fakeIpc([[tunnel("starting", { route })], [tunnel("starting", { route })], [tunnel("ready", { route, desktop })]]);
    const opened = await openDesktopProfile(rdp(ssm), { ipc, listenPort: () => 50002, pollMs: 1 });
    expect(ipc.open).toHaveBeenCalledWith("rdp-desktop", "윈도 서버 · RDP 경로");
    expect(ipc.start).toHaveBeenCalledWith("route-tab", { direction: "ssm", listen: 50002, port: 3389, host: "win.example.test", protocol: "rdp" });
    expect(ipc.status).toHaveBeenCalledTimes(3);
    expect(opened.preset).toEqual({ host: "win.example.test", port: 3389, tlsOnly: true });
    expect(opened.source).toEqual({ tabHandle: "route-tab", tunnelId: "7", targetId: "rdp-desktop", destination: desktop.destination });
    await opened.close();
    expect(ipc.close).toHaveBeenCalledWith("route-tab");
  });

  it("VNC SSM 경로는 노드 자신의 서비스만 요청한다", async () => {
    const desktop = { protocol: "vnc" as const, targetId: "vnc-desktop", destination: { host: "127.0.0.1", port: 5901 } };
    const ipc = fakeIpc([[tunnel("ready", { desktop })]]);
    const vnc: DesktopProfile = { id: "vnc-desktop", displayName: "노드 화면", defaultMethod: "vnc", vnc: { host: "", port: 5901, route: ssm } };
    const opened = await openDesktopProfile(vnc, { ipc, listenPort: () => 50003, pollMs: 1 });
    expect(ipc.open).toHaveBeenCalledWith("vnc-desktop", "노드 화면 · VNC 경로");
    const request = ipc.start.mock.calls[0]?.[1];
    expect(request).toEqual({ direction: "ssm", listen: 50003, port: 5901, protocol: "vnc" });
    expect(request && "host" in request).toBe(false);
    expect(opened.source?.destination).toEqual({ host: "127.0.0.1", port: 5901 });
  });

  it.each([
    ["승인 거절", [], null, "SSM 터널 승인이 취소되었습니다"],
    ["SSO 만료", [[tunnel("failed", { error: "SSM: SSO 재로그인 필요" })]], "7", "SSM: SSO 재로그인 필요"],
    ["문서 실행 거부", [[tunnel("starting")], [tunnel("failed", { error: "SSM: 포트 전달 문서 실행 권한 거부" })]], "7", "SSM: 포트 전달 문서 실행 권한 거부"],
    ["준비 뒤 끊김", [[tunnel("disconnected", { error: "터널 프로세스가 종료되었습니다" })]], "7", "터널 프로세스가 종료되었습니다"],
  ] as const)("SSM 경로 %s이면 경로 탭을 닫고 Rust 오류를 그대로 돌려준다", async (_, statuses, start, message) => {
    const ipc = fakeIpc(statuses.map(item => [...item]), start);
    await expect(openDesktopProfile(rdp(ssm), { ipc, listenPort: () => 50004, pollMs: 1 })).rejects.toThrow(message);
    expect(ipc.close).toHaveBeenCalledWith("route-tab");
  });

  it("SSM 경로의 준비 시간이 지나면 실패하고, 종료 확인 실패는 SSM 경로 탭으로 남길 정보를 준다", async () => {
    const ipc = fakeIpc(Array.from({ length: 50 }, () => [tunnel("starting")]));
    ipc.close.mockRejectedValueOnce("종료 실패");
    const failure = await openDesktopProfile(rdp(ssm), { ipc, listenPort: () => 50005, pollMs: 1, timeoutMs: 5 }).catch(error => error);
    expect(failure).toBeInstanceOf(DesktopRouteError);
    expect(failure).toMatchObject({ handle: "route-tab", targetId: "rdp-desktop", method: "ssm",
      message: "터널 준비 시간이 초과되었습니다 · 터널 종료 확인 불가: 종료 실패" });
  });

  it("SSH 경로는 서버 루프백 전달이 준비된 뒤 Rust가 확인한 목적지로 source를 만든다", async () => {
    const desktop = { protocol: "vnc" as const, targetId: "ssh-server", destination: { host: "127.0.0.1", port: 5901 } };
    const ipc = fakeIpc([[tunnel("starting")], [tunnel("ready", { desktop })]]);
    const vnc: DesktopProfile = { id: "vnc-desktop", displayName: "리눅스 화면", defaultMethod: "vnc",
      vnc: { host: "", port: 5901, route: { kind: "ssh", targetId: "ssh-server" } } };
    const opened = await openDesktopProfile(vnc, { ipc, listenPort: () => 50001, pollMs: 1 });
    expect(ipc.open).toHaveBeenCalledWith("ssh-server", "리눅스 화면 · VNC 경로");
    expect(ipc.start).toHaveBeenCalledWith("route-tab", { direction: "local", listen: 50001, host: "127.0.0.1", port: 5901, protocol: "vnc" });
    expect(opened.source).toEqual({ tabHandle: "route-tab", tunnelId: "7", targetId: "ssh-server", destination: desktop.destination });
    expect(opened.preset).toEqual({ host: "", port: 5901 });
    expect(ipc.close).not.toHaveBeenCalled();
    await opened.close();
    expect(ipc.close).toHaveBeenCalledWith("route-tab");
  });

  it.each([
    ["승인 거절", [], null, "SSH 터널 승인이 취소되었습니다"],
    ["시작 실패", [[tunnel("failed", { error: "로컬 수신 주소 사용 중: 127.0.0.1:50000" })]], "7", "로컬 수신 주소 사용 중: 127.0.0.1:50000"],
    ["서비스 불일치", [[tunnel("ready", { desktop: { protocol: "vnc", targetId: "ssh-server", destination: { host: "127.0.0.1", port: 3389 } } })]], "7", "승인된 터널 서비스가 일치하지 않습니다"],
    ["서비스 미지정", [[tunnel("ready")]], "7", "승인된 터널 서비스가 일치하지 않습니다"],
  ] as const)("%s이면 경로용 터널 탭을 닫고 실제 오류를 돌려준다", async (_, statuses, start, message) => {
    const ipc = fakeIpc(statuses.map(item => [...item]), start);
    await expect(openDesktopProfile(rdp({ kind: "ssh", targetId: "ssh-server" }), { ipc, listenPort: () => 50000, pollMs: 1 }))
      .rejects.toThrow(message);
    expect(ipc.close).toHaveBeenCalledWith("route-tab");
  });

  it("터널 종료를 확인하지 못하면 원래 오류와 함께 표시한다", async () => {
    const ipc = fakeIpc([], null);
    ipc.close.mockRejectedValueOnce("종료 실패");
    await expect(openDesktopProfile(rdp({ kind: "ssh", targetId: "ssh-server" }), { ipc }))
      .rejects.toThrow("SSH 터널 승인이 취소되었습니다 · 터널 종료 확인 불가: 종료 실패");
  });
});
