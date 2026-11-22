# F01 검증한 서버에 OpenSSH로 접속하기

선행: C02

**사용자 결과:** 같은 개발 서버를 직접 SSH 또는 SSH-over-SSM으로 열 수 있다. 호스트 키가 바뀌면 서버 로그인을 중단한다. 저장된 비밀번호는 사용자가 대상·계정·항목을 확인한 한 번의 로그인에만 쓰인다.

**준비물:** Windows 11·Ubuntu 24.04 GNOME Wayland의 OpenSSH 클라이언트, C02의 AWS CLI·플러그인, 두 경로가 승인된 비운영 SSH 서버·시험 계정과 SFTP 서브시스템, 관리자가 별도 채널로 제공한 호스트 공개 키와 SHA256 지문. SSH Agent(예: Bitwarden)와 등록한 공개 키, 별도 시험 키 파일, 잠금·취소 가능한 시험용 Bitwarden CLI 및 비밀번호 인증 허용 계정을 준비한다. 개인 금고와 실제 `known_hosts` 대신 시험용 자료를 쓴다. 서버 설치나 계정·호스트 키 변경은 관리자의 별도 승인 작업이다.

| Given (조건) | When (행동) | Then (관찰) |
|---|---|---|
| 지문을 대조한 서버와 등록된 Agent 키가 있다 | 직접, SSH-over-SSM으로 차례로 열고 `whoami`를 입력한다 | 두 탭에서 선택한 SSH 계정이 응답한다. SSM은 전송만 맡고 Agent는 서명만 한다. |
| 실제 서버 키와 시험용 신뢰 기록이 다르다 | 직접 또는 SSM 경로로 연다 | 호스트 키 불일치에서 중단한다. 암호 조회·전송, 키 기록 자동 교체, 다른 인증 방식 재시도는 모두 0회다. |
| 사용자가 저장된 인증을 선택한다 | 대상·계정·항목 확인에서 취소하거나 CLI 금고가 잠긴 채 계속한다 | 취소와 금고 잠김이 구별된다. 비밀번호 전송은 0회이며 일반 셸의 입력 대기는 유지된다. |

## 개발 순서

1. **Red — 동일한 보안 조건을 시험한다.** `app/src/features/ssh/ssh.test.ts`에 실제 호스트·계정·항목 ID 표시, 사용자 취소, SSM 셸 전용 대상의 SFTP 메뉴 부재를 쓴다. `app/src-tauri/src/ssh/`에 SSH와 F02의 SFTP가 소비할 접속 계획의 Rust 시험을 둔다. 직접/SSM 모두 호스트 키 검증이 필수이며, 다른 호스트·서비스·취소된 요청으로 인증 자료를 요구하면 비밀 조회 호출 수가 0이어야 한다.
2. **Green — 서비스와 경로를 분리한다.** Rust는 C01의 대상 ID로 목적지(host/port·SSH 계정), 전송(직접 또는 C02의 profile/region/node), 서비스 인증(Agent·지정한 키 참조·선택한 비밀번호)을 확정한다. 직접 경로는 OS `ssh`, SSM 경로는 OpenSSH의 `ProxyCommand`가 공식 `AWS-StartSSHSession` 문서와 SSH 포트를 사용하게 한다. 프록시에는 고정한 앱 실행기와 Rust가 발급한 식별자만 넘긴다. 실행기가 검증한 값으로 `aws ssm start-session` 인자 배열을 만들고 표준 입출력은 SSH 바이트 전송 전용으로 둔다. 사용자 표시명·설정 문자열을 셸 구문으로 조립하거나 사용자 전역 SSH 설정을 고치지 않는다. 앱 소유 설정(`-F`)을 SSH와 SFTP에 함께 사용한다.
3. **Green — 비밀번호보다 호스트 키를 먼저 확인한다.** 관리자가 제공한 공개 키의 지문을 `ssh-keygen -lf <공개키파일> -E sha256`로 확인하고 실제 서버 신원·포트에 묶어 사용자 전용 신뢰 파일에 등록한다. `StrictHostKeyChecking=yes`, 지정한 `UserKnownHostsFile`과 경로에 관계없이 같은 서버를 가리키는 `HostKeyAlias`를 적용한다. 전역 신뢰 파일이나 DNS 자동 신뢰로 시험 핀이 무력화되지 않게 한다. 최초 미등록·변경은 인증 전 중단하고 별도 채널로 재확인한다. Agent 모드는 선택한 공개 키와 `IdentitiesOnly=yes`를 사용하고, 키 파일 모드는 선택한 파일을 OpenSSH가 직접 연다. Agent 포워딩과 선택 밖 인증 폴백은 끈다.
4. **Green — 암호는 인증 컴포넌트 안에서만 전달한다.** `직접 입력`은 사용자가 직접 입력하는 인증 경로, `저장된 인증 사용`은 로컬 확인 창을 거치는 별도 경로다. 저장 경로에서는 비밀번호 방식만 허용한 OpenSSH의 인증 요청을 전용 `SSH_ASKPASS` 실행기로 받는다. 터미널 출력 문자열을 감시해 답하지 않는다. 고정된 실행기와 해당 자식·대상·계정에 묶인 1회 IPC에서만 요청을 받고, 실제 대상과 항목 ID를 로컬에서 승인받은 뒤 `bw status`와 `bw get password <항목-ID>`를 실행한다. CLI 잠김·CLI 없음·항목 없음·취소를 구별한다. CLI 세션 키가 필요하면 로컬 인증 컴포넌트가 `bw` 자식에만 전달하며 PTY·SSH·AI 자식 환경에서는 제거한다. `bw` 출력은 비공개 파이프로 받아 askpass의 전용 응답 파이프에 한 번만 쓰고 즉시 폐기한다. 비밀을 WebView, argv, 임시 파일, 클립보드, 로그, AI 입력으로 복사하지 않는다. 취소·탭 종료 시 인증 요청을 무효화하고 해당 자식과 프록시만 회수한다. 재시도는 새 요청이다.
   저장 인증용 `ssh` 자식 환경에만 `SSH_ASKPASS=<설치한 실행기의 절대 경로>`와 `SSH_ASKPASS_REQUIRE=force`를 지정한다. `NumberOfPasswordPrompts=1`로 제한하고 그 자식에는 비밀번호 인증 이외의 방식을 끈다. 그래야 제어 터미널이 있는 상태에서도 프롬프트를 사람이 대신 채워 금고 연동 시험이 잘못 통과하는 일을 막을 수 있다. 직접 입력 모드는 이 환경을 상속하지 않는다. 두 OS에서 사람의 터미널 입력 없이 승인 1건·askpass 호출 1회·비밀번호 응답 1회로 로그인하는지 본다. 취소 시 응답은 0회여야 한다. 설치된 OpenSSH가 강제 askpass를 지원하지 않으면 저장 인증 불가 상태를 표시하고 직접 입력을 별도로 선택하게 한다.
