# C02 회사 SSO로 관리 노드를 찾아 메인 셸 열기

선행: C01

**사용자 결과:** 프로필·리전·관리 노드를 고르면 SSH 키 없이 메인 탭이 열린다. 같은 노드에 탭을 두 개 열어도 입력과 종료는 탭별로 독립한다.

**준비물:** 네이티브 Windows 11(한국어 두벌식)·Ubuntu 24.04 GNOME Wayland(IBus Hangul 두벌식), 두 PC의 AWS CLI v2와 공식 Session Manager 플러그인, IAM Identity Center 시험 프로필, UTF-8 대화형 셸이 있는 승인된 비운영 온라인 노드. 관리자에게 목록 조회·세션 시작·자기 세션 종료와 검증용 세션 상태 조회 범위를 확인한다. 빈 리전, 조회가 거부된 시험 프로필, 만료된 별도 시험 로그인, 오프라인 시험 노드를 준비한다. 설치·권한 변경·Agent 정지는 앱 밖에서 별도 승인받는다.

| Given (조건) | When (행동) | Then (관찰) |
|---|---|---|
| 선택한 프로필·리전에 온라인 노드가 있다 | 마법사에서 시험·저장 후 같은 노드를 두 번 연다 | 독립된 두 탭에서 `whoami`가 응답한다. 첫 탭을 닫은 뒤에도 둘째 탭은 입력을 받고 첫 AWS 세션만 끝난다. |
| 시험 로그인이 만료됐다 | 목록을 요청하고 재로그인을 선택한다 | 만료 상태와 해당 프로필이 보인다. 사용자가 브라우저 인증을 끝내면 같은 프로필·리전으로 다시 조회한다. |
| 조회 성공 0건 / IAM 거부 / 노드 연결 끊김 / 플러그인 누락 | 각각 조회 또는 열기를 시도한다 | 빈 목록 / 권한 거부 / 노드 연결 불가 / 로컬 플러그인 없음으로 구분한다. 실패한 연결의 입력과 선택은 남는다. |

## 개발 순서

