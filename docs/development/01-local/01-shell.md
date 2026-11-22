# L01 로컬 탭에서 실제 PowerShell과 Bash 실행하기

선행: F00

**사용자 결과:** 가운데 터미널에서 평소 쓰는 셸 명령을 입력하고, 실행 중인 명령만 `Ctrl+C`로 끊은 뒤 같은 프롬프트를 계속 쓴다. 탭을 닫으면 그 탭이 연 셸만 종료된다.

**준비물:** F00의 `app/` 데스크톱 프로젝트, 일반 사용자 계정의 네이티브 Windows 11(PowerShell·ConPTY)과 Ubuntu 24.04 GNOME Wayland(Bash·Unix PTY), 비어 있는 로컬 시험 폴더. 두 OS의 기본 터미널에서 사용할 셸의 경로와 버전을 확인한다. AWS 계정·AI 공급자는 필요 없다. 아래 명령은 티켓 구현 시점에 실행한다. 개발 명령의 기준 디렉터리는 `app/`이다.

## 인수 조건

| Given | When | Then |
|---|---|---|
| 같은 창의 두 로컬 세션 A와 B | A에서 `echo LOCAL_A`, B에서 `echo LOCAL_B` 실행 | 각 셸의 결과와 다음 프롬프트가 해당 탭에만 보이며 두 셸의 PID가 다르다. |
| A의 셸에서 대기 명령 실행 중 | `Ctrl+C`를 누른 뒤 `echo AFTER_INTERRUPT` 실행 | 대기 명령이 멈추고 같은 셸에서 `AFTER_INTERRUPT` 한 줄이 출력된다. |
| A와 B가 실행 중 | A에서 `exit 7` 실행 후 A의 이전 세션 ID로 입력 요청 | A의 마지막 출력 뒤에 종료 코드 7이 전달되고 입력이 거부된다. B는 다음 명령에 응답한다. |
| 셸 경로가 없거나 PTY를 열 수 없는 시험 조건 | 로컬 세션 열기 요청 | 실패한 단계와 OS 오류를 반환한다. 사용할 수 있는 세션 ID나 가짜 프롬프트가 남지 않는다. |

## 개발 순서

