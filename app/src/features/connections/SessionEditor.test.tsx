// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { SavedConnections } from "./connectionIpc";
import type { ConnectionTestDraft, SavedTarget } from "./profiles";
import { SessionEditor, type SessionEditorProps } from "./SessionEditor.tsx";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("../terminal/TerminalView", () => ({ TerminalView: () => null }));
vi.mock("../ssm/ssmDiscovery", () => ({
  createSsmDiscovery: () => ({
    listProfiles: async () => ({ ok: true, value: ["company-sso"] }),
    configuredRegion: async () => ({ ok: true, value: "ap-northeast-2" }),
    listOnlineNodes: async () => ({ ok: true, value: [{ instanceId: "i-0123456789abcdef0", name: "web" }] }),
    relogin: async () => ({ ok: true, value: undefined }),
  }),
}));

const node = { profile: "company-sso", region: "ap-northeast-2", instanceId: "i-0123456789abcdef0" };
const sshTarget = (id: string, displayName: string, auth: { kind: "agent"; publicKeyPath: string } | { kind: "manualPassword" },
  transport: { kind: "direct" } | ({ kind: "ssm" } & typeof node) = { kind: "direct" }) => ({
  revision: 1, profile: { id, groupId: "work", displayName, defaultMethod: "ssh" as const, ssh: {
    host: "dev.example.test", port: 22, account: "deploy", transport, auth,
  } } satisfies SavedTarget,
});
const groups: SavedConnections["groups"] = [
  { revision: 1, profile: { id: "work", displayName: "회사" } },
  { revision: 1, profile: { id: "dev", displayName: "개발", parentId: "work" } },
  { revision: 1, profile: { id: "db", displayName: "DB", parentId: "dev" } },
  { revision: 1, profile: { id: "home", displayName: "집" } },
];

let host: HTMLDivElement;
let root: Root;
const actions = {
  test: vi.fn(async (attempt: ConnectionTestDraft) => ({ draftId: attempt.draftId, draftVersion: attempt.draftVersion, runId: "run" })),
  cancelTest: vi.fn(async () => { }),
  save: vi.fn(async () => { }),
  saveTarget: vi.fn(async () => ({ revision: 1 })),
  onClose: vi.fn(),
};

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

async function render(props: Partial<SessionEditorProps> = {}) {
  await act(async () => root.render(<SessionEditor request={{}} groups={groups} {...actions} {...props} />));
}
const button = (name: string) => [...host.querySelectorAll("button")].find(item => (item.getAttribute("aria-label") ?? item.textContent) === name);
const press = (name: string) => act(async () => { button(name)!.click(); });
const labels = () => [...host.querySelectorAll(".editor-method label, .editor-method .editor-field > span")]
  .map(item => (item.firstChild?.textContent || item.textContent)?.trim());
const control = (name: string) => [...host.querySelectorAll("label")].find(item => item.firstChild?.textContent?.trim() === name)
  ?.querySelector<HTMLInputElement | HTMLSelectElement>("input, select") ?? undefined;
const errorOf = (name: string) => control(name)?.closest(".editor-field")?.querySelector('[role="alert"]')?.textContent;
const options = (name: string) => [...(control(name) as HTMLSelectElement).options].map(option => option.textContent);
async function type(name: string, value: string) {
  const input = control(name)!;
  await act(async () => {
    const prototype = input instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(input, value);
    input.dispatchEvent(new Event(input instanceof HTMLSelectElement ? "change" : "input", { bubbles: true }));
  });
}
async function blur(name: string) {
  await act(async () => { control(name)!.dispatchEvent(new FocusEvent("focusout", { bubbles: true })); });
}
const method = (label: string) => act(async () => {
  [...host.querySelectorAll<HTMLButtonElement>('[role="radio"]')].find(item => item.textContent === label)!.click();
});
async function chooseNode() {
  await type("프로필", node.profile);
  await vi.waitFor(() => expect(options("온라인 노드")).toContain(`web (${node.instanceId})`));
  await type("온라인 노드", node.instanceId);
}
const saved = () => (actions.saveTarget.mock.calls as unknown[][])[0][0] as { targetRevision: number | null; target: SavedTarget };

