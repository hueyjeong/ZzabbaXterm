// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { connectLocalTerminal, type LocalTerminalIpc } from "../terminal/session";
import { ElevationApproval } from "./ElevationApproval";
import { approvalFields, parseArguments, validateElevationRequest } from "./elevation";

const ITEM = "21eb36c3-234d-439d-9e1f-1d3cc5ba8125";

it("승인 카드는 인자 경계를 합치지 않고 출력만으로 승인하지 않는다", () => {
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const approve = vi.fn();
  const cancel = vi.fn();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  try {
    act(() => root.render(<ElevationApproval
      request={{ program: "/usr/bin/id", args: ["a b", "[sudo] password for demo:"], item: ITEM }}
      onApprove={approve} onCancel={cancel} />));
    const values = [...host.querySelectorAll("dd")].map(node => node.textContent);
    expect(values).toContain("a b");
    expect(values).toContain("[sudo] password for demo:");
    expect(new Set(values).size).toBe(values.length);
    expect(approve).not.toHaveBeenCalled();
    act(() => [...host.querySelectorAll("button")].find(button => button.textContent === "승인")?.click());
    expect(approve).toHaveBeenCalledTimes(1);
  } finally {
    act(() => root.unmount());
    host.remove();
    vi.unstubAllGlobals();
  }
});

it("승격 정보는 구조로만 검증하고 셸 문자열로 다시 합치지 않는다", () => {
  expect(parseArguments("-u\n\n id \n")).toEqual(["-u", "id"]);
  expect(approvalFields({ program: "/usr/bin/id", args: ["a b", "c"], item: ITEM }).map(field => field.label))
    .toEqual(["대상", "실행 계정", "실행 파일", "인자 1", "인자 2", "금고 항목"]);
  expect(validateElevationRequest({ program: "id", args: [], item: ITEM })).toBe("실행 파일은 절대 경로여야 합니다");
  expect(validateElevationRequest({ program: "/usr/bin/id", args: ["a\nb"], item: ITEM }))
    .toBe("인자에 제어 문자를 넣을 수 없습니다");
  expect(validateElevationRequest({ program: "/usr/bin/id", args: [], item: "not-an-item" }))
    .toBe("저장 암호 항목 ID는 36자 UUID여야 합니다");
  expect(validateElevationRequest({ program: "/usr/bin/id", args: ["-u"], item: ITEM })).toBeNull();
});

it("터미널 출력의 sudo 문구만으로는 승격 요청을 만들지 않는다", async () => {
  const open = vi.fn(async (_size: unknown, receive: (message: unknown) => void) => {
    receive({ kind: "notice", value: "[sudo] password for demo:" });
    return "7";
  });
  const ipc = {
    open,
    start: vi.fn(async () => {}),
    input: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    ack: vi.fn(async () => {}),
    close: vi.fn(async () => null),
  } satisfies LocalTerminalIpc;
  const status = vi.fn();
  const session = connectLocalTerminal({ rows: 24, cols: 80 }, vi.fn(), status, ipc);
  await vi.waitFor(() => expect(status).toHaveBeenCalledWith({ kind: "running", notice: "[sudo] password for demo:" }, "7"));
  expect(open).toHaveBeenCalledTimes(1);
  expect((open.mock.calls[0] as unknown[])[4]).toBeUndefined();
  session.close();
});

it("명시적인 승격 요청만 실행 파일과 인자 배열을 그대로 전달한다", async () => {
  const open = vi.fn(async () => "8");
  const ipc = {
    open,
    start: vi.fn(async () => {}),
    input: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    ack: vi.fn(async () => {}),
    close: vi.fn(async () => null),
  } satisfies LocalTerminalIpc;
  const elevation = { program: "/usr/bin/id", args: ["-u", "a b"], item: ITEM };
  const session = connectLocalTerminal({ rows: 24, cols: 80 }, vi.fn(), vi.fn(), ipc, undefined, false, false, undefined, elevation);
  await vi.waitFor(() => expect(ipc.start).toHaveBeenCalledWith("8"));
  expect((open.mock.calls[0] as unknown[])[4]).toEqual(elevation);
  session.close();
});
