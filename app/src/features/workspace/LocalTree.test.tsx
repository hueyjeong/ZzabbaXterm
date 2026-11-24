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

const local = (id: string, displayName: string, groupId?: string) => ({
  revision: 1,
  profile: { id, displayName, ...(groupId ? { groupId } : {}), defaultMethod: "local" as const, local: { shell: "powerShell" as const, startDirectory: "C:\\work" } },
});
const nested: SavedConnections = {
  version: 2,
  groups: [
    { revision: 1, profile: { id: "work", displayName: "작업" } },
    { revision: 1, profile: { id: "child", displayName: "하위", parentId: "work" } },
    { revision: 1, profile: { id: "grand", displayName: "손자", parentId: "child" } },
    { revision: 1, profile: { id: "other", displayName: "기타" } },
  ],
  targets: [local("top", "최상위 셸"), local("deep", "깊은 셸", "grand"), local("shell", "작업 셸", "work")],
};

/** 중첩 트리를 그린다. 시험이 끝나면 unmount를 호출한다. */
function renderTree(props: Partial<Parameters<typeof LocalTree>[0]> = {}) {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const handlers = {
    onOpen: vi.fn(), onRegisterSshHost: vi.fn(), onOpenTunnel: vi.fn(), onEdit: vi.fn(), onDelete: vi.fn(), onDuplicate: vi.fn(),
    onAddSession: vi.fn(), onAddGroup: vi.fn(), onEditGroup: vi.fn(), onDeleteGroup: vi.fn(),
  };
  const render = (extra: Partial<Parameters<typeof LocalTree>[0]> = {}) =>
    act(() => root.render(<LocalTree profiles={nested} {...handlers} {...props} {...extra} />));
  render();
  const tree = host.querySelector<HTMLUListElement>("ul[role=tree]")!;
  const name = (item: Element) => item.querySelector(":scope > .tree-row > span")?.textContent;
  const row = (label: string) => {
    const match = [...host.querySelectorAll<HTMLLIElement>("li[role=treeitem]")].find(item => name(item) === label);
    if (!match) throw new Error(`행 없음: ${label}`);
    return match;
  };
  const shown = () => [...tree.querySelectorAll("li[role=treeitem]")].filter(item => !item.closest("[hidden]")).map(name);
  const levels = () => Object.fromEntries([...tree.querySelectorAll("li[role=treeitem]")].map(item => [name(item), item.getAttribute("aria-level")]));
  const search = (value: string) => act(() => {
    const input = host.querySelector<HTMLInputElement>("input[type=search]")!;
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  const press = (element: Element, key: string, shiftKey = false) =>
    act(() => { element.dispatchEvent(new KeyboardEvent("keydown", { key, shiftKey, bubbles: true })); });
  const contextMenu = (element: Element) =>
    act(() => { element.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true })); });
  const menu = () => host.querySelector<HTMLElement>("[role=menu]");
  const menuItems = () => [...host.querySelectorAll<HTMLButtonElement>("button[role=menuitem]")].map(item => item.textContent);
  const choose = (label: string) => act(() => {
    const item = [...host.querySelectorAll<HTMLButtonElement>("button[role=menuitem]")].find(entry => entry.textContent === label);
    if (!item) throw new Error(`메뉴 항목 없음: ${label}`);
    item.click();
  });
  const unmount = () => {
    act(() => root.unmount());
    host.remove();
    vi.unstubAllGlobals();
  };
  return { host, tree, handlers, render, row, shown, levels, search, press, contextMenu, menu, menuItems, choose, unmount };
}

it("그룹을 parentId로 중첩해 aria-level과 함께 그리고 그룹 없는 세션은 최상위 그룹 뒤에 둔다", () => {
  const view = renderTree();
  try {
    expect(view.shown()).toEqual(["로컬", "내 PC", "작업", "하위", "손자", "깊은 셸", "작업 셸", "기타", "최상위 셸"]);
    expect(view.levels()).toEqual({
      "로컬": "1", "내 PC": "2", "작업": "1", "하위": "2", "손자": "3", "깊은 셸": "4", "작업 셸": "2", "기타": "1", "최상위 셸": "1",
    });
    expect(view.row("손자").parentElement?.closest("li")).toBe(view.row("하위"));
    expect(view.row("손자").style.getPropertyValue("--tree-level")).toBe("3");
    act(() => { view.row("최상위 셸").dispatchEvent(new MouseEvent("dblclick", { bubbles: true })); });
    expect(view.handlers.onOpen).toHaveBeenCalledWith(nested.targets[0].profile);

    // 그룹 머리를 누르면 접고, 다시 누르면 펼친다. 접힌 하위는 숨긴다.
    act(() => view.row("하위").querySelector<HTMLElement>(".tree-row")!.click());
    expect(view.row("하위").getAttribute("aria-expanded")).toBe("false");
    expect(view.shown()).toEqual(["로컬", "내 PC", "작업", "하위", "작업 셸", "기타", "최상위 셸"]);
    act(() => view.row("하위").querySelector<HTMLElement>(".tree-row")!.click());
    expect(view.shown()).toContain("깊은 셸");
  } finally {
    view.unmount();
  }
});

