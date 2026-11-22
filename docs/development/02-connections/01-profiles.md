# C01 그룹 트리에서 로컬 연결을 시험하고 저장하기

선행: L03

**사용자 결과:** `회사 개발 → 작업 셸`을 선택하면 같은 이름의 다른 대상이 아니라 내가 시험한 로컬 셸이 열린다. 연결을 시험하다 취소하거나 실패해도 기존 트리는 그대로다.

**준비물:** L03의 로컬 탭·PTY, Windows 11의 PowerShell, Ubuntu 24.04 GNOME Wayland의 Bash, 쓰기 가능한 시험 디렉터리와 존재하지 않는 경로 하나. 실제 사용자 설정과 분리한 앱 설정 디렉터리를 쓴다. AWS 도구나 계정은 이 티켓의 준비물이 아니다.

| Given (조건) | When (행동) | Then (관찰) |
|---|---|---|
| 같은 표시명의 대상 두 개가 서로 다른 그룹에 있다 | 한 행을 선택해 Enter 또는 더블클릭한다 | 선택한 대상 ID의 기본 경로로 탭 하나가 열린다. 문맥 메뉴에는 그 대상에 설정된 방식만 있다. |
| 새 로컬 대상의 초안이 있다 | 유효한 시작 디렉터리로 연결을 시험한 뒤 저장하고 앱을 다시 연다 | 실제 셸이 응답했던 대상과 기본 방식이 같은 그룹에 남는다. 시험용 프로세스는 이미 종료됐다. |
| 시험에 성공한 뒤 시작 경로를 없는 디렉터리로 바꾼다 | 다시 시험하고, 실패 화면에서 취소한다 | 이전 성공은 무효다. 실제 시작 실패와 입력값이 남고, 취소 전후 저장 파일과 트리는 동일하다. |

## 개발 순서

1. **Red — 초안과 저장본을 구별한다.** `app/src/features/connections/profiles.test.ts`에 취소·시험 실패·값 수정 후 이전 성공 결과 도착·동명 대상 선택 사례를 쓴다. `app/src-tauri/src/connections/`에 허용하지 않은 필드(예: `password`, `accessToken`)를 보낸 초안의 거부, 실패한 저장 후 이전 파일을 다시 읽는 Rust 시험을 먼저 추가한다. 직렬화할 때 비밀 필드 값을 빈 문자열로 바꾸는 구현도 실패여야 한다.
2. **Green — 데이터 경계를 만든다.** Rust가 그룹 ID·대상 ID, 표시명, 실제 대상, 접속 방식 목록과 기본 방식, 방식별 경로·서비스 인증 참조를 검증한다. 로컬 방식은 지원 셸과 시작 디렉터리만 받는다. 셸 명령 문자열이나 임의 프로그램 실행 옵션은 받지 않는다. 후속 방식에서도 AWS 프로필 이름, 키 참조·지문, 금고 항목 ID만 저장하고 비밀 값은 입력 스키마에서 제외한다. 저장 시 사용자 전용 앱 설정 경로에서 임시 파일을 완전히 쓴 뒤 기존 파일을 원자적으로 교체한다. 기록 실패는 저장 실패다.
3. **Green — 마법사를 실제 PTY에 연결한다.** `app/src/features/connections/`에서 `+ → 대상 → 접속 방식 → 인증 → 연결 테스트·저장` 순으로 한 초안을 편집한다. 로컬 인증 값은 `현재 사용자`다. Rust는 선택한 셸과 작업 디렉터리로 시험 전용 PTY를 열어 고정된 시험 문자열을 출력하는 셸 명령의 결과와 정상 종료를 확인한다(입력 에코를 성공으로 세지 않는다). 시간 초과·취소·창 닫기에는 해당 자식만 종료·회수한다. Rust에서 시험 결과를 초안 버전에 묶고, 현재 버전이 성공한 경우에만 로컬 UI의 별도 저장 동작을 받는다. 후속 CLI도 같은 초안 형식을 쓰며 저장 권한은 UI에 남긴다. AWS 방식의 실연결 시험은 [C02](02-ssm.md)가 맡는다.
4. **Green — 저장본으로 트리를 그린다.** 그룹 아래 대상 행을 표시하고 검색 결과도 원래 그룹에 둔다. 기존 트리 폭을 쓰고 글자 15px·행 36px·행 전체 선택을 적용한다. 화살표로 이동·접기·펼치기, Enter·더블클릭으로 기본 연결, 문맥 메뉴로 설정된 다른 방식을 연다. 트리에는 영구 대상 ID, 열리는 탭에는 매번 Rust가 발급한 세션 ID를 쓴다.
5. **실기기에서 판정한다.** 아래 명령은 모두 후속 구현 시 `app/`에서 실행한다. `npx vitest run src/features/connections/profiles.test.ts`와 `cargo test --manifest-path src-tauri/Cargo.toml connections`에서 1단계 사례가 실제로 실행되는지 확인한다. `npm run tauri dev`로 두 OS에서 각각 정상 경로 시험·저장·재시작과 실패 경로 시험·취소를 반복한다. 정상 행을 열어 PowerShell은 `Get-Location`, Bash는 `pwd`로 선택했던 시작 경로를 대조한다.