1. **Red — 실제 PTY로 실패를 보라.** `app/src-tauri/src/terminal/`에 OS별 통합 시험을 둔다. 작은 PTY를 열고 친 명령 자체에는 없는 표식을 계산해 출력시킨다(두 셸 모두 `echo (7+5)`가 아니라 PowerShell은 `Write-Output (7+5)`, Bash는 `printf '%s\n' "$((7+5))"`). 결과 줄 `12`를 기다린 뒤 `exit 7`의 종료 통지를 검사한다. 읽기에 제한 시간을 두어 실패 단계를 알 수 있게 한다. 두 세션 격리, 없는 실행 파일, 닫은 ID, 다른 창 소유 ID, 0행·0열 요청도 시험한다. `cargo test --manifest-path src-tauri/Cargo.toml terminal`로 대상 시험이 실행되는지 확인한다.
2. **Green — Rust가 셸을 소유하게 한다.** `app/src-tauri/Cargo.toml`의 `portable-pty`로 `native_pty_system().openpty(PtySize)` → slave의 `spawn_command(CommandBuilder)` 순서로 연다. Windows는 설치된 네이티브 PowerShell, Linux는 `/bin/bash`를 대화형으로 직접 기동한다. 실행 파일·인자·작업 디렉터리를 구분하고 실행 뒤 부모의 slave 사본을 놓는다. 파이프에서 명령마다 새 프로세스를 띄우면 TUI·셸 변수·현재 경로가 유지되지 않는다. 열기 중 실패하면 그때까지 획득한 자원을 회수한다.
3. **탭과 세션을 연결한다.** `app/src-tauri/src/terminal/`에서 백엔드가 새 ID를 발급하고, 호출 창·PTY master·writer·child를 함께 관리한다. `app/src-tauri/src/lib.rs`에 열기(초기 행·열, 출력 수신 채널), 입력(ID, 바이트), 크기 변경(ID, 행·열), 닫기(ID)를 등록한다. 열기 요청은 임의 명령문 대신 로컬 셸 선택만 받는다. 다른 요청은 호출 창 소유권과 살아 있는 ID를 확인한 뒤 처리한다. master의 `try_clone_reader`는 전용 읽기 작업에서 사용한다. 막히는 읽기나 `child.wait()` 중에는 세션 목록 잠금을 잡지 않는다.
4. **xterm.js를 입출력 끝점으로 붙인다.** `app/src/features/terminal/`에서 `@xterm/xterm`의 `Terminal`과 `@xterm/addon-fit`을 사용한다. 출력 수신기를 연결한 뒤 세션을 열어 첫 프롬프트도 받는다. xterm `onData`를 UTF-8 바이트로 변환해 해당 세션의 writer에 순서대로 쓰고, PTY 출력은 세션별 [Tauri Channel](https://v2.tauri.app/develop/calling-frontend/#channels)로 바이트 순서대로 `term.write(Uint8Array)`에 전달한다. 에코는 셸에 맡긴다. 크기 변경 시 보이는 터미널에 `fit()`을 적용한 실제 `cols`·`rows`로 master를 resize한다. 탭 전환·숨김은 닫기 요청이 아니다. IME와 연속 출력 부하의 합격 기준은 [L03](03-input.md)에 있다.
5. **종료와 단절을 분리한다.** 정상 종료는 마지막 출력까지 보낸 뒤 자식 대기 결과를 한 번 전달한다. EOF 자체나 출력이 잠시 없는 상태를 종료 코드로 바꾸지 않는다. 읽기·쓰기·IPC 오류는 해당 세션의 단절로 처리한다. 탭 닫기와 창 파괴는 같은 정리 경로에서 입력을 막고, 앱 소유 PTY·자식 및 붙어 있는 전경 작업만 정리하고 자식을 회수한다. 막힌 읽기도 깨어나야 한다. 같은 닫기 요청이 겹쳐도 중복 종료나 다른 세션 종료가 없어야 한다. 셸 전체 이름으로 프로세스를 일괄 종료하지 않는다.

## L02 터미널 뷰 계약

탭마다 `TerminalView({ active, label })`를 고정된 React `key`로 마운트한다. `active`는 표시·포커스·크기 맞춤만 바꾸고 `label`은 접근성 이름에만 쓴다. 비활성 탭을 마운트 상태로 유지하면 그 탭의 셸·출력·입력 구독이 유지된다. 탭을 목록에서 제거하면 뷰가 세션을 닫는다. 열기 응답이 뒤늦게 오면 받은 ID를 시작 전에 닫는다.

L02 상태줄은 `onStatus(status, sessionId?)`에서 현재 탭의 `TerminalStatus`와 Rust가 발급한 실제 ID를 받는다. 첫 `opening`과 ID를 발급받기 전의 열기 실패에는 ID가 없다. `opening`에서 ID가 생긴 뒤에는 `running`과 최종 상태에도 같은 ID가 전달된다. 탭이 닫히면 탭 목록과 함께 상태를 버린다. 상태줄에서 별도 셸이나 출력 구독을 열 필요가 없다.

터미널 입력 면은 `terminal_start` 응답 전과 최종 상태에서 키 입력과 포커스를 차단한다. 시작 응답 전에 도착한 출력은 파싱을 잠시 보류해, ConPTY의 초기 커서 위치 질의 응답도 준비된 세션에 순서대로 전달한다.

Rust는 `terminal_open`에서 초기 행·열과 채널을 받고 호출 창에 묶인 ID를 발급한다. ID를 받은 뒤 `terminal_start`로 출력을 시작한다. `terminal_input`·`terminal_resize`·`terminal_ack`·`terminal_close`는 같은 ID를 받는다. 채널은 최대 16 KiB의 생 바이트와 JSON 종료·단절 상태를 같은 순서로 전달한다. 파서 완료 위치를 누적 ACK로 돌려주면 세션별 256 KiB 읽기 한도가 열린다. 정상 탭 닫기는 종료 코드를 표시하는 자연 종료와 별개다.

Rust는 열기 요청을 받는 즉시 호출 문서 세대를 기록한다. PTY 작업이 시작되기 전에 문서가 교체되면 이전 요청을 거부한다. 열린 세션에 `terminal_start`가 5초 내 도착하지 않으면 그 세션만 회수한다.

WebView가 새 문서를 로드하면 이전 문서의 세션을 닫고 새 문서의 열기를 받는다. 보낸 바이트가 남아 있는데 누적 ACK가 15초 동안 전진하지 않으면 해당 세션을 단절로 정리한다. 확인할 출력이 없는 유휴 세션에는 이 시한을 적용하지 않는다.

## 자동 검증과 실기기 시연

1. Rust 시험과 F00의 Vitest 실행 명령으로 `app/src/features/terminal/`의 입력·출력 연결 시험을 실행한다. 한 세션에서 나온 바이트가 다른 터미널에 전달되지 않는지, 닫기 뒤의 입력이 거부되는지 확인한다. PTY 통합 시험은 양쪽 OS에서 실제 셸로 실행한다.
2. 각 OS에서 `npm run tauri dev`로 시험 창을 연다. 로컬 세션 두 개를 연다(L02 이전에는 같은 셸 뷰를 두 개 띄운 개발 시험 화면으로 검사). 양쪽의 PID를 확인한다(PowerShell: `$PID`, Bash: `echo $$`). 위 A/B 표식을 실행한다. Windows는 `$Host.UI.RawUI.WindowSize`, Linux는 `stty size`로 창을 가로·세로로 바꾸기 전후의 열과 행을 비교한다.
3. A에서 Windows는 `Start-Sleep 60`, Linux는 `sleep 60` 중 `Ctrl+C`를 누른 뒤 인수 조건대로 다음 명령을 실행한다. `exit 7`로 A를 끝내고 B를 계속 쓴다. 새 A를 열어 대기 명령 중 탭을 닫는다. 작업 관리자 또는 `ps`에서 A의 기록한 셸 PID가 사라지고 B는 살아 있는지 확인한다. 시험 창도 닫아 B까지 정리되는지 본다.
4. **완료 증거:** OS별 셸·버전, 두 PID, 리사이즈 전후 행·열, 인터럽트 후 출력, 종료 코드, 탭·창 닫기 결과와 자동 시험 결과를 티켓에 첨부한다. Windows는 개발 WebView의 첫 PowerShell 프롬프트를 관찰했다. Windows 창에서 직접 조작하는 나머지 시연과 Ubuntu GNOME Wayland 실기기 시연은 미실행이다.

참고: [portable-pty 생성·입출력 예제](https://docs.rs/portable-pty/latest/portable_pty/), [제품 설계의 로컬 터미널 기준](../../ops-shell-blueprint.md#개발-순서와-통과-기준).
