// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import type { SavedConnections } from "../connections/connectionIpc";
import type { SshAuth, SshTransport } from "../connections/profiles";
import { LocalTree } from "./LocalTree";

it("직접 SSH의 Agent와 키 파일에만 관리자 확인을 거치는 nano 연동을 제공한다", () => {
  const ssh = (displayName: string, auth: SshAuth, transport: SshTransport = { kind: "direct" }) => ({
    revision: 1,
    profile: {
      id: displayName, displayName, groupId: "ssh", defaultMethod: "ssh" as const,
      ssh: { host: "server", port: 22, account: "developer", hostKeyFingerprint: "SHA256:host", auth, transport },
    },
  });
  const agent = { kind: "agent", publicKeyPath: "/keys/client.pub" } as const;
  const direct = ssh("Agent 서버", agent);
  const profiles: SavedConnections = {
    version: 1, groups: [{ revision: 1, profile: { id: "ssh", displayName: "서버" } }],
    targets: [direct, ssh("키 서버", { kind: "keyFile", privateKeyPath: "/keys/client" }),
      ssh("수동 암호 서버", { kind: "manualPassword" }), ssh("저장 암호 서버", { kind: "storedPassword", itemId: "item" }),
      ssh("SSM 경유 서버", agent, { kind: "ssm", profile: "test", region: "us-east-1", instanceId: "i-test" })],
  };
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const onOpen = vi.fn();
  const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  try {
    act(() => root.render(<LocalTree profiles={profiles} onOpen={onOpen} onRegisterSshHost={vi.fn()} onOpenTunnel={vi.fn()} />));
    const row = (name: string) => {
      const match = [...host.querySelectorAll<HTMLLIElement>("li[role=treeitem]")].find(item => item.textContent === name);
      if (!match) throw new Error(`대상 행 없음: ${name}`);
      return match;
    };
    const menuItem = (name: string) => [...host.querySelectorAll<HTMLButtonElement>("button[role=menuitem]")]
      .find(button => button.textContent === name);
    const showMenu = (name: string) => act(() => {
      row(name).dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
    });
    for (const name of ["내 PC", "수동 암호 서버", "저장 암호 서버", "SSM 경유 서버"]) {
      showMenu(name);
      expect(menuItem("nano 연동으로 열기")).toBeUndefined();
    }
    showMenu("키 서버");
    expect(menuItem("nano 연동으로 열기")).toBeDefined();

    act(() => { row("Agent 서버").dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })); });
    act(() => { row("Agent 서버").dispatchEvent(new MouseEvent("dblclick", { bubbles: true })); });
    showMenu("Agent 서버");
    act(() => menuItem("SSH")?.click());
    expect(onOpen.mock.calls).toEqual([[direct.profile], [direct.profile], [direct.profile]]);
    expect(confirm).not.toHaveBeenCalled();

    showMenu("Agent 서버");
    act(() => menuItem("nano 연동으로 열기")?.click());
    expect(confirm).toHaveBeenCalledWith(expect.stringContaining("관리자가 SSH 셸과 SFTP에서 같은 경로가 같은 파일"));
    expect(onOpen).toHaveBeenCalledTimes(3);
    confirm.mockReturnValue(true);
    showMenu("Agent 서버");
    act(() => menuItem("nano 연동으로 열기")?.click());
    expect(onOpen).toHaveBeenCalledTimes(4);
    expect(onOpen).toHaveBeenLastCalledWith(direct.profile, { remoteEditor: true });
  } finally {
    act(() => root.unmount());
    host.remove();
    confirm.mockRestore();
    vi.unstubAllGlobals();
  }
});
