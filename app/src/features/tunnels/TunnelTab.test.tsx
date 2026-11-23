// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { TunnelTab } from "./TunnelTab";
import { tunnelIpc } from "./tunnelIpc";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

let host: HTMLDivElement;
let root: Root;
const onShare = vi.fn();
const onOpenDesktop = vi.fn();
const props = { tab: "owned", targetId: "saved-target", method: "ssh", onShare, onOpenDesktop } as const;
const tunnel = { id: "7", route: "-D 127.0.0.1:1080 → SOCKS", endpoint: "fixture@127.0.0.1:2222", owners: ["시험 A", "시험 B"], error: null };

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Linux");
  invoke.mockReset();
  onShare.mockReset();
  onOpenDesktop.mockReset();
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("distinguishes readiness, failure and disconnection for the owned tunnel", async () => {
  vi.useFakeTimers();
  invoke.mockResolvedValueOnce([{ ...tunnel, state: "starting" }])
    .mockResolvedValueOnce([{ ...tunnel, state: "ready" }])
    .mockResolvedValueOnce([{ ...tunnel, state: "disconnected", error: "SSH 자식 종료" },
      { ...tunnel, id: "8", state: "failed", error: "수신 포트 충돌" }]);
  await act(async () => root.render(<TunnelTab {...props} />));
  expect(host.querySelector('[role="status"]')?.textContent).toBe("준비 중");
  expect(host.textContent).toContain("시험 A, 시험 B");
  await act(async () => { await vi.advanceTimersByTimeAsync(300); });
  expect(host.querySelector('[role="status"]')?.textContent).toBe("수신 준비됨");
  await act(async () => host.querySelector<HTMLButtonElement>('li button')?.click());
  expect(onShare).toHaveBeenCalledWith("saved-target", "owned", "7");
  await act(async () => { await vi.advanceTimersByTimeAsync(300); });
  expect([...host.querySelectorAll('[role="status"]')].map(status => status.textContent)).toEqual(["끊김", "시작 실패"]);
  expect([...host.querySelectorAll('[role="alert"]')].map(alert => alert.textContent)).toEqual(["SSH 자식 종료", "수신 포트 충돌"]);
  expect(invoke.mock.calls).toEqual(Array.from({ length: 3 }, () => ["tunnel_status", { tab: "owned" }]));
});

it.each(["rdp", "vnc"] as const)("offers only the Ready %s desktop action with the Rust destination", async protocol => {
  const destination = { host: "private.example.invalid", port: 14444 };
  const desktop = { protocol, targetId: "desktop-target", destination };
  invoke.mockResolvedValue([
    { ...tunnel, id: "tcp", state: "ready" },
    { ...tunnel, id: "starting", state: "starting", desktop },
    { ...tunnel, id: "failed", state: "failed", desktop },
    { ...tunnel, id: "disconnected", state: "disconnected", desktop },
    { ...tunnel, state: "ready", desktop },
  ]);
  await act(async () => root.render(<TunnelTab {...props} />));
  const actions = () => [...host.querySelectorAll<HTMLButtonElement>("li button")]
    .filter(button => button.textContent?.endsWith("로 열기"));
  expect(actions().map(button => button.textContent)).toEqual([`${protocol.toUpperCase()}로 열기`]);
  expect(actions()[0]?.closest("li")?.querySelector('[role="status"]')?.textContent).toBe("수신 준비됨");
  await act(async () => actions()[0]?.click());
  expect(onOpenDesktop.mock.calls).toEqual([[protocol, {
    tabHandle: "owned", tunnelId: "7", targetId: "desktop-target", destination,
  }]]);
  await act(async () => root.render(<TunnelTab {...props} closeState={{ kind: "closing" }} />));
  expect(actions()).toEqual([]);
});

