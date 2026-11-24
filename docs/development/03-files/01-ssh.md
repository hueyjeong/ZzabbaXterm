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
2. **Green — 서비스와 경로를 분리한다.** Rust는 C01의 대상 ID로 목적지(host/port·SSH 계정), 전송(직접 또는 C02의 profile/region/node), 서비스 인증(Agent·지정한 키 참조·선택한 비밀번호)을 확정한다. 직접 경로와 SSM 경로 모두 OS `ssh`를 쓰고 `ProxyCommand none`을 유지한다. SSM 경로는 앱이 소유한 SSM 포트 전달([R01](../04-remote/01-tunnels.md)의 `AWS-StartPortForwardingSession`)을 먼저 열고, OpenSSH는 그 루프백 수신 포트로 접속한다. `ssh`가 띄운 `aws` 프로세스는 앱이 세션 소유권을 확인하거나 종료를 보장할 수 없으므로 `ProxyCommand`로 SSM 세션을 열지 않는다. 사용자 표시명·설정 문자열을 셸 구문으로 조립하거나 사용자 전역 SSH 설정을 고치지 않는다. 앱 소유 설정(`-F`)을 SSH와 SFTP에 함께 사용한다.
3. **Green — 비밀번호보다 호스트 키를 먼저 확인한다.** 관리자가 제공한 공개 키의 지문을 `ssh-keygen -lf <공개키파일> -E sha256`로 확인하고 실제 서버 신원·포트에 묶어 사용자 전용 신뢰 파일에 등록한다. `StrictHostKeyChecking=yes`, 지정한 `UserKnownHostsFile`과 경로에 관계없이 같은 서버를 가리키는 `HostKeyAlias`를 적용한다. 전역 신뢰 파일이나 DNS 자동 신뢰로 시험 핀이 무력화되지 않게 한다. 최초 미등록·변경은 인증 전 중단하고 별도 채널로 재확인한다. Agent 모드는 선택한 공개 키와 `IdentitiesOnly=yes`를 사용하고, 키 파일 모드는 선택한 파일을 OpenSSH가 직접 연다. Agent 포워딩과 선택 밖 인증 폴백은 끈다.
4. **Green — 암호는 인증 컴포넌트 안에서만 전달한다.** `직접 입력`은 사용자가 직접 입력하는 인증 경로, `저장된 인증 사용`은 로컬 확인 창을 거치는 별도 경로다. 저장 경로에서는 비밀번호 방식만 허용한 OpenSSH의 인증 요청을 전용 `SSH_ASKPASS` 실행기로 받는다. 터미널 출력 문자열을 감시해 답하지 않는다. 고정된 실행기와 해당 자식·대상·계정에 묶인 1회 IPC에서만 요청을 받고, 실제 대상과 항목 ID를 로컬에서 승인받은 뒤 `bw status`와 `bw get password <항목-ID>`를 실행한다. CLI 잠김·CLI 없음·항목 없음·취소를 구별한다. CLI 세션 키가 필요하면 로컬 인증 컴포넌트가 `bw` 자식에만 전달하며 PTY·SSH·AI 자식 환경에서는 제거한다. `bw` 출력은 비공개 파이프로 받아 askpass의 전용 응답 파이프에 한 번만 쓰고 즉시 폐기한다. 비밀을 WebView, argv, 임시 파일, 클립보드, 로그, AI 입력으로 복사하지 않는다. 취소·탭 종료 시 인증 요청을 무효화하고 해당 자식과 프록시만 회수한다. 재시도는 새 요청이다.
   저장 인증용 `ssh` 자식 환경에만 `SSH_ASKPASS=<설치한 실행기의 절대 경로>`와 `SSH_ASKPASS_REQUIRE=force`를 지정한다. `NumberOfPasswordPrompts=1`로 제한하고 그 자식에는 비밀번호 인증 이외의 방식을 끈다. 그래야 제어 터미널이 있는 상태에서도 프롬프트를 사람이 대신 채워 금고 연동 시험이 잘못 통과하는 일을 막을 수 있다. 직접 입력 모드는 이 환경을 상속하지 않는다. 두 OS에서 사람의 터미널 입력 없이 승인 1건·askpass 호출 1회·비밀번호 응답 1회로 로그인하는지 본다. 취소 시 응답은 0회여야 한다. 설치된 OpenSSH가 강제 askpass를 지원하지 않으면 저장 인증 불가 상태를 표시하고 직접 입력을 별도로 선택하게 한다.