5. **자동 → 실기기 시연.** 다음 명령은 후속 구현 시 `app/`에서 실행한다. `npx vitest run src/features/ssh/ssh.test.ts`, `cargo test --manifest-path src-tauri/Cargo.toml ssh`로 검증하고 `npm run tauri dev`로 두 OS에서 직접/SSM의 실제 셸 응답을 확인한다. 한 탭을 닫고 나머지 탭에서 `whoami`를 반복한다. 지정 키·Agent·직접 입력·확인한 시험 항목으로 로그인하고, 그다음 시험 신뢰 파일에 다른 공개 키를 넣어 중단을 재현한다. 확인 취소·CLI 잠김·실행 파일 누락도 격리된 시험용 환경에서 반복한다. `bw status`는 데스크톱 금고 상태로 대체할 수 없다.

**완료 증거:** OS·OpenSSH 버전, 경로/인증별 실제 결과, 키 불일치·취소 때 조회 및 응답 파이프에 쓴 횟수(내용 제외), 닫은 탭의 자식·SSM 세션 회수 결과를 기록한다. T4의 Bitwarden 취소 증거를 [F03](03-elevation.md)에 연결한다. 장비·승인이 없는 조합은 `미실행(사유)`이다.

참고: [OpenSSH 설정](https://man.openbsd.org/ssh_config) · [AWS SSH-over-SSM](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-getting-started-enable-ssh-connections.html) · [Bitwarden SSH Agent](https://bitwarden.com/help/ssh-agent/) / [CLI](https://bitwarden.com/help/cli/)

## SSH 저장 프로필 v1

`defaultMethod: "ssh"`인 대상에는 `ssh`만 저장한다. `ssh`는 실제 서버의 `host`, `port`, `account`, 해당 서버·포트에 등록된 호스트 키를 가리키는 `hostKeyFingerprint`, `transport`, `auth`로 구성된다. `transport`는 `direct` 또는 `ssm`이며, `ssm`에는 기존 C02의 `profile`, `region`, `instanceId` 참조 세 개가 필요하다.

`auth`는 `agent`(`publicKeyPath`), `keyFile`(`privateKeyPath`), `manualPassword`, `storedPassword`(`itemId`) 중 하나다. 호스트 공개 키 파일 경로는 네이티브 선택에만 쓰며 프로필에 저장되지 않는다. 기존 `local`·`ssm` 설정은 v1 그대로 읽는다.

## 직접 SSH 통합 후보

마법사의 `verify_ssh_host`는 Rust에서 검증한 초안의 대상·호스트·포트·지문을 대조한다. 네이티브 파일 선택과 별도 확인을 모두 거친 공개 키만 앱 설정 디렉터리의 `ssh-trust`에 등록한다. SSH-over-SSM과 저장 암호는 이 후보에서 사용 불가다.

저장 대상은 `terminal_open` → `terminal::saved::launch` → `ssh::terminal::DirectLaunch`를 거친다. Rust는 각 PTY ID에 독립 계획을 만들고 자식을 회수한 뒤 임시 파일을 해제한다. 연결 시험은 고정 원격 명령의 출력과 종료 코드를 확인한다. 화면 없는 시험에서 사용자 입력이 필요한 인증은 성공할 수 없다.
