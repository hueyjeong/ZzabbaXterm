import { invoke } from "@tauri-apps/api/core";
import { beforeEach, expect, it, vi } from "vitest";
import { createLocalConnectionIpc } from "./connectionIpc";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

beforeEach(() => { vi.mocked(invoke).mockReset(); });

it("그룹과 세션 편집 명령에 초안, 개정, 하위 항목을 그대로 넘긴다", async () => {
  vi.mocked(invoke).mockResolvedValue({ revision: 2 });
  const ipc = createLocalConnectionIpc();
  const group = { version: 2, revision: 1, group: { id: "child", displayName: "하위", parentId: "work" } } as const;
  await expect(ipc.saveGroup(group)).resolves.toEqual({ revision: 2 });
  const target = {
    version: 2, targetRevision: null,
    target: { id: "top", displayName: "최상위 셸", defaultMethod: "local", local: { shell: "powerShell", startDirectory: "C:\\work" } },
  } as const;
  await ipc.saveTarget(target);
  const expected = [{ kind: "group", id: "child", revision: 1 }, { kind: "target", id: "shell", revision: 3 }] as const;
  await ipc.deleteGroup("work", 4, expected);
  await ipc.deleteTarget("top", 1);
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["connection_group_save", { draft: group }],
    ["connection_target_save", { draft: target }],
    ["connection_group_delete", { id: "work", revision: 4, expected }],
    ["connection_target_delete", { id: "top", revision: 1 }],
  ]);
});

it("편집 명령의 구조화된 오류를 그대로 거부한다", async () => {
  const referenced = { kind: "referenced", targets: ["rdp"] };
  vi.mocked(invoke).mockRejectedValue(referenced);
  await expect(createLocalConnectionIpc().deleteTarget("ssh", 1)).rejects.toBe(referenced);
});
