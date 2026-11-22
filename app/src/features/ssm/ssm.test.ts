// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SsmSelectionView } from "./SsmSelectionView";
import { useSsmSelection, type SsmDiscovery, type SsmOnlineNode, type SsmResult } from "./ssm";

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
    return createElement("div", null, createElement(SsmSelectionView, { state, regions: [east, west] }),
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

async function choose(label: string, value: string) {
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
    expect(field("리전").value).toBe(west);
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
    expect(error()).toBe("alpha: SSO 로그인 만료");
    expect(adapter.relogin).not.toHaveBeenCalled();
    const button = host.querySelector("button");
    expect(button?.textContent).toBe("SSO 재로그인");
    await act(async () => { button?.click(); button?.click(); });
    expect(status()).toBe("alpha: SSO 재로그인 중");
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
    await act(async () => host.querySelector("button")?.click());
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
    expect(host.querySelector("button")).toBeNull();
    await choose("리전", east);
    expect(error()).toBe("alpha: Session Manager 플러그인 없음");
    expect(host.querySelector("button")).toBeNull();
    await choose("리전", west);
    expect(error()).toBe("alpha: 분류되지 않은 오류");
    expect(host.querySelector("button")).toBeNull();
    expect(adapter.listOnlineNodes.mock.calls.map(([scope]) => scope.region)).toEqual([east, west, east, west]);
    expect(adapter.relogin).not.toHaveBeenCalled();
    expect(selected()).toBeNull();
  });
});