describe("세션 추가", () => {
  it("여섯 방식 모두 한 화면에서 입력해 저장한다", async () => {
    const fill: Record<string, () => Promise<void>> = {
      로컬: () => type("시작 디렉터리", "C:\\work"),
      SSH: async () => {
        await type("호스트", "ssh.example.test");
        await type("계정", "deploy");
        await type("인증 방식", "keyFile");
        await type("개인 키 파일 경로", "C:\\keys\\id");
      },
      "AWS SSM": chooseNode,
      S3: async () => {
        await type("IAM Identity Center 프로필", "company");
        await type("버킷 리전", "ap-northeast-2");
        await type("버킷", "company-dev");
      },
      RDP: () => type("호스트", "win.example.test"),
      VNC: () => type("호스트", "vnc.example.test"),
    };
    const expected = ["local", "ssh", "ssm", "s3", "rdp", "vnc"];
    for (const [index, label] of Object.keys(fill).entries()) {
      await render({ request: { groupId: "dev" } });
      expect(host.querySelector("h2")?.textContent).toBe("세션 추가");
      await type("이름", `세션 ${label}`);
      await method(label);
      expect(button("저장")!.disabled).toBe(label !== "로컬");
      await fill[label]();
      expect(button("저장")!.disabled, label).toBe(false);
      await press("저장");
      expect(saved()).toMatchObject({ targetRevision: null, target: { groupId: "dev", displayName: `세션 ${label}`, defaultMethod: expected[index] } });
      expect(actions.onClose).toHaveBeenCalledWith(expect.objectContaining({ kind: "saved" }));
      await act(async () => root.unmount());
      root = createRoot(host);
      vi.clearAllMocks();
    }
  });

  it("방식을 바꾸면 그 방식의 필드만 표시하고 방향키로 방식을 고른다", async () => {
    await render();
    const radios = () => [...host.querySelectorAll<HTMLButtonElement>('[role="radiogroup"][aria-label="접속 방식"] [role="radio"]')];
    expect(radios().map(item => item.textContent)).toEqual(["로컬", "SSH", "AWS SSM", "S3", "RDP", "VNC"]);
    expect(labels()).toEqual(["셸", "시작 디렉터리"]);
    await method("SSH");
    expect(labels()).toEqual(["호스트", "포트", "계정", "연결 경로", "인증 방식", "호스트 키 SHA256 지문 (선택)"]);
    await type("연결 경로", "ssm");
    expect(host.querySelector(".editor-method legend")?.textContent).toBe("AWS Systems Manager");
    await method("S3");
    expect(labels()).toEqual(["IAM Identity Center 프로필", "버킷 리전", "버킷", "접두어 (선택)"]);
    await method("RDP");
    expect(labels()).toEqual(["연결 경로", "호스트", "포트", "TLS 전용 허용"]);
    await act(async () => { radios()[4].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true })); });
    expect(radios()[5].getAttribute("aria-checked")).toBe("true");
    expect(document.activeElement).toBe(radios()[5]);
    expect(labels()).toEqual(["연결 경로", "호스트", "포트"]);
  });

  it("RDP 연결 경로 문구와 SSH 세션 후보를 표시한다", async () => {
    await render({ targets: [
      sshTarget("key-ssh", "키 SSH", { kind: "agent", publicKeyPath: "C:\\keys\\id.pub" }),
      sshTarget("pw-ssh", "암호 SSH", { kind: "manualPassword" }),
      sshTarget("ssm-ssh", "SSM SSH", { kind: "agent", publicKeyPath: "C:\\keys\\id.pub" }, { kind: "ssm", ...node }),
    ] });
    await method("RDP");
    expect(options("연결 경로")).toEqual(["직접 연결", "SSH 터널 경유", "AWS SSM 경유"]);
    expect(button("연결 시험")).toBeUndefined();
    await type("연결 경로", "ssh");
    expect(options("SSH 세션")).toEqual(["SSH 세션 선택", "키 SSH"]);
    expect(control("SSH 세션")!.value).toBe("key-ssh");
    await type("이름", "윈도");
    await type("호스트", "10.0.1.25");
    await press("저장");
    expect(saved().target).toMatchObject({ rdp: { host: "10.0.1.25", port: 3389, tlsOnly: false, route: { kind: "ssh", targetId: "key-ssh" } } });
  });

  it("RDP 직접 연결을 IP 주소로 저장하고 VNC 직접 연결은 호스트와 포트로 표시한다", async () => {
    await render();
    await type("이름", "윈도");
    await method("RDP");
    await type("호스트", "10.0.1.25");
    await press("저장");
    expect(saved().target).toEqual({ id: expect.any(String), displayName: "윈도", defaultMethod: "rdp",
      rdp: { host: "10.0.1.25", port: 3389, tlsOnly: false, route: { kind: "direct" } } });
    await act(async () => root.unmount());
    root = createRoot(host);
    await render();
    await method("VNC");
    expect(labels()).toEqual(["연결 경로", "호스트", "포트"]);
    expect(control("포트")!.value).toBe("5900");
    expect(button("연결 시험")).toBeUndefined();
  });

  it("그룹 선택에 중첩 그룹을 들여 써서 표시하고 최상위를 맨 위에 둔다", async () => {
    await render({ request: { groupId: "db" } });
    expect(options("그룹")).toEqual(["최상위", "회사", "\u00a0\u00a0\u00a0개발", "\u00a0".repeat(6) + "DB", "집"]);
    expect(control("그룹")!.value).toBe("db");
    await type("그룹", "");
    await type("이름", "셸");
    await press("저장");
    expect(saved().target).not.toHaveProperty("groupId");
  });

  it("빈 필수 입력은 건드린 뒤에, 형식 오류는 바로 필드 아래에 표시한다", async () => {
    await render();
    expect(host.querySelectorAll('[role="alert"]')).toHaveLength(0);
    await blur("이름");
    expect(errorOf("이름")).toBe("필수 입력");
    await method("RDP");
    expect(errorOf("호스트")).toBeUndefined();
    await type("호스트", "127.1");
    expect(errorOf("호스트")).toBe("호스트 형식 오류");
    await type("포트", "0");
    expect(errorOf("포트")).toBe("포트 형식 오류");
    expect(button("저장")!.disabled).toBe(true);
  });

  it("Rust 검증 오류를 해당 필드 아래에, 충돌을 상태 줄에 표시하고 입력을 유지한다", async () => {
    actions.saveTarget.mockRejectedValueOnce({ kind: "invalid", field: "desktop.host" }).mockRejectedValueOnce({ kind: "conflict" });
    await render();
    await type("이름", "윈도");
    await method("RDP");
    await type("호스트", "win.example.test");
    await press("저장");
    expect(errorOf("호스트")).toBe("호스트 형식 오류");
    await press("저장");
    expect(errorOf("호스트")).toBeUndefined();
    expect(host.querySelector(".editor-status [role=\"alert\"]")?.textContent).toBe("다른 작업이 세션을 변경했습니다");
    expect(control("호스트")!.value).toBe("win.example.test");
    expect(actions.onClose).not.toHaveBeenCalled();
  });
});

