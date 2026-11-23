// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { LocalConnectionWizard } from "./LocalConnectionWizard";
import type { LocalProfileDraft } from "./profiles";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("../terminal/TerminalView", () => ({ TerminalView: () => null }));

const draft: LocalProfileDraft = {
  version: 1, groupRevision: null, targetRevision: null,
  group: { id: "work", displayName: "개발" },
  target: { id: "new-target", groupId: "work", displayName: "원격 화면", defaultMethod: "local", local: { shell: "powerShell", startDirectory: "C:\\work" } },
};
const sshTarget = (id: string, displayName: string, auth: { kind: "agent"; publicKeyPath: string } | { kind: "manualPassword" }) => ({
  revision: 1, profile: { id, groupId: "work", displayName, defaultMethod: "ssh" as const, ssh: {
    host: "dev.example.test", port: 22, account: "deploy", hostKeyFingerprint: `SHA256:${"A".repeat(43)}`, transport: { kind: "direct" as const }, auth,
  } },
});

let host: HTMLDivElement;
let root: Root;
const actions = { test: vi.fn(), cancelTest: vi.fn(), save: vi.fn(async () => { }), onClose: vi.fn() };

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  Object.assign(HTMLDialogElement.prototype, {
    showModal(this: HTMLDialogElement) { this.setAttribute("open", ""); },
    close(this: HTMLDialogElement) { this.removeAttribute("open"); },
  });
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

const button = (name: string) => [...host.querySelectorAll("button")].find(item => (item.getAttribute("aria-label") ?? item.textContent) === name);
const press = (name: string) => act(async () => { button(name)!.click(); });
const radios = () => [...host.querySelectorAll<HTMLButtonElement>('[role="radiogroup"][aria-label="접속 방식"] [role="radio"]')];
const current = () => host.querySelector('[aria-current="step"]')?.textContent;

it("머리·단계 표시·방식 타일·하단 단추 줄로 RDP 프로필을 만든다", async () => {
  await act(async () => root.render(<LocalConnectionWizard draftId="ui" initialDraft={draft} groups={[]} {...actions}
    targets={[sshTarget("key-ssh", "키 SSH", { kind: "agent", publicKeyPath: "C:\\keys\\id.pub" }), sshTarget("pw-ssh", "암호 SSH", { kind: "manualPassword" })]} />));
  expect(host.querySelector("h2")?.textContent).toBe("새 연결");
  expect([...host.querySelectorAll(".wizard-steps li")].map(item => item.textContent)).toEqual(["대상", "접속 방식", "인증", "확인"]);
  expect(current()).toBe("대상");
  expect([...host.querySelectorAll(".wizard-footer button")].map(item => item.textContent)).toEqual(["다음", "취소"]);
  await press("다음");
  expect(current()).toBe("접속 방식");
  expect(radios().map(item => item.textContent)).toEqual(["로컬", "SSH", "AWS SSM", "S3", "RDP", "VNC"]);
  expect(radios().map(item => item.querySelector("[data-kind]")?.getAttribute("data-kind"))).toEqual(["local", "ssh", "ssm", "s3", "rdp", "vnc"]);
  expect(radios().map(item => item.tabIndex)).toEqual([0, -1, -1, -1, -1, -1]);
  await act(async () => { radios()[0].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true })); });
  expect(radios()[5].getAttribute("aria-checked")).toBe("true");
  expect(document.activeElement).toBe(radios()[5]);
  await act(async () => { radios()[5].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true })); });
  const rdp = radios()[4];
  expect(rdp.getAttribute("aria-checked")).toBe("true");
  expect(rdp.tabIndex).toBe(0);
  const route = [...host.querySelectorAll("label")].find(item => item.firstChild?.textContent?.trim() === "경로")!.querySelector("select")!;
  await act(async () => {
    route.value = "ssh";
    route.dispatchEvent(new Event("change", { bubbles: true }));
  });
  const name = [...host.querySelectorAll("label")].find(item => item.firstChild?.textContent?.trim() === "TLS 인증 DNS")!.querySelector("input")!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(name, "xrdp.example.test");
    name.dispatchEvent(new Event("input", { bubbles: true }));
  });
  expect(host.querySelector<HTMLInputElement>('input[inputmode="numeric"]')?.value).toBe("3389");
  await press("다음");
  expect(current()).toBe("인증");
  const target = [...host.querySelectorAll("label")].find(item => item.firstChild?.textContent?.trim() === "SSH 대상")!.querySelector("select")!;
  expect([...target.options].map(option => option.textContent)).toEqual(["SSH 대상 선택", "키 SSH"]);
  expect(target.value).toBe("key-ssh");
  expect([...host.querySelectorAll(".wizard-footer button")].map(item => item.textContent)).toEqual(["이전", "다음", "취소"]);
  await press("다음");
  expect(current()).toBe("확인");
  expect(button("연결 테스트")).toBeUndefined();
  expect(host.querySelector('[aria-label="RDP 연결 대상"]')?.textContent).toContain("키 SSH");
  await press("저장");
  expect(actions.save).toHaveBeenCalledWith({ run: expect.anything(), draft: expect.objectContaining({ target: expect.objectContaining({
    defaultMethod: "rdp", rdp: { host: "xrdp.example.test", port: 3389, tlsOnly: false, route: { kind: "ssh", targetId: "key-ssh" } },
  }) }) });
});

it("닫기 아이콘은 취소와 같은 동작이고 편집에서는 대상 이름을 제목으로 쓴다", async () => {
  await act(async () => root.render(<LocalConnectionWizard draftId="edit" groups={[]} {...actions}
    initialDraft={{ ...draft, groupRevision: 1, targetRevision: 3, target: { ...draft.target, displayName: "작업 셸" } }} />));
  expect(host.querySelector("h2")?.textContent).toBe("작업 셸");
  await press("닫기");
  await vi.waitFor(() => expect(actions.onClose).toHaveBeenCalledWith({ kind: "cancelled", draftId: "edit" }));
});
