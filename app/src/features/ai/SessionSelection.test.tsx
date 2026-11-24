// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SessionSelection, type SessionSelectionProps, type SessionSelectionSnapshot } from "./SessionSelection";

const alpha = { id: "41", label: "개발 서버", region: "ap-northeast-2", node: "i-001" };
const beta = { ...alpha, id: "42" };
const snapshot = ({ id, region, node }: typeof alpha, selectionId = `900719925474099${id}`): SessionSelectionSnapshot =>
  ({ id, region, node, selectionId, confirmed: false });
let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.body.appendChild(document.createElement("div"));
  root = createRoot(host);
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.unstubAllGlobals();
});

function actions() {
  return {
    select: vi.fn<SessionSelectionProps["actions"]["select"]>().mockImplementation(async (id: string) => snapshot(id === alpha.id ? alpha : beta)),
    confirm: vi.fn<SessionSelectionProps["actions"]["confirm"]>().mockImplementation(async (id: string, token: string) =>
      ({ ...snapshot(id === alpha.id ? alpha : beta, token), confirmed: true })),
    clear: vi.fn<SessionSelectionProps["actions"]["clear"]>().mockResolvedValue(null),
  };
}

const render = (actions: SessionSelectionProps["actions"], candidates = [alpha, beta], visible = true) =>
  act(async () => root.render(<><textarea aria-label="중앙 터미널 입력" />
    <SessionSelection actions={actions} candidates={candidates} visible={visible} /></>));
