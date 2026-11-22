# M01 · Mac에서 기본 창 버튼과 메인 셸 검증하기

선행: D01

**추가 착수 조건:** 실제 Mac과 시험 계정을 확보한 뒤 시작한다. 이 티켓의 장비 대기는 Windows·Linux 설치본 인수를 막지 않는다.

| Given | When | Then |
|---|---|---|
| macOS 네이티브 창과 로컬 셸 탭 두 개 | 왼쪽 기본 창 버튼을 사용하고 4분할과 집중 보기를 왕복한다 | 기본 버튼이 중복되거나 탭을 가리지 않는다. 뷰 전환은 전체 화면과 별개이며 입력·활성 탭·세션이 보존된다. |
| 실제 macOS 한국어 두벌식과 대화형 CLI | `한` 조합 중 Backspace, 여러 줄 붙여넣기, 응답 취소 후 다시 입력한다 | `하`로 돌아가고 중복 전송이 없다. 종료 뒤 원래 셸에서 `echo TERMINAL_OK`를 실행할 수 있다. |
| GUI 런처에서 시작한 앱과 경로를 찾을 수 없는 외부 도구 | SSM 또는 해당 CLI를 선택한다 | 누락 원인이 보이고 다른 탭과 로컬 셸은 유지된다. 인증 실패와 실행 파일 누락을 구별한다. |

1. **Red:** [macOS 사전 요구 사항](https://v2.tauri.app/start/prerequisites/#macos)대로 Xcode Command Line Tools, Rust stable, 호환 Node.js/npm을 준비한다. `sw_vers`, CPU 아키텍처, 키보드 입력 소스와 기본 셸을 기록한다. Windows·Linux 증거는 macOS 통과로 복사하지 않는다.
2. **Green:** 동일 소스를 `app/`에서 `npm ci`, `npm test`, `cargo test --locked --manifest-path src-tauri/Cargo.toml`, `npm run tauri dev`로 연다. 플랫폼 차이는 창 제어·WKWebView·Unix PTY·도구/소켓 경로에서 먼저 고친다. 왼쪽 네이티브 신호등 공간은 유지하고 오른쪽 복제 버튼은 제거한다. 입력 버그는 임의 한글 키 치환 대신 PTY 바이트와 조합/확정 경계를 대조한다.
3. **네이티브 시연:** T8의 핸들·포커스·패널 복원을 macOS 창 관례로 확인하고 T1 로컬/SSM, T2 네 CLI, T3 SSM 세션 순으로 같은 기준을 적용한다. Apple 키보드의 `delete`는 뒤로 지우기 동작으로 시험한다. 다른 원격 기능도 macOS 지원으로 표기하려면 해당 T4–T7을 Mac에서 재현한다. 승인된 비운영 계정·SSO가 없으면 그 칸은 `미실행(사유)`이다.
4. **인수:** D01의 보조 실행 파일 준비 절차를 Mac의 실제 아키텍처로 수행하고 `npm run tauri build -- --config src-tauri/tauri.bundle.conf.json --bundles app`으로 로컬 시험용 `.app`을 만든다. Finder에서 열어 언어 입력, GUI에서의 PATH·Agent 소켓 감지, 앱에 포함된 CLI 기동, 창 종료 후 PTY 정리를 기록한다. 외부 배포용 서명·공증은 조직이 별도로 결정한다.

**완료 증거:** Mac 모델·OS 버전, T1–T3 및 창 동작 기록, CLI별 결과, `.app` 기동 결과, 원격 기능의 검증 범위. macOS 결과는 Windows·Linux 1차 기록과 별도로 남긴다.