5. **자동 → 실기기 시연.** 다음 명령은 후속 구현 시 `app/`에서 실행한다. `npx vitest run src/features/ssh/ssh.test.ts`, `cargo test --manifest-path src-tauri/Cargo.toml ssh`로 검증하고 `npm run tauri dev`로 두 OS에서 직접/SSM의 실제 셸 응답을 확인한다. 한 탭을 닫고 나머지 탭에서 `whoami`를 반복한다. SSM 탭을 닫은 뒤 `aws ssm describe-sessions --state History`로 해당 포트 전달 세션의 종료를 확인한다. 지정 키·Agent·직접 입력·확인한 시험 항목으로 로그인하고, 그다음 시험 신뢰 파일에 다른 공개 키를 넣어 중단을 재현한다. 확인 취소·CLI 잠김·실행 파일 누락도 격리된 시험용 환경에서 반복한다. `bw status`는 데스크톱 금고 상태로 대체할 수 없다.

**완료 증거:** OS·OpenSSH 버전, 경로/인증별 실제 결과, 키 불일치·취소 때 조회 및 응답 파이프에 쓴 횟수(내용 제외), 닫은 탭의 자식·SSM 세션 회수 결과를 기록한다. T4의 Bitwarden 취소 증거를 [F03](03-elevation.md)에 연결한다. 장비·승인이 없는 조합은 `미실행(사유)`이다.

