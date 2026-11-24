/** 우클릭이 쓰는 xterm 기능만 모은 형태다. 시험은 이 형태의 가짜 객체를 넘긴다. */
export interface RightClickTerminal {
  hasSelection(): boolean;
  getSelection(): string;
  clearSelection(): void;
  paste(data: string): void;
  readonly modes: { readonly mouseTrackingMode: string };
}

export type TerminalClipboard = Pick<Clipboard, "readText" | "writeText">;

/**
 * Windows Terminal·PuTTY와 같은 우클릭: 선택 영역이 있으면 복사하고 선택을 지운다. 없으면 붙여넣는다.
 * 마우스 보고 모드(vim, tmux 등)에서는 우클릭을 원격 프로그램에 맡긴다.
 * `canPaste`는 클립보드를 읽은 뒤에 다시 확인한다. 그 사이 탭이 닫히거나 입력이 막힐 수 있다.
 */
export function rightClick(term: RightClickTerminal, clipboard: TerminalClipboard | undefined, canPaste: () => boolean): void {
  if (!clipboard) return;
  if (term.hasSelection()) {
    const selected = term.getSelection();
    term.clearSelection();
    void clipboard.writeText(selected).catch(() => undefined);
    return;
  }
  if (term.modes.mouseTrackingMode !== "none" || !canPaste()) return;
  void clipboard.readText().then((text) => {
    if (text && canPaste()) term.paste(text);
  }).catch(() => undefined);
}
