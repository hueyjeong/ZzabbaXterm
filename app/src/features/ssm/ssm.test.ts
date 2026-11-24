// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SsmSelectionView } from "./SsmSelectionView";
import { useSsmSelection, type SsmDiscovery, type SsmOnlineNode, type SsmResult } from "./ssm";
import { createSsmDiscovery } from "./ssmDiscovery";

const invokeSsm = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeSsm }));

const east = "us-east-1";
const west = "us-west-2";
const one = { instanceId: "i-00000001", name: "공용" };
const two = { instanceId: "i-00000002", name: "공용" };
let host: HTMLDivElement;
let root: Root;

function discovery() {
  return {
    listProfiles: vi.fn<SsmDiscovery["listProfiles"]>().mockResolvedValue(ok(["alpha", "beta"])),
    configuredRegion: vi.fn<SsmDiscovery["configuredRegion"]>().mockResolvedValue(ok(east)),
    listOnlineNodes: vi.fn<SsmDiscovery["listOnlineNodes"]>().mockResolvedValue(ok([one, two])),
    relogin: vi.fn<SsmDiscovery["relogin"]>().mockResolvedValue(ok(undefined)),
  } satisfies SsmDiscovery;
}

function ok<T>(value: T): SsmResult<T> {
  return { ok: true, value };
}

async function render(adapter: SsmDiscovery) {
  function Fixture() {
    const state = useSsmSelection(adapter);
    return createElement("div", null, createElement(SsmSelectionView, { state }),
      createElement("output", { "aria-label": "마법사에 전달할 대상" }, JSON.stringify(state.selection)));
  }
  await act(async () => root.render(createElement(Fixture)));
}

function field(label: string) {
  const select = Array.from(host.querySelectorAll("label"))
    .find((item) => item.firstChild?.textContent?.trim() === label)?.querySelector("select");
  if (!select) throw new Error(`선택 항목 없음: ${label}`);
  return select;
}

function regionField() {
  const input = host.querySelector("label input");
  if (!(input instanceof HTMLInputElement)) throw new Error("리전 입력 없음");
  return input;
}

function reloginButton() {
  return [...host.querySelectorAll("button")].find((button) => button.textContent === "AWS 재로그인");
}

async function choose(label: string, value: string) {
  if (label === "리전") {
    const input = regionField();
    expect(input.disabled).toBe(false);
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const apply = [...host.querySelectorAll("button")].find((button) => button.textContent === "리전 적용");
    expect(apply?.disabled).toBe(false);
    await act(async () => apply?.click());
    return;
  }
  const control = field(label);
  expect(control.disabled).toBe(false);
  await act(async () => {
    control.value = value;
    control.dispatchEvent(new Event("change", { bubbles: true }));
  });
}

function status() { return host.querySelector('[role="status"]')?.textContent; }
function error() { return host.querySelector('[role="alert"]')?.textContent; }
function selected() { return JSON.parse(host.querySelector("output")?.textContent ?? "null"); }

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

