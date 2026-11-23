/** 사용자가 명시적으로 요청한 sudo 승격 대상이다. 셸 문자열이 아니라 실행 파일과 인자 배열을 그대로 보낸다. */
export type ElevationRequest = Readonly<{
  program: string;
  args: readonly string[];
  item: string;
}>;

export type ElevationField = Readonly<{ label: string; value: string }>;

/** 승인 화면은 인자 경계를 합치지 않고 하나씩 표시한다. */
export function approvalFields(request: ElevationRequest): readonly ElevationField[] {
  return [
    { label: "대상", value: "내 PC" },
    { label: "실행 계정", value: "root" },
    { label: "실행 파일", value: request.program },
    ...request.args.map((argument, index) => ({ label: `인자 ${index + 1}`, value: argument })),
    { label: "금고 항목", value: request.item },
  ];
}

const ITEM = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function validateElevationRequest(request: ElevationRequest): string | null {
  if (!request.program.startsWith("/")) return "실행 파일은 절대 경로여야 합니다";
  if (!request.program.trim()) return "실행 파일을 입력하세요";
  if (request.args.length > 256) return "인자는 256개까지 지정할 수 있습니다";
  if (request.args.some((argument) => argument.includes("\0") || /[\x00-\x1f\x7f]/.test(argument))) {
    return "인자에 제어 문자를 넣을 수 없습니다";
  }
  if (!ITEM.test(request.item.trim())) return "저장 암호 항목 ID는 36자 UUID여야 합니다";
  return null;
}

/** 한 줄에 하나씩 적은 인자를 빈 항목 없이 배열로 만든다. */
export function parseArguments(text: string): string[] {
  return text.split("\n").map((argument) => argument.trim()).filter(Boolean);
}
