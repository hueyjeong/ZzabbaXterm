// @vitest-environment jsdom
import { act, createRef, StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import type { S3Location } from "./s3";
import { StorageTab } from "./StorageTab";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

it("고정된 저장 ID를 한 번 열고 Rust가 반환한 위치를 숨김 전환 뒤에도 유지한다", async () => {
  const location: S3Location = { bucket: "approved-bucket", prefix: " 한글// ", region: "eu-west-1" };
  const targetId = "registered-s3";
  invoke.mockImplementation(async (command: string, { request }: { request: { id?: string } }) => {
    switch (command) {
      case "storage_open":
        return { ok: true, value: { sessionId: request.id, profile: "company", region: location.region, endpoint: "https://s3.eu-west-1.amazonaws.com", location } };
      case "storage_list_objects": return { ok: true, value: { rows: [], nextToken: null } };
      case "storage_count_previous_files": return { ok: true, value: 0 };
      case "storage_close": return true;
      default: throw new Error(command);
    }
  });
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const closeRef = createRef<{ confirmClose(): Promise<void> }>();
  const render = (active: boolean) => act(async () => root.render(<StrictMode>
    <div hidden={!active}><StorageTab targetId={targetId} active={active} closing={false} closeRef={closeRef} onRetest={vi.fn()} /></div>
  </StrictMode>));
  try {
    await render(true);
    const sessionId = invoke.mock.calls[0]?.[1].request.id;
    expect(sessionId).toEqual(expect.any(String));
    expect(invoke.mock.calls.filter(([command]) => command === "storage_open")).toEqual([
      ["storage_open", { request: { targetId, id: sessionId } }],
    ]);
    expect(invoke).toHaveBeenCalledWith("storage_list_objects", { request: { sessionId, ...location, token: null } });
    expect(host.querySelector<HTMLInputElement>("label:nth-child(2) input")?.value).toBe(location.prefix);
    await render(false);
    await render(true);
    expect(invoke.mock.calls.filter(([command]) => command === "storage_open" || command === "storage_list_objects")).toHaveLength(2);
    expect(invoke.mock.calls.some(([command]) => command === "storage_close")).toBe(false);
    await act(async () => closeRef.current?.confirmClose());
    expect(invoke).toHaveBeenCalledWith("storage_close", { request: { sessionId } });
  } finally {
    await act(async () => root.unmount());
    host.remove();
    vi.unstubAllGlobals();
  }
});