it("검색하면 일치하는 항목과 조상 그룹만 남기고 이름이 일치한 그룹은 내용을 모두 보인다", () => {
  const view = renderTree();
  try {
    act(() => view.row("하위").querySelector<HTMLElement>(".tree-row")!.click());
    view.search("깊은");
    expect(view.shown()).toEqual(["작업", "하위", "손자", "깊은 셸"]);
    view.search("하위");
    expect(view.shown()).toEqual(["작업", "하위", "손자", "깊은 셸"]);
    view.search("최상위");
    expect(view.shown()).toEqual(["최상위 셸"]);
    expect(view.host.querySelector("[role=status]")).toBeNull();
    view.search("없는 이름");
    expect(view.shown()).toEqual([]);
    expect(view.host.querySelector("[role=status]")?.textContent).toBe("검색 결과 없음");
    // 검색을 지우면 접어 둔 상태가 돌아온다.
    view.search("");
    expect(view.row("하위").getAttribute("aria-expanded")).toBe("false");
  } finally {
    view.unmount();
  }
});

it("중첩 트리에서 화살표·Home·End·Enter·Space·F2·Delete로 이동하고 동작한다", () => {
  const view = renderTree();
  try {
    const focused = () => document.activeElement?.querySelector(":scope > .tree-row > span")?.textContent;
    act(() => view.row("작업").focus());
    view.press(view.row("작업"), "ArrowDown");
    expect(focused()).toBe("하위");
    view.press(view.row("하위"), "ArrowRight");
    expect(focused()).toBe("손자");
    view.press(view.row("손자"), "ArrowLeft");
    expect(view.row("손자").getAttribute("aria-expanded")).toBe("false");
    view.press(view.row("손자"), "ArrowRight");
    expect(view.row("손자").getAttribute("aria-expanded")).toBe("true");
    view.press(view.row("손자"), "ArrowRight");
    expect(focused()).toBe("깊은 셸");
    view.press(view.row("깊은 셸"), "ArrowLeft");
    expect(focused()).toBe("손자");
    view.press(view.row("손자"), " ");
    expect(view.row("손자").getAttribute("aria-expanded")).toBe("false");
    view.press(view.row("손자"), "ArrowLeft");
    expect(focused()).toBe("하위");
    view.press(view.row("하위"), "End");
    expect(focused()).toBe("최상위 셸");
    expect(view.row("최상위 셸").tabIndex).toBe(0);
    view.press(view.row("최상위 셸"), "ArrowUp");
    expect(focused()).toBe("기타");
    view.press(view.row("기타"), "Home");
    expect(focused()).toBe("로컬");
    view.press(view.row("최상위 셸"), "Enter");
    expect(view.handlers.onOpen).toHaveBeenCalledWith(nested.targets[0].profile);

    view.press(view.row("하위"), "F2");
    expect(view.handlers.onEditGroup).toHaveBeenCalledExactlyOnceWith("child");
    view.press(view.row("작업 셸"), "F2");
    expect(view.handlers.onEdit).toHaveBeenCalledExactlyOnceWith("shell");
    view.press(view.row("기타"), "Delete");
    expect(view.handlers.onDeleteGroup).toHaveBeenCalledExactlyOnceWith("other");
    view.press(view.row("작업 셸"), "Delete");
    expect(view.handlers.onDelete).toHaveBeenCalledExactlyOnceWith("shell");
    // 합성 "로컬" 그룹은 이름 바꾸기와 삭제 대상이 아니다.
    view.press(view.row("로컬"), "F2");
    view.press(view.row("로컬"), "Delete");
    expect(view.handlers.onEditGroup).toHaveBeenCalledTimes(1);
    expect(view.handlers.onDeleteGroup).toHaveBeenCalledTimes(1);
  } finally {
    view.unmount();
  }
});

