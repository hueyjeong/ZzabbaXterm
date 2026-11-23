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