describe("SSM 선택", () => {
  it("이전 프로필의 기본 리전과 이전 리전의 노드 응답을 버린다", async () => {
    const oldRegion = Promise.withResolvers<SsmResult<string | null>>();
    const oldNodes = Promise.withResolvers<SsmResult<readonly SsmOnlineNode[]>>();
    const adapter = discovery();
    adapter.configuredRegion.mockReturnValueOnce(oldRegion.promise);
    adapter.listOnlineNodes.mockReturnValueOnce(oldNodes.promise).mockResolvedValueOnce(ok([two]));
    await render(adapter);
    await choose("프로필", "alpha");
    expect(status()).toBe("기본 리전 조회 중");
    await choose("프로필", "beta");
    expect(status()).toBe("온라인 노드 조회 중");
    await choose("리전", west);
    await act(async () => {
      oldRegion.resolve(ok("eu-west-1"));
      oldNodes.resolve(ok([one]));
    });
    expect(adapter.configuredRegion.mock.calls.map(([profile]) => profile)).toEqual(["alpha", "beta"]);
    expect(adapter.configuredRegion.mock.calls[0][1].aborted).toBe(true);
    expect(adapter.listOnlineNodes.mock.calls.map(([scope]) => scope)).toEqual([
      { profile: "beta", region: east }, { profile: "beta", region: west },
    ]);
    expect(adapter.listOnlineNodes.mock.calls[0][1].aborted).toBe(true);
    expect(regionField().value).toBe(west);
    expect(Array.from(field("온라인 노드").options, (option) => option.value)).toEqual(["", two.instanceId]);
  });

  it("기본 리전이 없으면 수동 선택을 기다리고 동명 노드를 ID와 조회 범위로 구별한다", async () => {
    const adapter = discovery();
    adapter.configuredRegion.mockResolvedValueOnce(ok(null));
    await render(adapter);
    await choose("프로필", "alpha");
    expect(status()).toBe("선택된 리전 없음");
    expect(adapter.listOnlineNodes).not.toHaveBeenCalled();
    await choose("리전", west);
    expect(field("온라인 노드").textContent).toContain(`공용 (${one.instanceId})`);
    expect(field("온라인 노드").textContent).toContain(`공용 (${two.instanceId})`);
    expect(selected()).toBeNull();
    await choose("온라인 노드", two.instanceId);
    expect(selected()).toEqual({ profile: "alpha", region: west, instanceId: two.instanceId });
    await choose("리전", east);
    expect(selected()).toBeNull();
    await choose("온라인 노드", one.instanceId);
    expect(selected()).toEqual({ profile: "alpha", region: east, instanceId: one.instanceId });
    await choose("프로필", "beta");
    expect(selected()).toBeNull();
  });

  it("만료에서 사용자가 재로그인한 뒤 같은 프로필과 수동 리전으로 다시 조회한다", async () => {
    const login = Promise.withResolvers<SsmResult<void>>();
    const adapter = discovery();
    adapter.configuredRegion.mockResolvedValueOnce(ok(null));
    adapter.listOnlineNodes.mockResolvedValueOnce({ ok: false, error: "SsoLoginRequired" });
    adapter.relogin.mockReturnValueOnce(login.promise);
    await render(adapter);
    await choose("프로필", "alpha");
    await choose("리전", west);
    expect(error()).toBe("alpha: AWS 로그인 만료");
    expect(adapter.relogin).not.toHaveBeenCalled();
    const button = reloginButton();
    expect(button?.textContent).toBe("AWS 재로그인");
    await act(async () => { button?.click(); button?.click(); });
    expect(status()).toBe("alpha: AWS 재로그인 중");
    expect(adapter.relogin.mock.calls.map(([scope, cause]) => [scope, cause])).toEqual([
      [{ profile: "alpha", region: west }, "SsoLoginRequired"],
    ]);
    expect(adapter.listOnlineNodes).toHaveBeenCalledTimes(1);
    await act(async () => login.resolve(ok(undefined)));
    expect(adapter.listOnlineNodes.mock.calls.map(([scope]) => scope)).toEqual([
      { profile: "alpha", region: west }, { profile: "alpha", region: west },
    ]);
    expect(adapter.configuredRegion).toHaveBeenCalledTimes(1);
    expect(error()).toBeUndefined();
    await choose("온라인 노드", one.instanceId);
    expect(selected()).toEqual({ profile: "alpha", region: west, instanceId: one.instanceId });
  });

  it("재로그인 중 범위를 바꾸면 예전 프로필의 재조회를 막는다", async () => {
    const login = Promise.withResolvers<SsmResult<void>>();
    const adapter = discovery();
    adapter.listOnlineNodes.mockResolvedValueOnce({ ok: false, error: "SsoLoginRequired" });
    adapter.relogin.mockReturnValueOnce(login.promise);
    await render(adapter);
    await choose("프로필", "alpha");
    await act(async () => reloginButton()?.click());
    await choose("프로필", "beta");
    await act(async () => login.resolve(ok(undefined)));
    expect(adapter.relogin.mock.calls[0][2].aborted).toBe(true);
    expect(adapter.listOnlineNodes.mock.calls.map(([scope]) => scope)).toEqual([
      { profile: "alpha", region: east }, { profile: "beta", region: east },
    ]);
    await choose("온라인 노드", two.instanceId);
    expect(selected()).toEqual({ profile: "beta", region: east, instanceId: two.instanceId });
  });

  it("빈 목록, IAM 거부, 플러그인 누락, 미분류 오류를 구별하고 자동 복구를 시작하지 않는다", async () => {
    const adapter = discovery();
    adapter.listOnlineNodes.mockResolvedValueOnce(ok([]))
      .mockResolvedValueOnce({ ok: false, error: "AccessDenied" })
      .mockResolvedValueOnce({ ok: false, error: "PluginMissing" })
      .mockRejectedValueOnce(new Error("unexpected"));
    await render(adapter);
    await choose("프로필", "alpha");
    expect(status()).toBe("온라인 노드 없음");
    expect(error()).toBeUndefined();
    expect(field("온라인 노드").disabled).toBe(true);
    expect(adapter.listOnlineNodes).toHaveBeenCalledTimes(1);
    await choose("리전", west);
    expect(error()).toBe("alpha: IAM 권한 거부");
    expect(status()).toBeUndefined();
    expect(reloginButton()).toBeUndefined();
    await choose("리전", east);
    expect(error()).toBe("alpha: Session Manager 플러그인 없음");
    expect(reloginButton()).toBeUndefined();
    await choose("리전", west);
    expect(error()).toBe("alpha: 분류되지 않은 오류");
    expect(reloginButton()).toBeUndefined();
    expect(adapter.listOnlineNodes.mock.calls.map(([scope]) => scope.region)).toEqual([east, west, east, west]);
    expect(adapter.relogin).not.toHaveBeenCalled();
    expect(selected()).toBeNull();
  });
});

