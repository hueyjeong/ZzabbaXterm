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
const field = () => host.querySelector("select")!;
const click = (name: string) => act(async () => {
  [...host.querySelectorAll("button")].find((item) => item.textContent === name)!.click();
});
const choose = (id: string) => act(async () => {
  field().value = id;
  field().dispatchEvent(new Event("change", { bubbles: true }));
});

it("구분된 실제 ID를 표시하고 연결 확인을 별도 입력으로 받는다", async () => {
  const port = actions();
  await render(port);
  expect([...field().options].map((option) => option.textContent)).toEqual([
    "세션 선택", "개발 서버 (세션 41)", "개발 서버 (세션 42)",
  ]);
  expect(field().value).toBe("");
  const terminal = host.querySelector("textarea")!;
  terminal.focus();
  await choose(beta.id);
  expect(port.select).toHaveBeenCalledExactlyOnceWith(beta.id);
  expect(port.confirm).not.toHaveBeenCalled();
  expect(host.textContent).toContain("ap-northeast-2 / i-001");
  await click("연결 확인");
  expect(port.confirm).toHaveBeenCalledExactlyOnceWith(beta.id, snapshot(beta).selectionId);
  expect(host.querySelector('[role="status"]')?.textContent).toBe("연결 확인됨");
  expect(document.activeElement).toBe(terminal);
  await render(port, [beta, alpha], false);
  await render(port, [beta, alpha]);
  expect(field().value).toBe(beta.id);
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
  expect(field().value).toBe(alpha.id);
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
  expect(field().value).toBe("");
  expect(field().disabled).toBe(true);
  expect(host.textContent).not.toContain("연결 확인됨");

  await act(async () => clearing.resolve(null));
  expect(field().disabled).toBe(false);
  await choose(alpha.id);
  await act(async () => late.resolve({ ...snapshot(alpha, "1"), confirmed: true }));
  expect(field().value).toBe(alpha.id);
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
  expect(field().value).toBe("");
  expect(host.textContent).not.toContain("연결 확인됨");
});

it("선택 응답 전에 세션이 닫혀도 도착한 선택을 해제한다", async () => {
  const port = actions();
  const late = Promise.withResolvers<SessionSelectionSnapshot>();
  port.select.mockReturnValueOnce(late.promise);
  await render(port);
  await choose(alpha.id);
  await render(port, [beta]);
  await act(async () => late.resolve(snapshot(alpha)));
  expect(port.clear).toHaveBeenCalledExactlyOnceWith(snapshot(alpha).selectionId);
  expect(field().value).toBe("");
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
  expect(field().value).toBe("");
  expect(host.textContent).not.toContain("연결 확인됨");
});

it("확인 응답의 선택 ID가 다르면 연결 확인 실패를 표시한다", async () => {
  const port = actions();
  port.confirm.mockResolvedValueOnce({ ...snapshot(alpha, "7"), confirmed: true });
  await render(port);
  await choose(alpha.id);
  await click("연결 확인");
  expect(field().value).toBe(alpha.id);
  expect(host.querySelector('[role="alert"]')?.textContent).toBe("세션 연결 확인 실패");
  expect(host.textContent).not.toContain("연결 확인됨");
});
