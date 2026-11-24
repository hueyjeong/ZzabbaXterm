const EDITABLE = "input, textarea:not(.xterm-helper-textarea), [contenteditable]:not([contenteditable='false']), .monaco-editor";

/**
 * 웹뷰 기본 메뉴(뒤로·새로 고침·검사)는 앱 동작이 아니므로 막는다. 입력칸과 편집기는 편집 메뉴를 유지한다.
 * Shift를 누른 우클릭은 개발 도구용으로 기본 메뉴를 남긴다.
 */
export function suppressDefaultContextMenu(event: MouseEvent): void {
  if (event.defaultPrevented || event.shiftKey) return;
  if (event.target instanceof Element && event.target.closest(EDITABLE)) return;
  event.preventDefault();
}