1. **Red — 실패 위치부터 구별한다.** `app/src/features/ssm/ssm.test.ts`에 프로필 전환 중 예전 조회 응답, 빈 목록과 조회 오류, 탭별 종료 이벤트를 넣는다. `app/src-tauri/src/ssm/`의 Rust 시험은 CLI 종료 코드·서비스 오류 코드·목록 JSON을 입력으로 삼는다. `AccessDeniedException`, `TargetNotConnected`, 만료, 실행 파일 누락, 분류할 수 없는 실패를 서로 다른 결과로 기대한다. 미분류 오류를 만료나 0건으로 바꾸면 실패다.
2. **Green — 마법사의 선택을 실제 계정에 연결한다.** Rust에서 `aws configure list-profiles`로 후보를 읽고, 선택한 프로필의 `aws configure get region --profile <profile>` 값을 리전 선택의 초깃값으로 쓴다. 리전이 없으면 사용자가 승인된 리전을 지정한다. `aws ssm describe-instance-information --profile <profile> --region <region> --output json --no-cli-pager`의 자동 페이지 처리를 유지하고 `InstanceInformationList`의 `InstanceId`, `PingStatus`를 읽는다. `Online`만 시작 가능하다. 표시명은 노드 ID를 대신할 수 없다. 모든 AWS 작업에 같은 프로필·리전을 명시하고 자격 증명·토큰 파일의 처리는 CLI에 맡긴다. 명령은 셸 문자열 대신 실행 파일과 인자 배열로 만든다.
3. **Green — 복구 동작을 오류에 맞춘다.** SSO 만료일 때만 로컬 UI의 재로그인 동작으로 `aws sso login --profile <profile>`을 시작한다. 완료 뒤 목록을 다시 조회한다. IAM 거부에 로그인 반복, 빈 목록에 자동 리전 전환은 하지 않는다. `session-manager-plugin --version`의 실행 파일 부재를 서비스 실패와 구별한다. 프로필·리전 변경이나 마법사 취소는 기존 조회의 결과를 폐기한다.
4. **Green — 같은 터미널 경로를 연다.** `app/src-tauri/src/ssm/`에서 공식 `aws ssm start-session --profile <profile> --region <region> --target <instance-id>`를 기존 PTY로 시작한다. C01의 연결 시험도 실제 세션을 열어 응답을 확인하고 시험 소유 세션을 닫은 뒤 결과를 돌려준다. 실제 탭은 Rust 발급 앱 세션 ID별로 PTY·자식 핸들과 프로필·리전·노드를 소유한다. AWS 세션 ID와 앱 ID는 다른 값이다. 소유권을 확인할 수 있도록 시작 시 `--reason`에 앱이 만든 요청별 난수를 넣고 `DescribeSessions`의 Reason·Owner·Target과 대조해 정확히 일치한 AWS 세션 ID만 취급한다. 원격 화면 텍스트는 소유권 근거가 아니다. 탭 닫기·취소 시 그 세션만 정상 종료하고 자식과 플러그인을 회수한다. 강제 단절 뒤 종료가 불분명하면 소유권이 확인된 ID만 `terminate-session` 대상으로 삼고 상태를 재조회한다. 조회나 종료 권한이 없으면 `종료 확인 불가`와 대상만 남긴다. 같은 노드 전체 또는 프로세스 이름으로 정리하지 않는다.
5. **자동 검증 후 T3를 시연한다.** 모든 명령은 후속 구현 시 실행한다. `app/`에서 `npx vitest run src/features/ssm/ssm.test.ts`, `cargo test --manifest-path src-tauri/Cargo.toml ssm`의 실제 시험 건수를 기록하고 `npm run tauri dev`로 앱을 연다. 각 OS의 일반 터미널에서 4단계 기준선 명령의 꺾쇠 값을 시험 값으로 치환해 `whoami` 결과를 확인하고 `exit`한다. 앱 마법사와 두 탭에서 같은 노드로 반복한다. 한 탭을 닫은 뒤 `aws ssm describe-sessions --state History --filters key=SessionId,value=<종료한-AWS-ID> --profile <profile> --region <region>`의 `Terminated`와 남은 탭의 새 `whoami` 응답을 대조한다. 실패 조건은 각각 준비한 시험 환경에서 재현하며 다른 사용자의 캐시·권한·Agent를 바꾸지 않는다.
6. **T1 원격을 별도로 판정한다.** 각 OS의 실제 SSM 탭에서 앞에 `A`를 입력한 뒤 물리 두벌식 키로 `ㅎ → ㅏ → ㄴ → Backspace`를 눌러 `한 → 하`를 본다. `ㄴ`과 공백으로 확정해 `A한 `이 되고, Backspace 두 번으로 `A`만 남아야 한다. 입력은 제출하지 말고 취소한다. 앱 내부 합성 이벤트나 로컬 PTY 결과로 대체하지 않는다.

### 검색 IPC 통합

Tauri 빌더는 `ssm::ipc::lifecycle()`을 플러그인으로 등록한다. 앱 호출 처리기는 `ssm::ipc::COMMAND`를 받으면 `ssm::ipc::dispatch(invoke)`를 동기적으로 호출하고 `true`를 반환한다. 빌드의 `AppManifest`는 `ssm_discovery`를 등록하며, `allow-ssm-discovery`는 로컬 `main` 웹뷰에만 부여한다. 수명 플러그인에는 별도 호출 명령이 없다.

JS는 문서에서 `window.__ZZABBA_SSM_DOCUMENT__`를 읽는다. 아직 값이 없으면 `zzabba:ssm-document`를 기다린 뒤 읽고, 대기 중 취소되면 호출하지 않는다. 호출 형식은 `invoke("ssm_discovery", { document, request })`이며, 하나의 작업에는 시작 시 읽은 문서 값을 끝까지 사용한다.