const field = () => host.querySelector<HTMLButtonElement>('button[aria-haspopup="menu"]')!;
const value = () => field().dataset.value;
const click = (name: string) => act(async () => {
  [...host.querySelectorAll("button")].find((item) => item.textContent === name || item.getAttribute("aria-label") === name)!.click();
});
const menuItems = () => [...host.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')];
async function choose(id: string) {
  await act(async () => { field().click(); });
  const item = menuItems().find((element) => element.textContent?.endsWith(`(세션 ${id})`));
  expect(item, id).toBeDefined();
  await act(async () => { item!.click(); });
}

it("구분된 실제 ID를 표시하고 연결 확인을 별도 입력으로 받는다", async () => {
  const port = actions();
  await render(port);
  expect(field().textContent).toBe("세션 선택");
  expect(field().getAttribute("aria-label")).toBe("AI 작업 세션: 선택 안 함");
  await act(async () => { field().click(); });
  expect(menuItems().map((item) => item.textContent)).toEqual(["개발 서버 (세션 41)", "개발 서버 (세션 42)"]);
  await act(async () => {
    host.querySelector('[role="menu"]')!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  });
  expect(host.querySelector('[role="menu"]')).toBeNull();
  expect(document.activeElement).toBe(field());
  expect(value()).toBe("");
  await choose(beta.id);
  expect(port.select).toHaveBeenCalledExactlyOnceWith(beta.id);
  expect(port.confirm).not.toHaveBeenCalled();
  expect(field().getAttribute("aria-label")).toBe("AI 작업 세션: 개발 서버 (세션 42)");
  expect(field().title).toBe("ap-northeast-2 / i-001");
  expect(host.querySelector('[role="status"]')?.textContent).toBe("연결 확인 전");
  const terminal = host.querySelector("textarea")!;
  terminal.focus();
  await click("연결 확인");
  expect(port.confirm).toHaveBeenCalledExactlyOnceWith(beta.id, snapshot(beta).selectionId);
  expect(host.querySelector('[role="status"]')?.textContent).toBe("연결 확인됨");
  expect(document.activeElement).toBe(terminal);
  await render(port, [beta, alpha], false);
  await render(port, [beta, alpha]);
  expect(value()).toBe(beta.id);
  expect(host.querySelector('[role="status"]')?.textContent).toBe("연결 확인됨");
  expect(port.select).toHaveBeenCalledTimes(1);
  expect(port.confirm).toHaveBeenCalledTimes(1);
  expect(port.clear).not.toHaveBeenCalled();
});

it("다른 세션을 거쳐 같은 ID를 다시 선택해도 이전 확인 응답을 버린다", async () => {
  const port = actions();
  const late = Promise.withResolvers<SessionSelectionSnapshot>();
  port.confirm.mockReturnValueOnce(late.promise);
  port.select.mockResolvedValueOnce(snapshot(alpha, "1"))
    .mockResolvedValueOnce(snapshot(beta, "2")).mockResolvedValueOnce(snapshot(alpha, "3"));
  await render(port);
  await choose(alpha.id);
  await click("연결 확인");
  await choose(beta.id);
  await choose(alpha.id);
  await act(async () => late.resolve({ ...snapshot(alpha, "1"), confirmed: true }));
  expect(value()).toBe(alpha.id);
  expect(host.textContent).not.toContain("연결 확인됨");
  await click("연결 확인");
  expect(port.confirm.mock.calls).toEqual([[alpha.id, "1"], [alpha.id, "3"]]);
});

it("숨겨지기 전에 끝나지 않은 확인을 해제하고 새 선택 ID로만 확인한다", async () => {
  const port = actions();
  const late = Promise.withResolvers<SessionSelectionSnapshot>();
  const clearing = Promise.withResolvers<null>();
  port.confirm.mockReturnValueOnce(late.promise);
  port.clear.mockReturnValueOnce(clearing.promise);
  port.select.mockResolvedValueOnce(snapshot(alpha, "1")).mockResolvedValueOnce(snapshot(alpha, "2"));
  await render(port);
  await choose(alpha.id);
  await click("연결 확인");
  await render(port, [alpha, beta], false);
  expect(port.clear).toHaveBeenCalledExactlyOnceWith("1");
  await render(port);
  expect(value()).toBe("");
  expect(field().disabled).toBe(true);
  expect(host.textContent).not.toContain("연결 확인됨");

  await act(async () => clearing.resolve(null));
  expect(field().disabled).toBe(false);
  await choose(alpha.id);
  await act(async () => late.resolve({ ...snapshot(alpha, "1"), confirmed: true }));
  expect(value()).toBe(alpha.id);
  expect(host.textContent).not.toContain("연결 확인됨");
  await click("연결 확인");
  expect(port.confirm.mock.calls).toEqual([[alpha.id, "1"], [alpha.id, "2"]]);
  expect(port.clear).toHaveBeenCalledTimes(1);
  expect(host.textContent).toContain("연결 확인됨");
});

it("선택했던 세션이 닫히면 숨겨진 상태에서도 정확한 선택을 해제한다", async () => {
  const port = actions();
  const late = Promise.withResolvers<SessionSelectionSnapshot>();
  port.confirm.mockReturnValueOnce(late.promise);
  await render(port);
  await choose(alpha.id);
  await click("연결 확인");
  await render(port, [beta], false);
  await act(async () => late.resolve({ ...snapshot(alpha), confirmed: true }));
  await render(port);
  expect(port.clear).toHaveBeenCalledExactlyOnceWith(snapshot(alpha).selectionId);
  expect(value()).toBe("");
  expect(host.textContent).not.toContain("연결 확인됨");
});

it("선택 응답 전에 세션이 닫혀도 도착한 선택을 해제한다", async () => {
  const port = actions();
  const clearing = Promise.withResolvers<null>();
  const late = Promise.withResolvers<SessionSelectionSnapshot>();
  port.select.mockReturnValueOnce(late.promise);
  port.clear.mockReturnValueOnce(clearing.promise);
  await render(port);
  await choose(alpha.id);
  await render(port, []);
  expect(field()).toBeNull();
  expect([...host.querySelectorAll('[role="status"]')].map((status) => status.textContent)).toEqual([
    "SSM 세션 없음", "세션 선택 중",
  ]);
  await act(async () => { late.resolve(snapshot(alpha)); });
  expect(port.clear).toHaveBeenCalledExactlyOnceWith(snapshot(alpha).selectionId);
  expect(field()).toBeNull();
  expect(host.textContent).toContain("세션 선택 해제 중");
  await act(async () => { clearing.reject(new Error("해제 실패")); });
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("세션 선택 해제 실패");
  expect(field()).toBeNull();
  expect(port.confirm).not.toHaveBeenCalled();
});

it("선택 해제 후 이전 연결 확인이 도착해도 세션을 되살리지 않는다", async () => {
  const port = actions();
  const late = Promise.withResolvers<SessionSelectionSnapshot>();
  port.confirm.mockReturnValueOnce(late.promise);
  await render(port);
  await choose(alpha.id);
  await click("연결 확인");
  await click("선택 해제");
  await act(async () => late.resolve({ ...snapshot(alpha), confirmed: true }));
  expect(port.clear).toHaveBeenCalledExactlyOnceWith(snapshot(alpha).selectionId);
  expect(value()).toBe("");
  expect(host.textContent).not.toContain("연결 확인됨");
});

it("확인 응답의 선택 ID가 다르면 연결 확인 실패를 표시한다", async () => {
  const port = actions();
  port.confirm.mockResolvedValueOnce({ ...snapshot(alpha, "7"), confirmed: true });
  await render(port);
  await choose(alpha.id);
  await click("연결 확인");
  expect(value()).toBe(alpha.id);
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("세션 연결 확인 실패");
  expect(host.textContent).not.toContain("연결 확인됨");
});

it("칩 우클릭 메뉴로 세션을 고르고 선택을 해제한다", async () => {
  const port = actions();
  await render(port);
  const contextMenu = async (init: MouseEventInit = {}) => {
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 10, clientY: 20, ...init });
    await act(async () => { field().dispatchEvent(event); });
    return event;
  };
  expect((await contextMenu({ shiftKey: true })).defaultPrevented).toBe(false);
  expect(host.querySelector('[role="menu"]')).toBeNull();

  expect((await contextMenu()).defaultPrevented).toBe(true);
  expect(menuItems().map((item) => item.textContent)).toEqual(["개발 서버 (세션 41)", "개발 서버 (세션 42)"]);
  await act(async () => { menuItems()[1].click(); });
  expect(port.select).toHaveBeenCalledExactlyOnceWith(beta.id);
  expect(value()).toBe(beta.id);

  await contextMenu();
  expect(host.querySelector(".ui-menu-label")?.textContent).toBe("ap-northeast-2 / i-001");
  expect(menuItems().map((item) => item.textContent)).toEqual(["개발 서버 (세션 41)", "개발 서버 (세션 42)", "선택 해제"]);
  await act(async () => { menuItems()[2].click(); });
  expect(port.clear).toHaveBeenCalledExactlyOnceWith(snapshot(beta).selectionId);
  expect(value()).toBe("");
});