**완료 증거:** OS별로 정상 경로, 실패 이유, 재시작 후 열린 대상, 취소 전후 설정의 동일 여부와 시험용 자식 회수 결과를 남긴다. 저장 파일은 값 대신 필드 이름만 첨부한다. AWS 연결 결과는 이 증거에 포함하지 않는다.

## 로컬 마법사 IPC

Tauri의 로컬 `main` 창에서만 호출한다. `Run`은 `{ draftId: string, draftVersion: number, runId: string }`이다. 마법사가 편집 식별자와 버전을 넘기면 Rust가 `runId`를 발급한다. `draftVersion`은 0 이상의 안전한 정수이며 파일 스키마 버전과 별개다.

| 명령 | 인자 | 성공 응답 |
|---|---|---|
| `connection_test_start` | `{ draftId, draftVersion, draft }` | `Run` |
| `connection_test_wait` | `{ run: Run }` | `Run & { sessionId: string }` |
| `connection_test_cancel` | `{ run: Run }` | `null` |
| `connection_save` | `{ request: { run: Run, draft } }` | `null` |
| `connections_load` | `{}` | `{ version, groups: [{ revision, profile }], targets: [{ revision, profile }] }` |

`draft`는 `{ version: 1, groupRevision, targetRevision, group, target }`이다. 새 항목의 개정은 `null`이고, 저장된 항목을 수정할 때는 목록 응답의 개정을 넘긴다. `group`은 `{ id, displayName }`, `target`은 `{ id, groupId, displayName, defaultMethod: "local", local: { shell, startDirectory } }`다. `shell`은 Windows의 `"powerShell"` 또는 Ubuntu의 `"bash"`이며, `startDirectory`는 절대 경로다. 로컬 인증은 현재 사용자이므로 인증 필드는 없다.

1. `start`에 현재 초안을 넘긴 뒤 반환된 `Run`을 `wait`에 그대로 전달한다. `wait`의 성공 응답은 시험 표식과 정상 종료를 확인하고 PTY를 회수한 뒤 반환한다. 호출 실패는 문자열로 거절된다.
2. 편집하거나 마법사를 닫으면 성공 여부와 무관하게 이전 `Run`을 취소한다. `start` 응답이 늦으면 도착한 `Run`을 즉시 취소한다. 재시험은 같은 문서의 같은 초안에서 이전 실행을 대체한다. 문서 이동과 창 닫기도 해당 문서의 실행을 취소한다.
3. 사용자가 성공한 초안에서 저장을 누를 때만 `connection_save`를 호출한다. Rust는 현재 문서, 실행 ID, 편집 버전, 시험한 초안 전체를 대조하고 저장 성공 후 실행 자격을 소진한다. 저장 성공 뒤 `connections_load`로 새 개정과 목록을 받는다.
