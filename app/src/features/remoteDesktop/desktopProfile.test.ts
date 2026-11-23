import { describe, expect, it, vi } from "vitest";
import type { DesktopRoute } from "../connections/profiles";
import type { TunnelStatus } from "../tunnels/tunnelIpc";
import { openDesktopProfile, SSM_FORWARD_UNAVAILABLE, type DesktopProfile } from "./desktopProfile";

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
    start: vi.fn(async () => start),
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

  it("SSM 경로는 Rust 터널을 시작하지 않고 차단 사유를 돌려준다", async () => {
    const ipc = fakeIpc([]);
    await expect(openDesktopProfile(rdp(ssm), { ipc })).rejects.toThrow(SSM_FORWARD_UNAVAILABLE);
    expect(ipc.open).not.toHaveBeenCalled();
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
