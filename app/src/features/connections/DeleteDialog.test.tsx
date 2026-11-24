// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { DeleteDialog } from "./DeleteDialog";

afterEach(() => { vi.unstubAllGlobals(); });

it("삭제 확인 창이 열리면 첫 포커스를 취소에 둔다", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  Object.assign(HTMLDialogElement.prototype, {
    showModal(this: HTMLDialogElement) { this.setAttribute("open", ""); },
    close(this: HTMLDialogElement) { this.removeAttribute("open"); },
  });
  const host = document.body.appendChild(document.createElement("div"));
  const root = createRoot(host);
  const onDelete = vi.fn(async () => { });
  await act(async () => root.render(<DeleteDialog name="작업 셸" onDelete={onDelete} onClose={vi.fn()} />));
  expect(document.activeElement?.textContent).toBe("취소");
  expect(onDelete).not.toHaveBeenCalled();
  expect(host.querySelector(".delete-contents")).toBeNull();
  await act(async () => root.render(<DeleteDialog name="운영" contents={{ groups: 2, targets: 5 }} onDelete={onDelete} onClose={vi.fn()} />));
  expect([...host.querySelectorAll("dialog h2, dialog p")].map(item => item.textContent)).toEqual(["운영", "하위 그룹 2개, 세션 5개"]);
  await act(async () => root.unmount());
  host.remove();
});