it("reports the Windows -R limit before start", async () => {
  vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Windows");
  invoke.mockResolvedValue([]);
  await act(async () => root.render(<TunnelTab {...props} />));
  expect(host.querySelector<HTMLOptionElement>('option[value="remote"]')?.disabled).toBe(true);
  expect(host.textContent).toContain("Windows에서 -R 원격 수신을 지원하지 않음");
  expect(invoke.mock.calls).toEqual([["tunnel_status", { tab: "owned" }]]);
});

it.each([
  ["노드 자신", "", "vnc", { direction: "ssm", listen: 15901, port: 5901, protocol: "vnc" }],
  ["원격 호스트", " rdp.internal.example ", "rdp", { direction: "ssm", listen: 15901, port: 5901, host: "rdp.internal.example", protocol: "rdp" }],
] as const)("starts an SSM route to %s on the owned handle", async (_, remote, service, request) => {
  vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Windows");
  invoke.mockResolvedValueOnce([]).mockResolvedValueOnce("9");
  await act(async () => root.render(<TunnelTab {...props} method="ssm" />));
  expect(host.querySelector('select:not([name])')).toBeNull();
  expect(host.textContent).not.toContain("Windows에서 -R");
  const field = (name: string) => host.querySelector<HTMLInputElement | HTMLSelectElement>(`[name="${name}"]`);
  const [listen, address, port, protocol] = ["listen", "host", "port", "protocol"].map(field);
  if (!listen || !address || !port || !protocol) throw new Error("SSM 터널 입력이 없습니다");
  listen.value = "15901";
  address.value = remote;
  port.value = "5901";
  protocol.value = service;
  await act(async () => host.querySelector<HTMLButtonElement>('button[type="submit"]')?.click());
  expect(invoke).toHaveBeenNthCalledWith(2, "tunnel_start", { tab: "owned", request });
});

it("keeps the close error and retries the same owned handle", async () => {
  vi.useFakeTimers();
  invoke.mockResolvedValueOnce([{ ...tunnel, state: "ready" }])
    .mockRejectedValueOnce("터널 종료 확인 불가: fixture")
    .mockResolvedValueOnce(null);
  await act(async () => root.render(<TunnelTab {...props} />));
  await act(async () => root.render(<TunnelTab {...props} closeState={{ kind: "closing" }} />));
  await expect(tunnelIpc.close(props.tab)).rejects.toBe("터널 종료 확인 불가: fixture");
  await act(async () => root.render(<TunnelTab {...props} closeState={{ kind: "unconfirmed", error: "터널 종료 확인 불가: fixture" }} />));
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("터널 종료 확인 불가: fixture");
  expect(host.querySelector<HTMLButtonElement>('li button')?.disabled).toBe(true);
  expect(host.querySelector('fieldset')?.disabled).toBe(true);
  await act(async () => { await vi.advanceTimersByTimeAsync(300); });
  await expect(tunnelIpc.close(props.tab)).resolves.toBeNull();
  expect(invoke.mock.calls).toEqual([
    ["tunnel_status", { tab: "owned" }],
    ["tunnel_tab_close", { tab: "owned" }],
    ["tunnel_tab_close", { tab: "owned" }],
  ]);
});

it("starts a local route on the owned handle without sending saved connection settings", async () => {
  invoke.mockResolvedValueOnce([]).mockResolvedValueOnce("7");
  await act(async () => root.render(<TunnelTab {...props} />));
  const listen = host.querySelector<HTMLInputElement>('input[name="listen"]');
  const port = host.querySelector<HTMLInputElement>('input[name="port"]');
  const protocol = host.querySelector<HTMLSelectElement>('select[name="protocol"]');
  if (!listen || !port || !protocol) throw new Error("터널 입력이 없습니다");
  listen.value = "13389";
  port.value = "3389";
  protocol.value = "rdp";
  await act(async () => host.querySelector<HTMLButtonElement>('button[type="submit"]')?.click());
  expect(invoke).toHaveBeenNthCalledWith(2, "tunnel_start", {
    tab: "owned", request: { direction: "local", listen: 13389, host: "127.0.0.1", port: 3389, protocol: "rdp" },
  });
});
