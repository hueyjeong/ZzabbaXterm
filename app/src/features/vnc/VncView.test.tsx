// @vitest-environment jsdom
import { act, StrictMode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { VncView } from "./VncView";

const { connect, createOwner } = vi.hoisted(() => ({ connect: vi.fn(), createOwner: vi.fn() }));
vi.mock("./session", () => ({ connect, createOwner, closeOwner: vi.fn(), CLOSE_UNCONFIRMED: "종료 확인 불가" }));
vi.mock("../remoteDesktop/RemoteDesktopView", () => ({ RemoteDesktopView: () => null }));

let root: Root | undefined;
afterEach(() => {
  act(() => root?.unmount());
  root = undefined;
  vi.clearAllMocks();
});

it("저장 프로필의 직접 TLS 주소를 미리 채우고 연결은 사용자가 시작한다", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
  await act(async () => { root!.render(<VncView active preset={{ host: "vnc.example.test", port: 5901 }} />); });
  expect(host.querySelector<HTMLInputElement>('input[name="host"]')?.value).toBe("vnc.example.test");
  expect(host.querySelector<HTMLInputElement>('input[name="port"]')?.value).toBe("5901");
  expect(createOwner).not.toHaveBeenCalled();
  expect(connect).not.toHaveBeenCalled();
  vi.unstubAllGlobals();
});

it("저장 프로필로 연 탭은 StrictMode에서도 한 번만 바로 연결한다", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  createOwner.mockResolvedValue("owner");
  connect.mockReturnValue(() => { });
  const host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
  await act(async () => { root!.render(<StrictMode><VncView active autoStart preset={{ host: "vnc.example.test", port: 5901 }} /></StrictMode>); });
  expect(createOwner).toHaveBeenCalledOnce();
  expect(connect).toHaveBeenCalledOnce();
  expect(connect.mock.calls[0]?.slice(0, 2)).toEqual(["owner", { host: "vnc.example.test", port: 5901, password: true }]);
  vi.unstubAllGlobals();
});