it("빈 곳은 최상위 추가, 그룹은 그 안의 추가·이름 바꾸기·삭제, 세션은 편집·복제·삭제 메뉴를 띄운다", () => {
  const view = renderTree();
  try {
    view.contextMenu(view.tree);
    expect(view.menu()?.getAttribute("aria-label")).toBe("연결 메뉴");
    expect(view.menuItems()).toEqual(["세션 추가…", "그룹 추가…"]);
    view.choose("세션 추가…");
    expect(view.handlers.onAddSession).toHaveBeenCalledExactlyOnceWith();
    view.contextMenu(view.tree);
    view.choose("그룹 추가…");
    expect(view.handlers.onAddGroup).toHaveBeenCalledExactlyOnceWith();

    // 합성 "로컬" 그룹은 빈 곳과 같은 최상위 메뉴다.
    view.contextMenu(view.row("로컬"));
    expect(view.menuItems()).toEqual(["세션 추가…", "그룹 추가…"]);
    view.press(view.menu()!, "Escape");

    view.contextMenu(view.row("하위").querySelector(".tree-row")!);
    expect(view.menu()?.getAttribute("aria-label")).toBe("하위 그룹 메뉴");
    expect(view.menuItems()).toEqual(["세션 추가…", "그룹 추가…", "이름 바꾸기…", "삭제"]);
    expect(view.menu()!.querySelectorAll("[role=separator]")).toHaveLength(2);
    for (const label of ["세션 추가…", "그룹 추가…", "이름 바꾸기…", "삭제"]) {
      view.contextMenu(view.row("하위"));
      view.choose(label);
    }
    expect(view.handlers.onAddSession).toHaveBeenLastCalledWith("child");
    expect(view.handlers.onAddGroup).toHaveBeenLastCalledWith("child");
    expect(view.handlers.onEditGroup).toHaveBeenCalledExactlyOnceWith("child");
    expect(view.handlers.onDeleteGroup).toHaveBeenCalledExactlyOnceWith("child");

    // 하위 세션의 우클릭은 상위 그룹 메뉴로 번지지 않는다.
    view.contextMenu(view.row("깊은 셸"));
    expect(view.menu()?.getAttribute("aria-label")).toBe("로컬 메뉴");
    expect(view.menuItems()).toEqual(["로컬", "편집…", "복제", "삭제"]);
    view.choose("복제");
    expect(view.handlers.onDuplicate).toHaveBeenCalledExactlyOnceWith("deep");
  } finally {
    view.unmount();
  }
});

it("메뉴 키와 Shift+F10으로 그룹·로컬 메뉴를 열고 닫으면 원래 행으로 포커스를 돌린다", () => {
  const view = renderTree();
  try {
    act(() => view.row("작업").focus());
    view.press(view.row("작업"), "F10");
    expect(view.menu()).toBeNull();
    view.press(view.row("작업"), "F10", true);
    expect(view.menu()?.getAttribute("aria-label")).toBe("작업 그룹 메뉴");
    expect(document.activeElement?.textContent).toBe("세션 추가…");
    view.press(document.activeElement!, "Escape");
    expect(view.menu()).toBeNull();
    expect(document.activeElement).toBe(view.row("작업"));

    view.press(view.row("로컬"), "ContextMenu");
    expect(view.menu()?.getAttribute("aria-label")).toBe("연결 메뉴");
    view.press(document.activeElement!, "Escape");
    expect(document.activeElement).toBe(view.row("로컬"));

    // 메뉴 항목을 고르면 동작 전에 행으로 포커스를 돌린다.
    view.contextMenu(view.row("기타"));
    view.choose("이름 바꾸기…");
    expect(document.activeElement).toBe(view.row("기타"));
  } finally {
    view.unmount();
  }
});

it("focusRequest가 바뀌면 트리의 현재 행으로 포커스를 돌린다", () => {
  const view = renderTree();
  try {
    act(() => view.row("기타").focus());
    act(() => (document.activeElement as HTMLElement).blur());
    expect(document.activeElement).toBe(document.body);
    view.render({ focusRequest: 1 });
    expect(document.activeElement).toBe(view.row("기타"));
    // 다른 곳에 있는 포커스는 가져오지 않는다.
    const outside = document.body.appendChild(document.createElement("button"));
    act(() => outside.focus());
    view.render({ focusRequest: 2 });
    expect(document.activeElement).toBe(outside);
    outside.remove();
  } finally {
    view.unmount();
  }
});

it("세션 행 메뉴의 편집…과 삭제는 세션 ID를 넘기고 내 PC에는 없다", () => {
  const profiles: SavedConnections = {
    version: 2, groups: [],
    targets: [{
      revision: 4,
      profile: { id: "bucket", displayName: "버킷", defaultMethod: "s3", s3: { profile: "company", region: "ap-northeast-2", bucket: "dev", prefix: "" } },
    }],
  };
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const onEdit = vi.fn();
  const onDelete = vi.fn();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  try {
    act(() => root.render(<LocalTree profiles={profiles} onOpen={vi.fn()} onRegisterSshHost={vi.fn()} onOpenTunnel={vi.fn()}
      onEdit={onEdit} onDelete={onDelete} />));
    const row = (name: string) => [...host.querySelectorAll<HTMLLIElement>("li[role=treeitem]")].find(item => item.textContent === name)!;
    const menuItems = () => [...host.querySelectorAll<HTMLButtonElement>("button[role=menuitem]")];
    const open = (name: string) => act(() => { row(name).dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true })); });
    open("내 PC");
    expect(menuItems().map(item => item.textContent)).not.toContain("편집…");
    open("버킷");
    expect(menuItems().map(item => item.textContent)).toEqual(["S3", "편집…", "삭제"]);
    act(() => menuItems().find(item => item.textContent === "편집…")!.click());
    expect(onEdit).toHaveBeenCalledExactlyOnceWith("bucket");
    open("버킷");
    act(() => menuItems().find(item => item.textContent === "삭제")!.click());
    expect(onDelete).toHaveBeenCalledExactlyOnceWith("bucket");
  } finally {
    act(() => root.unmount());
    host.remove();
    vi.unstubAllGlobals();
  }
});
