// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { RdpTab } from "./RdpTab";

const { connect } = vi.hoisted(() => ({ connect: vi.fn(() => () => Promise.resolve()) }));
vi.mock("./session", () => ({ connect }));
vi.mock("../remoteDesktop/RemoteDesktopView", () => ({ RemoteDesktopView: () => null }));

let root: Root | undefined;
afterEach(() => {
  act(() => root?.unmount());
  root = undefined;
  vi.clearAllMocks();
});

function dispatchTrusted(target: HTMLElement, event: Event) {
  // jsdom의 공개 dispatchEvent는 항상 isTrusted를 지운다. 실제 컴포넌트의 신뢰 검사는 유지한다.
  const impl = Object.getOwnPropertySymbols(event).find(symbol => symbol.description === "impl");
  if (!impl) throw new Error("jsdom 이벤트 구현을 찾을 수 없습니다");
  const state = Reflect.get(event, impl);
  state.isTrusted = true;
  Reflect.get(target, impl)._dispatch(state);
}

it("TLS 전용 opt-in은 기본 꺼짐이고 켠 값이 초안과 연결 상태에 표시된다", async () => {
  const host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  const onSecurityMode = vi.fn();
  await act(async () => { root!.render(<RdpTab active label="RDP 1" onSecurityMode={onSecurityMode} />); });

  const checkbox = host.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
  expect(checkbox.checked).toBe(false);
  expect(checkbox.closest("label")?.textContent).toContain("NLA를 지원하지 않는 서버(TLS 전용) 허용");
  const name = host.querySelector<HTMLInputElement>("input:not([type])")!;
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    setter.call(name, "rdp.example.invalid");
    name.dispatchEvent(new Event("input", { bubbles: true }));
    checkbox.click();
    dispatchTrusted(host.querySelector("form")!, new Event("submit", { bubbles: true, cancelable: true }));
  });

  expect(connect).toHaveBeenCalledWith(
    { host: "rdp.example.invalid", port: 3389, tlsOnly: true },
    expect.any(Function), expect.any(Function), expect.any(Function), expect.any(Function), expect.any(Function),
  );
  expect(host.textContent).toContain("NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)");
  expect(onSecurityMode).toHaveBeenCalledWith(true);
});

it("저장 프로필의 값을 입력칸에 미리 채우고 사용자가 연결을 누르기 전에는 연결하지 않는다", async () => {
  const host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  const source = { tabHandle: "route-tab", tunnelId: "9", targetId: "ssh-server", destination: { host: "127.0.0.1", port: 3389 } };
  await act(async () => { root!.render(<RdpTab active label="RDP 1" preset={{ host: "xrdp.example.test", port: 3390, tlsOnly: true }} />); });
  expect(host.querySelector<HTMLInputElement>("input:not([type])")?.value).toBe("xrdp.example.test");
  expect(host.querySelector<HTMLInputElement>('input[type="number"]')?.value).toBe("3390");
  expect(host.querySelector<HTMLInputElement>('input[type="checkbox"]')?.checked).toBe(true);
  expect(connect).not.toHaveBeenCalled();

  await act(async () => { root!.render(<RdpTab key="tunnel" active label="RDP 1" source={source} preset={{ host: "xrdp.example.test", port: 3389, tlsOnly: false }} />); });
  expect(host.querySelector<HTMLInputElement>("input:not([type])")?.value).toBe("xrdp.example.test");
  expect(host.querySelector('input[type="number"]')).toBeNull();
  await act(async () => { dispatchTrusted(host.querySelector("form")!, new Event("submit", { bubbles: true, cancelable: true })); });
  expect(connect).toHaveBeenCalledTimes(1);
  expect((connect.mock.calls[0] as unknown[])[0]).toEqual({ host: "xrdp.example.test", port: 3389, tlsOnly: false, source });
});
