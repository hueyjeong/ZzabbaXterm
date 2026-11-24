// @vitest-environment jsdom
import { act, StrictMode } from "react";
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

type Ended = (message: string, closed: boolean, kind?: "transport") => void;
const ended = (call: number) => (connect.mock.calls[call] as unknown[])[5] as Ended;

it("저장 프로필로 연 탭은 StrictMode에서도 한 번만 바로 연결한다", async () => {
  const host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => {
    root!.render(<StrictMode><RdpTab active autoStart label="RDP 1" preset={{ host: "xrdp.example.test", port: 3390, tlsOnly: true }} /></StrictMode>);
  });
  expect(connect).toHaveBeenCalledTimes(1);
  expect((connect.mock.calls[0] as unknown[])[0]).toEqual({ host: "xrdp.example.test", port: 3390, tlsOnly: true });
  expect(host.querySelector("form")).toBeNull();
  expect(host.textContent).toContain("xrdp.example.test:3390");
});

it("연결이 끊기면 오류와 다시 연결을 보이고 같은 초안으로 다시 연결한다", async () => {
  const host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => { root!.render(<RdpTab active autoStart label="RDP 1" preset={{ host: "xrdp.example.test", port: 3390 }} />); });
  await act(async () => { ended(0)("RDP 연결이 종료되었습니다", true); });
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("RDP 연결이 종료되었습니다");
  expect(host.querySelector("form")).toBeNull();
  await act(async () => { host.querySelector<HTMLButtonElement>('button[aria-label="다시 연결"]')!.click(); });
  expect(connect).toHaveBeenCalledTimes(2);
  expect((connect.mock.calls[1] as unknown[])[0]).toEqual((connect.mock.calls[0] as unknown[])[0]);
  expect(host.querySelector('[role="alert"]')).toBeNull();

  await act(async () => { ended(1)("RDP 연결이 종료되었습니다", true); });
  const edit = [...host.querySelectorAll("button")].find((button) => button.textContent === "연결 설정 편집")!;
  await act(async () => { edit.click(); });
  expect(host.querySelector<HTMLInputElement>("input:not([type])")?.value).toBe("xrdp.example.test");
});

it("터널 경로가 끊기면 경로를 다시 열고 새 경로가 준비된 뒤 연결한다", async () => {
  const host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  const preset = { host: "xrdp.example.test", port: 3389 };
  const first = { tabHandle: "route-tab", tunnelId: "9", targetId: "ssh-server", destination: { host: "127.0.0.1", port: 3389 } };
  const onReopenRoute = vi.fn(() => Promise.resolve(true));
  const render = (source: typeof first) => act(async () => {
    root!.render(<RdpTab active autoStart label="RDP 1" source={source} preset={preset} onReopenRoute={onReopenRoute} />);
  });
  await render(first);
  await act(async () => { ended(0)("RDP 전송 연결이 끊겼습니다", true, "transport"); });
  await act(async () => { host.querySelector<HTMLButtonElement>('button[aria-label="다시 연결"]')!.click(); });
  expect(onReopenRoute).toHaveBeenCalledOnce();
  expect(connect).toHaveBeenCalledTimes(1);
  const second = { ...first, tabHandle: "route-tab-2", tunnelId: "10" };
  await render(second);
  expect(connect).toHaveBeenCalledTimes(2);
  expect((connect.mock.calls[1] as unknown[])[0]).toEqual({ host: "xrdp.example.test", port: 3389, tlsOnly: false, source: second });
});

it("경로 다시 열기가 실패하면 뒤늦게 바뀐 경로로 연결하지 않고 다시 연결을 다시 받는다", async () => {
  const host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  const preset = { host: "xrdp.example.test", port: 3389 };
  const first = { tabHandle: "route-tab", tunnelId: "9", targetId: "ssh-server", destination: { host: "127.0.0.1", port: 3389 } };
  const onReopenRoute = vi.fn(() => Promise.resolve(false));
  const render = (source: typeof first) => act(async () => {
    root!.render(<RdpTab active autoStart label="RDP 1" source={source} preset={preset} onReopenRoute={onReopenRoute} />);
  });
  await render(first);
  await act(async () => { ended(0)("RDP 전송 연결이 끊겼습니다", true, "transport"); });
  const retry = () => act(async () => { host.querySelector<HTMLButtonElement>('button[aria-label="다시 연결"]')!.click(); });
  await retry();
  expect(onReopenRoute).toHaveBeenCalledOnce();
  await render({ ...first, tabHandle: "late-tab" });
  expect(connect).toHaveBeenCalledTimes(1);
  await retry();
  expect(onReopenRoute).toHaveBeenCalledTimes(2);
});

it("전송 오류라도 종류 코드가 없으면 같은 경로로 다시 연결한다", async () => {
  const host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  const source = { tabHandle: "route-tab", tunnelId: "9", targetId: "ssh-server", destination: { host: "127.0.0.1", port: 3389 } };
  const onReopenRoute = vi.fn(() => Promise.resolve(true));
  await act(async () => {
    root!.render(<RdpTab active autoStart label="RDP 1" source={source} preset={{ host: "xrdp.example.test", port: 3389 }} onReopenRoute={onReopenRoute} />);
  });
  await act(async () => { ended(0)("RDP 전송 연결이 끊겼습니다", true); });
  await act(async () => { host.querySelector<HTMLButtonElement>('button[aria-label="다시 연결"]')!.click(); });
  expect(onReopenRoute).not.toHaveBeenCalled();
  expect(connect).toHaveBeenCalledTimes(2);
});

it("다시 연결을 빠르게 두 번 눌러도 경로는 한 번만 다시 연다", async () => {
  const host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  const source = { tabHandle: "route-tab", tunnelId: "9", targetId: "ssh-server", destination: { host: "127.0.0.1", port: 3389 } };
  const reopened = Promise.withResolvers<boolean>();
  const onReopenRoute = vi.fn(() => reopened.promise);
  await act(async () => {
    root!.render(<RdpTab active autoStart label="RDP 1" source={source} preset={{ host: "xrdp.example.test", port: 3389 }} onReopenRoute={onReopenRoute} />);
  });
  await act(async () => { ended(0)("RDP 전송 연결이 끊겼습니다", true, "transport"); });
  const button = host.querySelector<HTMLButtonElement>('button[aria-label="다시 연결"]')!;
  await act(async () => { button.click(); button.click(); });
  expect(onReopenRoute).toHaveBeenCalledOnce();
  expect(connect).toHaveBeenCalledTimes(1);
  await act(async () => { reopened.resolve(true); });
});