참고: [OpenSSH 설정](https://man.openbsd.org/ssh_config) · [AWS SSH-over-SSM](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-getting-started-enable-ssh-connections.html) · [Bitwarden SSH Agent](https://bitwarden.com/help/ssh-agent/) / [CLI](https://bitwarden.com/help/cli/)

## SSH 저장 프로필 v1

`defaultMethod: "ssh"`인 대상에는 `ssh`만 저장한다. `ssh`는 실제 서버의 `host`, `port`, `account`, 해당 서버·포트에 등록된 호스트 키를 가리키는 `hostKeyFingerprint`, `transport`, `auth`로 구성된다. `transport`는 `direct` 또는 `ssm`이며, `ssm`에는 기존 C02의 `profile`, `region`, `instanceId` 참조 세 개가 필요하다.

`auth`는 `agent`(`publicKeyPath`), `keyFile`(`privateKeyPath`), `manualPassword`, `storedPassword`(`itemId`) 중 하나다. 호스트 공개 키 파일 경로는 네이티브 선택에만 쓰며 프로필에 저장되지 않는다. 기존 `local`·`ssm` 설정은 v1 그대로 읽는다.

## 직접 SSH 통합 후보

마법사의 `verify_ssh_host`는 Rust에서 검증한 초안의 대상·호스트·포트·지문을 대조한다. 기본 앱 창에 연결된 네이티브 파일 선택과 확인을 거치면 Rust가 문서·초안 세대에 묶인 일회 예약을 반환한다. `commit_ssh_host`는 유효한 예약만 소비해 앱 설정 디렉터리의 `ssh-trust`에 등록한다. 마법사 종료, 초안 변경, 페이지 이동은 대기 중인 예약을 취소한다.

Agent 공개 키는 열린 파일의 핸들로 링크 여부, 정규 파일 여부, 16 KiB 상한을 검사한 뒤 읽는다. 저장 암호 대상은 터미널 탭의 승인 경로로 연다.

## SSH-over-SSM 경로

`transport: "ssm"` 대상은 `ssh::terminal`이 접속 계획을 만들기 전에 다음 순서로 전송을 연다.

1. 등록된 호스트 키 PIN을 먼저 확인한다. PIN이 없거나 다르면 AWS 호출 없이 중단한다.
2. `tunnels::ssm::open_ssh_forward`가 저장된 `profile`·`region`·`instanceId`로 `Route::Ssm { listen: 127.0.0.1:<빈 포트>, node: instanceId, port: ssh.port, host: None }` 전달을 연다. 문서는 `AWS-StartPortForwardingSession`, 매개변수는 `portNumber=<SSH 포트>,localPortNumber=<빈 포트>`다. 자격 내보내기, 노드 온라인 확인, 플러그인 확인, 세션 소유권(DescribeSessions) 확인, 수신 소켓 소유권 확인은 포트 전달 탭과 같다.
3. 빈 포트는 루프백에서 bind한 뒤 놓아 고른다. 플러그인이 bind 충돌로 끝나면 새 포트로 최대 3회까지 다시 연다. 그 밖의 시작 실패는 다시 시도하지 않는다.
4. 수신이 확인되면 접속 계획의 `ssh_config`에 `HostName 127.0.0.1`, `Port <전달 포트>`를 쓰고 `HostKeyAlias`는 원래 대상의 `[host]:port`로 둔다. 그래서 저장된 PIN과 `StrictHostKeyChecking yes`, `ProxyCommand none` 등 나머지 설정이 직접 경로와 같게 적용된다. SSM 전송에만 `ServerAliveInterval 60`, `ServerAliveCountMax 3`을 더해 입력 없이 열어 둔 셸이 SSM 세션 유휴 제한(기본 20분)으로 끊기지 않게 한다.

SSM 전달(`SsmChild`)은 접속 계획(`ConnectionPlan`)이 소유한다. 터미널 탭, 같은 탭의 SFTP 파일 목록, 원격 nano 편집기가 같은 계획을 공유하므로 모두 같은 전달로 접속한다. 이때 SSH·SFTP·nano는 한 포트 전달에 각각 TCP 연결로 붙으므로 노드의 SSM Agent가 포트 전달 다중 연결(TCP multiplexing)을 지원해야 한다. 이 기능은 SSM Agent 3.0.222.0에서 추가되었고, 클라이언트 Session Manager plugin은 1.2 이상이어야 한다([SSM Agent 릴리스 노트](https://github.com/aws/amazon-ssm-agent/blob/mainline/RELEASENOTES.md), [AWS 발표](https://aws.amazon.com/about-aws/whats-new/2020/10/port-forwarding-sessions-created-sessions-manager-support-multiple-simultaneous-connections)). 탭을 닫거나 `ssh`가 끝나 계획이 해제되면 로컬 CLI·플러그인 트리를 정리하고 같은 요청의 원격 세션을 `terminate-session`으로 닫는다. 전달이 먼저 끝나면 터미널 감독자가 `ssh` 자식을 정리하고 탭에 `SSM 전달 종료: <사유>`를 표시한다.

터미널 탭과 원격 nano 탭은 PIN만 확인하고 바로 열려 `연결 중`을 표시하며, 전달 준비는 PTY 작업자가 탭의 취소 토큰으로 진행한다. 준비 중 탭 닫기·문서 전환·창 닫기·앱 종료는 준비를 즉시 멈추고 이미 시작한 SSM 자식과 원격 세션을 같은 정리 경로로 닫으며, 준비가 늦게 끝나도 `ssh`를 시작하지 않는다. 준비 실패는 `열기 실패: <오류>`로 표시한다.

저장 전 연결 시험도 같은 경로를 쓴다. 시험을 취소하거나 시간이 초과되면 전달 준비를 중단한다. 호스트 키 등록은 관리자가 제공한 공개 키 파일만 읽으므로 네트워크 접속이 없고 SSM 대상도 원래 `[host]:port` 별칭으로 등록한다. 저장 암호 대상도 같은 전달 위에서 승인 경로로 연다. 승인 창(`전송: SSM · <profile> · <region> · <instanceId>` 줄 포함)을 먼저 띄우고, 승인 뒤 탭을 등록한 다음 탭의 취소 토큰 아래에서 전달을 연다. 전달 뒤 계획을 만들 때 등록된 PIN을 다시 확인하고, 계획의 스냅샷이 그 PIN을 계획 수명 동안 고정한다. 승인 직후부터 금고 승인을 묶기 직전까지 승인 기한(120초)을 넘기면 만료로 끝내고 금고를 쓰지 않는다.

**실제 노드 시험.** `tunnels::ssm::tests::live`의 무시 시험 두 개가 실제 SSM 노드에서 이 경로를 확인한다. `live_ssh_over_ssm_end_to_end`는 `open_ssh_forward` → `ConnectionPlan::prepare(KeyFile)`로 `ssh` 원격 명령, 같은 계획의 동시 SFTP 목록, 다른 키 PIN의 호스트 키 거부, 계획 해제 뒤 세션 `Terminated`를 확인한다. `live_port_forward_preserves_bytes`는 `ForwardPlan` → `prepare` → `spawn`으로 노드 에코 서버(`tests/ssm_forward_bytes.py node-params`)까지의 SHA-256 왕복을 확인한다. 대상은 `ZZABBA_LIVE_PROFILE`·`ZZABBA_LIVE_REGION`·`ZZABBA_LIVE_NODE`·`ZZABBA_LIVE_KEY`(개인 키 경로)·`ZZABBA_LIVE_HOSTKEY`(노드 호스트 공개 키 파일)로 받고, `app/src-tauri`에서 `cargo test --lib tunnels::ssm::tests::live -- --ignored --nocapture --test-threads=1`로 실행한다. 직접 SSO 프로필이나 `aws login` 프로필이 아니면 시험 안에서만 자격 내보내기를 사용자 AWS 설정으로 우회한다.

다음은 SSH-over-SSM 대상에서 명시적 오류로 남는다: SSH 포트 전달 탭, 원격 sudo 승격, AI CLI 등록, 원격 화면의 SSH 경로.

**저장 인증 기반의 현재 상태(비기본 기능).** `stored-auth` 기능으로 빌드할 때만 다음이 포함된다: Bitwarden CLI 호출(`auth/vault.rs`), 로컬 승인 전송(Windows named pipe / Linux Unix 소켓, 상대 프로세스·이미지·자식 관계 확인), `zzabba-askpass` helper, 승인 1회 소비(`auth/mod.rs`). 터미널 탭은 `terminal_open` → `terminal::saved::launch_interactive` → `ssh::terminal::saved_interactive`로 저장 암호 대상을 열고, `Pending::approve`로 묶은 `Approval`이 PTY 자식과 `Exchange`를 함께 만든다. 승인 창을 읽는 시간은 별도 기한(120초)으로 재고, 승인 뒤 askpass 교환 기한은 spawn 시점부터 다시 잰다. `Exchange`는 탭 닫기·문서 이동·창 닫기에서 취소되고 `bw` 자식도 그 수명에 회수된다. 교환이 실패하면 금고 오류를 탭 상태줄 알림으로 전한다. 승인 창은 기존 네이티브 확인 패턴으로 서버·계정·지문·금고 항목 ID를 표시한다. 연결 시험·SFTP·원격 편집기·터널과 `DirectLaunch::prepare`는 지금처럼 저장 암호를 명시적 오류로 거부한다. 기본 빌드에는 이 경로가 없고, 마법사는 `connection_capabilities`의 `storedAuth` 값으로 옵션 노출과 저장을 함께 막는다. F01 완료 판정은 실제 OpenSSH 종단 증거가 남을 때 한다.

금고 준비와 되돌림:
- `bw`는 고정 절대 경로(`%ProgramFiles%\Bitwarden CLI\bw.exe`, `/usr/bin/bw`, `/usr/local/bin/bw`)에서만 찾고 `Bitwarden::new`의 소유자·ACL 검사를 통과해야 한다. 세션 키는 앱 프로세스의 `BW_SESSION` 환경에서만 읽고 WebView·argv·자식 환경·로그로 내보내지 않는다.
- 세션 키가 없거나 `bw status`가 `locked`·`unauthenticated`이거나 CLI가 없으면 같은 대상·계정·지문의 **직접 입력**으로 열고 터미널 상태줄에 전환 이유를 표시한다. 자동 재시도나 다른 자격 증명 폴백은 없다.
- 마법사는 호스트 키 확인을 마친 저장 암호 대상을 연결 시험 없이 참조로 저장한다. 연결 시험 경로 자체는 저장 암호를 명시적 오류로 거부한다.

경계와 전제:
- Windows는 named pipe DACL을 소유자 전용으로 만들고 `reject_remote_clients`를 켠 뒤, 연결 피어가 승인한 `ssh` 자식이고 이미지가 신뢰된 경로인지 확인한다. askpass 파이프 검사는 helper의 stdout 핸들을 복제해 `GetFileType == FILE_TYPE_PIPE`와 `GetNamedPipeServerProcessId`/`GetNamedPipeClientProcessId`가 모두 파이프를 만든 `ssh` PID를 가리키는지 본다. 2026-11-23 Windows 11 측정에서 `CreatePipe`의 쓰기 끝(client) 핸들에서도 두 API가 성공하고 생성 프로세스 PID를 돌려주는 것을 확인했다.
- Linux는 `SO_PEERPIDFD`(커널 6.5 이상)로 상대 프로세스를 고정한다. Ubuntu 24.04(6.8)는 통과하고, Debian 12(6.1) 같은 환경에서는 채널을 열지 못한다. 미지원을 별도 상태로 안내하는 처리는 아직 없다.
- 배포 전제: helper(`zzabba-askpass`)는 앱 옆에, `bw`는 관리자·소유자가 검증한 경로에 있어야 한다. 현재 per-user 설치(`%LOCALAPPDATA%`, `bundle.active=false`, `externalBin` 없음)로는 두 검사를 통과하지 못하며, 패키징 정책을 정해야 한다.
- 같은 사용자 권한의 악성 코드가 승인 창을 사칭하거나 IPC에 주입하는 경우는 이 경계 밖이다.
- OpenSSH가 askpass를 지원하지 않는 환경을 탐지해 `저장 인증 불가`로 표시하는 처리는 아직 없다.

저장 대상은 `terminal_open` → `terminal::saved::launch` → `ssh::terminal::DirectLaunch`를 거친다. Rust는 각 PTY ID에 독립 계획을 만들고 직접 실행한 자식의 종료를 확인한 뒤 임시 파일을 해제한다. 닫기에서 종료 요청이나 상태 확인이 실패하면 같은 탭에 `종료 확인 불가`가 표시되며 다시 확인할 수 있다. Windows의 확인 범위는 소유한 프로세스 핸들 하나다.

SSH 연결 시험은 별도 터미널에서 비밀번호나 키 암호를 입력받는다. Rust는 현재 문서와 초안의 시험 ID에만 입력을 연결하고, 고정 원격 명령의 시도별 출력과 정상 종료가 모두 확인된 뒤 저장을 승인한다.
시험 자식의 종료가 미확인이면 취소 또는 창 닫기를 다시 요청할 수 있다. 재확인은 같은 OS 소유 자식만 대상으로 하며, 미확인 상태에서는 창 닫기 요청을 보류하고 자식과 접속 계획을 유지한다.

`exec`를 거부하거나 POSIX `printf`가 없는 계정은 직접 셸을 열 수 있어도 현재 연결 시험을 통과할 수 없다. 이 제약을 없애려면 같은 OpenSSH 자식이 호스트 키와 사용자 인증을 통과한 뒤 받은 `shell` 채널 수락을 원격 출력과 분리된 로컬 경로로 전달해야 한다. Windows OpenSSH 9.5p2의 `-E` 파일은 자식이 실행 중일 때 읽기 공유를 거부하고, 일반 ConPTY 출력은 서버 출력과 섞인다. 두 OS에서 사용할 수 있는 독립 이벤트가 확보될 때까지 현재 시험 계약을 유지한다.

루프백 검증은 테스트 전용 Paramiko가 설치된 Python을 `ZZABBA_F01_FIXTURE_PYTHON`으로 지정하고 `cargo test --locked --manifest-path app/src-tauri/Cargo.toml --lib f01_interactive_loopback_saves_and_reopens -- --ignored`를 실행한다. 시험이 만든 계정·키·임시 포트로 직접 입력, 암호화 키, 취소, 저장·재로드·재접속 및 활성 셸 닫기를 확인한다.
