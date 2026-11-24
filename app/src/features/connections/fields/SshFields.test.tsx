// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { SshFields } from "./SshFields";
import type { SshInput } from "../profiles";

const ssh: SshInput = {
  host: "dev.example.invalid",
  port: "2222",
  account: "deploy",
  hostKeyFingerprint: `SHA256:${"A".repeat(43)}`,
  transport: { kind: "direct" },
  auth: null,
};

function mount() {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const host = document.body.appendChild(document.createElement("div"));
  return { host, root: createRoot(host) };
}

it("저장 인증 가용 여부에 따라 저장 암호 선택을 막는다", () => {
  const { host, root } = mount();
  const render = (storedAuth: boolean) => act(() => {
    root.render(<SshFields ssh={ssh} edit={vi.fn()} storedAuth={storedAuth} error={() => undefined} touch={vi.fn()} />);
  });

  render(false);
  expect(host.querySelector<HTMLOptionElement>('option[value="storedPassword"]')?.disabled).toBe(true);
  render(true);
  expect(host.querySelector<HTMLOptionElement>('option[value="storedPassword"]')?.disabled).toBe(false);
  act(() => root.unmount());
});

it("연결 경로에서 AWS SSM 경유를 고르면 SSM 대상 선택을 기다리는 전송으로 바꾸고 노드 선택을 경로 아래에 둔다", () => {
  const { host, root } = mount();
  const edit = vi.fn();
  act(() => root.render(<SshFields ssh={ssh} edit={edit} storedAuth={false} error={() => undefined} touch={vi.fn()}
    ssm={<div data-testid="ssm" />} />));

  const labels = [...host.querySelectorAll("label")].map(label => label.firstChild?.textContent?.trim());
  expect(labels).toEqual(["호스트", "포트", "계정", "연결 경로", "인증 방식", "호스트 키 SHA256 지문 (선택)"]);
  const option = host.querySelector<HTMLOptionElement>('option[value="ssm"]');
  expect([...option!.parentElement!.querySelectorAll("option")].map(item => item.textContent)).toEqual(["직접 연결", "AWS SSM 경유"]);
  expect(host.querySelector('[data-testid="ssm"]')).toBeNull();
  const select = option!.parentElement as HTMLSelectElement;
  act(() => {
    select.value = "ssm";
    select.dispatchEvent(new Event("change", { bubbles: true }));
  });
  expect(edit).toHaveBeenCalledWith({ transport: { kind: "ssm" } });
  act(() => root.render(<SshFields ssh={{ ...ssh, transport: { kind: "ssm" } }} edit={edit} storedAuth={false}
    error={field => field === "ssh.hostKeyFingerprint" ? "지문 형식 오류" : undefined} touch={vi.fn()} ssm={<div data-testid="ssm" />} />));
  expect(host.querySelector('[data-testid="ssm"]')).not.toBeNull();
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("지문 형식 오류");
  act(() => root.unmount());
});
