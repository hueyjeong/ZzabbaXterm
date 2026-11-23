// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { SshAuthFields, SshMethodFields } from "./SshFields";
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

it("전송 방식에서 SSH-over-SSM을 고르면 SSM 대상 선택을 기다리는 전송으로 바꾼다", () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const edit = vi.fn();
  act(() => root.render(<SshMethodFields ssh={ssh} edit={edit} />));

  const option = host.querySelector<HTMLOptionElement>('option[value="ssm"]');
  expect(option?.disabled).toBe(false);
  expect(option?.textContent).toBe("SSH-over-SSM");
  const select = option!.parentElement as HTMLSelectElement;
  act(() => {
    select.value = "ssm";
    select.dispatchEvent(new Event("change", { bubbles: true }));
  });
  expect(edit).toHaveBeenCalledWith({ transport: { kind: "ssm" } });
  act(() => root.unmount());
});