| `request` | 성공 값 |
|---|---|
| `{ action: "reserve" }` | 문자열 `requestId` |
| `{ action: "run", requestId, operation: { kind: "listProfiles" } }` | SSO 후보 `string[]` |
| `{ action: "run", requestId, operation: { kind: "configuredRegion", profile } }` | `string \| null` |
| `{ action: "run", requestId, operation: { kind: "listOnlineNodes", profile, region } }` | `{ instanceId: string }[]` |
| `{ action: "relogin", requestId }` | `null` |
| `{ action: "cancel", requestId }` | `null` |

명령 실패는 `{ code }`로 거부된다. `SsoLoginRequired`를 반환한 노드 조회만 해당 ID로 한 번 재로그인할 수 있다. 사용자가 재로그인을 고르면 같은 ID를 보내고, 성공하면 새 ID로 같은 프로필·리전을 조회한다. 취소가 ID 발급보다 먼저 오면 발급 직후 `cancel`만 호출한다. 프로필·리전 전환 또는 마법사 닫기에서는 대기 중인 요청과 미사용 만료 ID를 `cancel`로 정리한다. 문서 전환·창 종료에서는 Rust가 해당 문서의 요청을 취소한다.

격리 시험: `cargo test --manifest-path app/src-tauri/Cargo.toml ssm::ipc::tests`.

### 연결 프로필과 시험 자격

마법사의 `AWS Systems Manager` 방식은 SSO 프로필, 리전, 온라인 노드를 선택한다. 리전을 수정한 뒤에는 `리전 적용`을 눌러 새 범위에서 노드를 다시 선택해야 시험 단계로 넘어갈 수 있다. 프로필·리전·노드를 바꾸면 이전 시험과 저장 자격이 취소된다. 시험 실패 후에도 선택한 값은 남으며, 시험·저장 화면에는 표시명과 별도로 SSO 프로필, 리전, 인스턴스 ID가 표시된다.

저장 스키마는 버전 1을 유지한다. 대상은 기존 `id`, `groupId`, `displayName`과 다음 두 방식 중 하나로 구성한다.

- 로컬: `defaultMethod: "local"`, `local: { shell, startDirectory }`.
- SSM: `defaultMethod: "ssm"`, `ssm: { profile, region, instanceId }`.

Rust는 두 방식이 섞인 레코드와 비밀·실행 파일·임의 인자 등 정의되지 않은 필드를 거부한다. 마법사는 [C01 시험·저장 IPC](01-profiles.md#로컬-마법사-ipc)를 그대로 사용한다. 현재 초안의 실제 연결 시험이 성공한 경우에만 저장할 수 있다. 저장된 SSM 행을 열 때 `terminal_open`의 연결 인자는 `targetId`뿐이며, Rust가 파일에서 대상의 방식을 다시 읽는다.

`terminal_open`이 PTY ID를 발급하면 SSM은 해당 ID의 `terminal_start`가 도착한 뒤에만 시작한다. 연결 시험은 같은 PTY에서 Linux 셸 표식과 종료 코드 0을 확인하고, 서비스 이력에서 종료까지 확인한 뒤 C01 저장 자격을 발급한다. `terminal_close`는 로컬 프로세스를 회수한 뒤 소유한 원격 세션의 종료 결과를 반환한다. 권한 거부 등으로 확인할 수 없는 경우 결과는 `종료 확인 불가: <대상>`이다.

**완료 증거:** `Windows SSM T1 / Ubuntu SSM T1 / Windows T3 / Ubuntu T3` 네 결과에 OS·WebView·입력기·CLI/플러그인 버전과 실제 관찰을 적는다. T3에는 실패별 화면 상태와 마법사 시험·탭별 세션 정리 근거를 포함한다. 계정·노드·세션 ID는 일관된 별칭으로 가리고 토큰·로그인 URL은 제외한다. 접근할 수 없는 조합은 `미실행(사유)`, macOS는 후속 실기기 검증이다.

참고: [Session Manager 시작](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-sessions-start.html) · [세션 상태 조회](https://docs.aws.amazon.com/cli/latest/reference/ssm/describe-sessions.html)
