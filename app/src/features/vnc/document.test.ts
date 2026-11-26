// @vitest-environment jsdom

import { afterEach, expect, it, vi } from "vitest";
import { invokeVnc, readPacket } from "./document";

const invoke = vi.hoisted(() => vi.fn(async () => { }));
vi.mock("@tauri-apps/api/core", () => ({ invoke, convertFileSrc: (file: string) => `/packet/${file}` }));
afterEach(() => vi.unstubAllGlobals());

it("교체된 문서의 요청과 이미 회수 중인 프레임을 폐기한다", async () => {
  const old = "a".repeat(32);
  const ticket = "b".repeat(32);
  vi.stubGlobal("__ZXT_VNC_DOCUMENT__", { proof: old });
  const { promise, resolve } = Promise.withResolvers<Response>();
  const fetch = vi.fn(() => promise);
  vi.stubGlobal("fetch", fetch);
  await invokeVnc("vnc_input", {}, old);
  expect(invoke).toHaveBeenCalledWith("vnc_input", {}, { headers: { "x-zxt-vnc-document": old } });
  const chunk = readPacket(old, ticket, new AbortController().signal);
  vi.stubGlobal("__ZXT_VNC_DOCUMENT__", { proof: "c".repeat(32) });
  resolve(new Response(new Uint8Array([1]), { headers: { "content-type": "application/octet-stream" } }));
  await expect(chunk).rejects.toThrow();
  await expect(invokeVnc("vnc_input", {}, old)).rejects.toBeTruthy();
  await expect(readPacket(old, ticket, new AbortController().signal)).rejects.toThrow();
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(invoke).toHaveBeenCalledTimes(1);
});