describe("세션 편집", () => {
  it("저장된 세션을 채워 열고 개정과 함께 저장한다", async () => {
    const target = sshTarget("server", "서버", { kind: "agent", publicKeyPath: "C:\\keys\\id.pub" });
    await render({ request: { target: target.profile, revision: 7 } });
    expect(host.querySelector("h2")?.textContent).toBe("세션 편집");
    expect(control("이름")!.value).toBe("서버");
    expect(control("그룹")!.value).toBe("work");
    expect(control("포트")!.value).toBe("22");
    expect(control("Agent 공개 키 파일 경로")!.value).toBe("C:\\keys\\id.pub");
    await type("포트", "2222");
    await press("저장");
    expect(actions.saveTarget).toHaveBeenCalledWith({ version: 2, targetRevision: 7, target: { ...target.profile, ssh: { ...target.profile.ssh, port: 2222 } } });
  });

  it("시험에 성공하면 티켓으로 저장하고 시험 중 표시와 성공을 상태 줄에 둔다", async () => {
    const target = { id: "bucket", displayName: "버킷", defaultMethod: "s3",
      s3: { profile: "company", region: "ap-northeast-2", bucket: "dev", prefix: "" } } satisfies SavedTarget;
    await render({ request: { target, revision: 2 } });
    await press("연결 시험");
    expect(host.querySelector('.editor-status [role="status"]')?.textContent).toBe("시험 성공");
    await press("저장");
    expect(actions.save).toHaveBeenCalledWith({ run: { draftId: expect.any(String), draftVersion: 0, runId: "run" },
      draft: { version: 2, targetRevision: 2, target } });
    expect(actions.saveTarget).not.toHaveBeenCalled();
  });

  it("저장된 SSM 노드를 표시하고 닫기 아이콘은 취소와 같다", async () => {
    await render({ request: { target: { id: "node", displayName: "노드", defaultMethod: "ssm", ssm: node }, revision: 1 } });
    expect(host.textContent).toContain(`${node.profile} / ${node.region} / ${node.instanceId}`);
    expect(button("저장")!.disabled).toBe(false);
    await press("닫기");
    await vi.waitFor(() => expect(actions.onClose).toHaveBeenCalledWith({ kind: "cancelled", draftId: expect.any(String) }));
  });
});
