// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import type { SavedConnections } from "../connections/connectionIpc";
import type { SshAuth, SshTransport } from "../connections/profiles";
import { LocalTree } from "./LocalTree";

it("직접 SSH의 Agent와 키 파일에만 네이티브 nano 연동 승인을 요청한다", () => {
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
    version: 2, groups: [{ revision: 1, profile: { id: "ssh", displayName: "서버" } }],
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

    showMenu("Agent 서버");
    act(() => menuItem("nano 연동으로 열기")?.click());
    expect(confirm).not.toHaveBeenCalled();
    expect(onOpen).toHaveBeenCalledTimes(4);
    expect(onOpen).toHaveBeenLastCalledWith(direct.profile, { remoteEditor: true });
  } finally {
    act(() => root.unmount());
    host.remove();
    confirm.mockRestore();
    vi.unstubAllGlobals();
  }
});


it("승격 메뉴는 저장 인증 기반 명령이 준비된 경우에만 표시한다", () => {
  const profiles: SavedConnections = {
    version: 2, groups: [{ revision: 1, profile: { id: "ssh", displayName: "서버" } }],
    targets: [{
      revision: 1,
      profile: {
        id: "agent", displayName: "Agent 서버", groupId: "ssh", defaultMethod: "ssh",
        ssh: {
          host: "server", port: 22, account: "developer", hostKeyFingerprint: "SHA256:host",
          auth: { kind: "agent", publicKeyPath: "/keys/client.pub" }, transport: { kind: "direct" },
        },
      },
    }],
  };
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  try {
    act(() => root.render(<LocalTree profiles={profiles} onOpen={vi.fn()} onRegisterSshHost={vi.fn()} onOpenTunnel={vi.fn()} />));
    const row = (name: string) => {
      const match = [...host.querySelectorAll<HTMLLIElement>("li[role=treeitem]")].find(item => item.textContent === name);
      if (!match) throw new Error(`대상 행 없음: ${name}`);
      return match;
    };
    const menuItem = (name: string) => [...host.querySelectorAll<HTMLButtonElement>("button[role=menuitem]")]
      .find(button => button.textContent === name);
    for (const name of ["내 PC", "Agent 서버"]) {
      act(() => { row(name).dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true })); });
      expect(menuItem("승격 실행…")).toBeUndefined();
    }
    act(() => root.render(<LocalTree profiles={profiles} onOpen={vi.fn()} onLocalElevate={vi.fn()} onElevate={vi.fn()} onRegisterSshHost={vi.fn()} onOpenTunnel={vi.fn()} />));
    for (const name of ["내 PC", "Agent 서버"]) {
      act(() => { row(name).dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true })); });
      expect(menuItem("승격 실행…")).toBeDefined();
    }
  } finally {
    act(() => root.unmount());
    host.remove();
    vi.unstubAllGlobals();
  }
});

it("그룹 없는 세션은 그룹 뒤 최상위에, 상위 그룹이 있는 그룹은 아직 최상위에 표시한다", () => {
  const local = (id: string, displayName: string, groupId?: string) => ({
    revision: 1,
    profile: { id, displayName, ...(groupId ? { groupId } : {}), defaultMethod: "local" as const, local: { shell: "powerShell" as const, startDirectory: "C:\\work" } },
  });
  const top = local("top", "최상위 셸");
  const profiles: SavedConnections = {
    version: 2,
    groups: [{ revision: 1, profile: { id: "work", displayName: "작업" } }, { revision: 1, profile: { id: "child", displayName: "하위", parentId: "work" } }],
    targets: [top, local("nested", "하위 셸", "child")],
  };
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const onOpen = vi.fn();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  try {
    act(() => root.render(<LocalTree profiles={profiles} onOpen={onOpen} onRegisterSshHost={vi.fn()} onOpenTunnel={vi.fn()} />));
    const tree = host.querySelector("ul[role=tree]")!;
    const rows = [...tree.children].map((item) => item.querySelector(".tree-row")?.textContent);
    expect(rows).toEqual(["로컬", "작업", "하위", "최상위 셸"]);
    const row = [...tree.children].find((item) => item.textContent === "최상위 셸")!;
    act(() => { row.dispatchEvent(new MouseEvent("dblclick", { bubbles: true })); });
    expect(onOpen).toHaveBeenCalledWith(top.profile);

    const search = host.querySelector<HTMLInputElement>("input[type=search]")!;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(search, "최상위");
      search.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect([...tree.children].map((item) => item.querySelector(".tree-row")?.textContent)).toEqual(["최상위 셸"]);
    expect(host.querySelector("[role=status]")).toBeNull();
  } finally {
    act(() => root.unmount());
    host.remove();
    vi.unstubAllGlobals();
  }
});
