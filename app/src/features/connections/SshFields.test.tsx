// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { SshAuthFields } from "./SshFields";
import type { SshInput } from "./profiles";

const ssh: SshInput = {
  host: "dev.example.invalid",
  port: "2222",
  account: "deploy",
  hostKeyFingerprint: `SHA256:${"A".repeat(43)}`,
  transport: { kind: "direct" },
  auth: null,
};

it("저장 인증 가용 여부에 따라 저장 암호 선택을 막는다", () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const render = (storedAuth: boolean) => act(() => {
    root.render(<SshAuthFields ssh={ssh} edit={vi.fn()} storedAuth={storedAuth} />);
  });

  render(false);
  expect(host.querySelector<HTMLOptionElement>('option[value="storedPassword"]')?.disabled).toBe(true);
  render(true);
  expect(host.querySelector<HTMLOptionElement>('option[value="storedPassword"]')?.disabled).toBe(false);
  act(() => root.unmount());
});