it("숨겨지면 열린 우클릭 메뉴를 닫고 다시 보일 때 되살리지 않는다", async () => {
  const port = actions();
  await render(port);
  await act(async () => {
    field().dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 10, clientY: 20 }));
  });
  expect(host.querySelector('[role="menu"]')).not.toBeNull();
  await render(port, [alpha, beta], false);
  expect(host.querySelector('[role="menu"]')).toBeNull();
  await render(port);
  expect(host.querySelector('[role="menu"]')).toBeNull();
});

it("메뉴 키와 Shift+F10으로 칩 메뉴를 열고 닫으면 칩으로 포커스를 돌린다", async () => {
  await render(actions());
  for (const init of [{ key: "ContextMenu" }, { key: "F10", shiftKey: true }]) {
    field().focus();
    const event = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
    await act(async () => { field().dispatchEvent(event); });
    expect(event.defaultPrevented).toBe(true);
    expect(host.querySelector('[role="menu"]')?.getAttribute("aria-label")).toBe("AI 작업 세션");
    expect(document.activeElement).toBe(menuItems()[0]);
    await act(async () => {
      host.querySelector('[role="menu"]')!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    expect(host.querySelector('[role="menu"]')).toBeNull();
    expect(document.activeElement).toBe(field());
  }
});