describe("SSM 검색 IPC", () => {
  beforeEach(() => { invokeSsm.mockReset(); });
  afterEach(() => { delete window.__ZZABBA_SSM_DOCUMENT__; });

  it("문서 또는 ID가 나오기 전 취소를 처리하고 작업마다 문서 값을 고정한다", async () => {
    const adapter = createSsmDiscovery();
    const waiting = new AbortController();
    const ready = new AbortController();
    const abandoned = adapter.listProfiles(waiting.signal);
    const pending = adapter.configuredRegion("alpha", ready.signal);
    expect(invokeSsm).not.toHaveBeenCalled();
    waiting.abort();
    await expect(abandoned).resolves.toEqual({ ok: false, error: "Unknown" });

    const reserved = Promise.withResolvers<string>();
    invokeSsm.mockReturnValueOnce(reserved.promise).mockResolvedValueOnce(null);
    window.__ZZABBA_SSM_DOCUMENT__ = "old-document";
    window.dispatchEvent(new Event("zzabba:ssm-document"));
    await Promise.resolve();
    expect(invokeSsm.mock.calls).toEqual([["ssm_discovery", { document: "old-document", request: { action: "reserve" } }]]);
    window.__ZZABBA_SSM_DOCUMENT__ = "new-document";
    ready.abort();
    reserved.resolve("15");
    await expect(pending).resolves.toEqual({ ok: false, error: "Unknown" });
    expect(invokeSsm.mock.calls).toEqual([
      ["ssm_discovery", { document: "old-document", request: { action: "reserve" } }],
      ["ssm_discovery", { document: "old-document", request: { action: "cancel", requestId: "15" } }],
    ]);
  });

  it("늦은 만료 응답에는 재로그인 권한이 없고 구조적 오류 코드만 구별한다", async () => {
    window.__ZZABBA_SSM_DOCUMENT__ = "document";
    const adapter = createSsmDiscovery();
    const scope = { profile: "alpha", region: east };
    const controller = new AbortController();
    const running = Promise.withResolvers<never>();
    invokeSsm.mockResolvedValueOnce("1").mockReturnValueOnce(running.promise).mockResolvedValueOnce(null);
    const pending = adapter.listOnlineNodes(scope, controller.signal);
    await Promise.resolve();
    await Promise.resolve();
    controller.abort();
    running.reject({ code: "SsoLoginRequired" });
    await expect(pending).resolves.toEqual({ ok: false, error: "Unknown" });
    await expect(adapter.relogin(scope, "SsoLoginRequired", new AbortController().signal))
      .resolves.toEqual({ ok: false, error: "Unknown" });
    expect(invokeSsm.mock.calls.map(([, args]) => args.request)).toEqual([
      { action: "reserve" }, { action: "run", requestId: "1", operation: { kind: "listOnlineNodes", ...scope } },
      { action: "cancel", requestId: "1" },
    ]);
    for (const [id, rejection, error] of [
      ["2", { code: "AccessDenied" }, "AccessDenied"],
      ["3", { code: "PluginMissing" }, "PluginMissing"],
      ["4", "SsoLoginRequired", "Unknown"],
    ] as const) {
      invokeSsm.mockResolvedValueOnce(id).mockRejectedValueOnce(rejection);
      await expect(adapter.listOnlineNodes(scope, new AbortController().signal)).resolves.toEqual({ ok: false, error });
    }
  });

  it("설치 또는 구성 오류에서 Linux 시스템 설치 요건을 표시한다", async () => {
    window.__ZZABBA_SSM_DOCUMENT__ = "document";
    invokeSsm.mockResolvedValueOnce("1").mockRejectedValueOnce({ code: "InvalidConfiguration" });
    await render(createSsmDiscovery());
    expect(error()).toContain("AWS CLI v2와 Session Manager 플러그인의 관리자 관리 시스템 설치가 필요합니다");
    expect(reloginButton()).toBeUndefined();
  });

  it("화면에서 만료된 조회 ID만 한 번 사용하고 성공 뒤 새 ID로 동일 범위를 조회한다", async () => {
    window.__ZZABBA_SSM_DOCUMENT__ = "document";
    invokeSsm.mockResolvedValueOnce("1").mockResolvedValueOnce(["alpha"])
      .mockResolvedValueOnce("2").mockResolvedValueOnce(east)
      .mockResolvedValueOnce("3").mockRejectedValueOnce({ code: "SsoLoginRequired" })
      .mockResolvedValueOnce(null).mockResolvedValueOnce("4").mockResolvedValueOnce([{ instanceId: one.instanceId }]);
    await render(createSsmDiscovery());
    await choose("프로필", "alpha");
    expect(error()).toBe("alpha: AWS 로그인 만료");
    const button = reloginButton();
    expect(button).toBeDefined();
    await act(async () => { button?.click(); button?.click(); });
    const operation = { kind: "listOnlineNodes", profile: "alpha", region: east };
    expect(invokeSsm.mock.calls.map(([, args]) => args.request)).toEqual([
      { action: "reserve" }, { action: "run", requestId: "1", operation: { kind: "listProfiles" } },
      { action: "reserve" }, { action: "run", requestId: "2", operation: { kind: "configuredRegion", profile: "alpha" } },
      { action: "reserve" }, { action: "run", requestId: "3", operation },
      { action: "relogin", requestId: "3" },
      { action: "reserve" }, { action: "run", requestId: "4", operation },
    ]);
    await choose("온라인 노드", one.instanceId);
    expect(selected()).toEqual({ profile: "alpha", region: east, instanceId: one.instanceId });
  });

  it("다른 범위 객체에서 만료 ID를 사용할 수 없고 실행 중 취소는 같은 ID에만 적용된다", async () => {
    window.__ZZABBA_SSM_DOCUMENT__ = "document";
    const adapter = createSsmDiscovery();
    const scope = { profile: "alpha", region: east };
    const lookup = new AbortController();
    const login = new AbortController();
    const running = Promise.withResolvers<null>();
    invokeSsm.mockResolvedValueOnce("7").mockRejectedValueOnce({ code: "SsoLoginRequired" })
      .mockReturnValueOnce(running.promise).mockResolvedValueOnce(null);
    await expect(adapter.listOnlineNodes(scope, lookup.signal)).resolves.toEqual({ ok: false, error: "SsoLoginRequired" });
    await expect(adapter.relogin({ ...scope }, "SsoLoginRequired", login.signal)).resolves.toEqual({ ok: false, error: "Unknown" });
    const pending = adapter.relogin(scope, "SsoLoginRequired", login.signal);
    lookup.abort();
    login.abort();
    running.resolve(null);
    await expect(pending).resolves.toEqual({ ok: false, error: "Unknown" });
    await expect(adapter.relogin(scope, "SsoLoginRequired", new AbortController().signal))
      .resolves.toEqual({ ok: false, error: "Unknown" });
    expect(invokeSsm.mock.calls.slice(2).map(([, args]) => args.request)).toEqual([
      { action: "relogin", requestId: "7" }, { action: "cancel", requestId: "7" },
    ]);
  });

  it("리전 전환과 마법사 닫기에서 사용하지 않은 만료 ID를 취소한다", async () => {
    window.__ZZABBA_SSM_DOCUMENT__ = "document";
    invokeSsm.mockResolvedValueOnce("1").mockResolvedValueOnce(["alpha"])
      .mockResolvedValueOnce("2").mockResolvedValueOnce(east)
      .mockResolvedValueOnce("3").mockRejectedValueOnce({ code: "SsoLoginRequired" })
      .mockResolvedValueOnce(null).mockResolvedValueOnce("4").mockRejectedValueOnce({ code: "SsoLoginRequired" })
      .mockResolvedValueOnce(null);
    await render(createSsmDiscovery());
    await choose("프로필", "alpha");
    expect(error()).toBe("alpha: AWS 로그인 만료");
    await choose("리전", west);
    expect(error()).toBe("alpha: AWS 로그인 만료");
    await act(async () => root.render(null));
    expect(invokeSsm.mock.calls.slice(6).map(([, args]) => args.request)).toEqual([
      { action: "cancel", requestId: "3" }, { action: "reserve" },
      { action: "run", requestId: "4", operation: { kind: "listOnlineNodes", profile: "alpha", region: west } },
      { action: "cancel", requestId: "4" },
    ]);
  });
});
