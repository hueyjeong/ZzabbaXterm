# R01 탭이 소유하는 SSH·SSM 터널

선행: F02, F03

사용자 결과: 사용자는 연결 탭에서 전달 방향, 양 끝점, 이 터널을 쓰는 탭을 확인한다. 마지막 소유 탭을 닫으면 앱이 연 터널만 종료된다.

준비물: Windows 11과 Ubuntu 24.04 GNOME Wayland의 OpenSSH, 호스트 키를 확인한 비운영 SSH 계정, AWS CLI와 Session Manager 플러그인, 포트 전달이 승인된 비운영 프로필·리전·관리 노드. 전달 양 끝에서 시험 문구를 돌려줄 서비스와 닫아도 되는 탭 두 개를 준비한다. 아래 명령은 구현 시점에 `app/`에서 사용한다.

## 인수 조건

- Given 같은 터널을 쓰는 A·B와 다른 터널 C / When A를 닫고 B에서 시험 요청을 보낸 뒤 B도 닫는다 / Then 첫 요청은 응답하고 두 번째 닫기 후 해당 수신 포트와 앱 소유 세션이 정리된다. C는 계속 응답한다.
- Given 다른 프로그램이 시험 포트를 점유 / When 같은 포트로 새 전달을 요청 / Then 충돌한 수신 측·포트를 표시하며 기존 프로그램은 원래 응답을 반환한다. 그 포트에 접속할 수 있다는 사실만으로 새 터널을 준비 완료로 표시하면 실패다.
- Given 열린 터널 두 개 / When 하나의 앱 소유 전달 프로세스를 강제로 종료 / Then 해당 소유 탭만 끊김으로 바뀐다. 이미 닫은 탭의 늦은 종료 이벤트가 새 탭을 끊으면 실패다.

## 구현 순서

1. **Red.** `app/src-tauri/src/tunnels/`에서 네 전달 방식의 인자 구성, 바인드 실패, 소유 탭 마지막 해제, 생성 도중 닫기·중복 닫기를 표 기반 Rust 테스트로 작성한다. `app/src/features/tunnels/`에는 준비 중·준비됨·실패를 구분하는 Vitest를 둔다.
2. **Green — 방향.** [OpenSSH](https://man.openbsd.org/ssh) 자식은 셸 문자열이 아닌 인자 배열로 실행한다. `-N`과 `-o ExitOnForwardFailure=yes`를 사용한다. `-L`은 로컬 루프백 수신 → SSH 서버에서 시험 서비스로 접속, `-R`은 SSH 서버 루프백 수신 → 클라이언트 쪽 시험 서비스로 접속, `-D`는 로컬 루프백 SOCKS 수신 → SSH 서버에서 클라이언트가 지정한 목적지로 접속이다. 동적 전달 화면에는 고정 목적지 대신 SOCKS를 표시한다. 기존 호스트 키·인증 경계를 사용하고 원격 수신도 승인된 인터페이스인지 확인한다.
3. **Green — SSM.** [Session Manager 문서](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-sessions-start.html)의 `aws ssm start-session`에 선택한 프로필·리전·관리 노드 및 `AWS-StartPortForwardingSession`을 넘긴다. `localPortNumber`는 로컬 수신, `portNumber`는 노드의 서비스 포트다. 노드에서 다른 호스트로 전달할 때는 `AWS-StartPortForwardingSessionToRemoteHost`에 `host`를 더한다. `host`는 DNS 이름 또는 IP 리터럴만 받고 공백·쉼표·`=`·제어 문자를 거부한다. 두 문서 모두 같은 프로필·리전·노드 승인 범위를 대조한다. 로컬 포워딩만 제공한다. CLI·플러그인 누락, 인증 만료, 문서 실행 거부, 목적지 거부를 나눠 표시한다. 서비스 로그인을 SSM 승인으로 대체하지 않는다.
4. **Green — 수명.** Rust가 발급한 ID에 생성 프로세스 핸들·SSM 세션 ID·소유 탭 집합을 묶는다. 각 요청에서 창과 탭의 소유권을 확인한다. 자식의 실제 준비 결과와 종료를 관찰한다. 사전 포트 검사만 믿지 말고 시작 중 바인드 실패도 처리한다. 수신 준비와 목적지 서비스의 실제 응답을 구분한다. 마지막 해제·창 종료 때 소유 자식과 플러그인을 정리하고 해당 SSM ID의 종료를 확인한다. 포트/PID 검색으로 다른 프로세스를 종료하지 않는다.
5. **자동 검증.** `cargo test --manifest-path src-tauri/Cargo.toml tunnels`와 `npx vitest run src/features/tunnels`에서 실제 실행한 사례 수를 기록한다. 로컬 수신기를 먼저 열어 바인드 충돌 후에도 기존 응답이 살아 있는지 검사한다.
6. **실기기 시연(T5 터널).** 두 OS에서 `-L`과 SSM은 로컬에서, `-R`은 원격 수신 측에서, `-D`는 SOCKS를 지정한 시험 클라이언트에서 승인된 목적지의 고유 문구를 읽는다. 같은 터널의 탭 닫기, 다른 터널의 강제 종료, 포트 충돌을 인수 조건대로 반복한다. OS·도구 버전·방향·익명화한 끝점·탭별 전후 응답·마지막 포트 해제와 SSM 종료 결과를 남긴다. 자원 부재는 `미실행(사유)`이다. 이 기록은 RDP·VNC 화면 시험과 별도다.

## 현재 연결 범위

- 왼쪽 트리의 저장된 직접 SSH 연결에서 **터널 열기**를 선택한다. Rust는 승인한 연결과 포트를 다시 확인하고, 마지막 소유 탭의 닫기에서만 해당 자식을 종료한다. 종료를 확인하지 못하면 같은 탭에서 다시 확인한다.
- 직접 SSH의 `-L`과 `-D`를 지원하며, `-R`은 루프백 수신을 검증할 수 있는 Linux 경로에서 제공한다. Windows `-R`은 시작 전에 차단한다.
- `-L`에서 RDP/VNC로 승인한 서비스가 SSH 서버의 `127.0.0.1` 또는 `::1`에 있다면 준비된 행에서 해당 원격 화면을 열 수 있다. 원본 터널 탭을 닫아도 화면 탭이 연결을 소유하면 전달은 이어지며 마지막 소유 탭에서 종료한다.
- 저장된 RDP·VNC 프로필의 SSH 경로는 같은 `-L` 전달을 열고 준비된 전달로 원격 화면 탭을 연다(`openDesktopProfile`). 목적지는 SSH 서버의 `127.0.0.1:포트`다.
- 격리 Ubuntu에서 OpenSSH `-L`·`-R`·`-D`의 실제 응답, 마지막 소유자 종료, 포트 충돌과 늦은 닫기를 확인했다. Windows와 네이티브 Ubuntu의 T5 전체는 미실행이다.

## SSM 포트 전달

- **시작 지점.** 왼쪽 트리의 저장된 SSM 대상에서 **터널 열기**를 고른 뒤 수신 포트, 원격 호스트(비우면 노드 자신), 목적지 포트, 서비스(TCP·RDP·VNC)를 입력한다. 저장된 RDP·VNC 프로필의 SSM 경로는 프로필 자신을 경로 대상으로 같은 실행 경로를 연다. RDP는 프로필의 TLS 인증 DNS 이름을 노드에서 접속할 원격 호스트로 쓰고(`AWS-StartPortForwardingSessionToRemoteHost`), VNC는 노드 자신의 서비스만 연다(`AWS-StartPortForwardingSession`).
- **승인 범위.** `tunnel_start`의 `{ direction: "ssm", listen, port, host?, protocol? }`는 Rust가 저장 파일에서 다시 읽은 프로필·리전·노드와 대조한다(`SsmPlan`). 원격 화면 프로필에서는 포트·호스트·서비스가 저장 값과 같아야 한다. VNC는 원격 호스트를 받지 않는다. `host`는 DNS 이름 또는 IP 리터럴만 받고 공백·쉼표·`=`·제어 문자를 거부한다. 네이티브 승인 창에 프로필·리전·노드·문서·매개변수·경로를 표시하고, 승인 후 저장 기록이 바뀌었으면 시작하지 않는다.
- **자격 경계와 실행.** 승인 뒤 기록은 `starting`으로 등록되고 작업자가 C02와 같은 경로로 준비한다: `aws configure export-credentials`로 SSO 자격을 내보내고(만료 시 `SSM: SSO 재로그인 필요`), `describe-instance-information`으로 노드의 `Online`을 확인하고, 공식 플러그인 경로와 버전을 검사한다. 실행은 `aws ssm start-session --region … --output json --target <노드> --reason <요청별 난수> --document-name <문서> --parameters <매개변수>`이며 `--profile`을 넣지 않는다. 자격은 격리한 CLI 홈과 환경 변수로만 전달하고, 엔드포인트·프로필 재정의 환경 변수는 제거한다. 탭이 닫히면 준비를 취소하고 자식을 만들지 않는다.
- **프로세스 구조.** PTY 대신 파이프(stdin 없음, stdout·stderr 판독)로 실행한다. Windows는 aws.exe를 생성 직후 `KILL_ON_JOB_CLOSE` 작업 개체에 넣어 그 자식인 session-manager-plugin.exe까지 묶는다. Linux·macOS는 aws CLI를 새 프로세스 그룹의 리더로 실행한다.
- **준비 판정.** 플러그인 stdout에서 `Starting session with SessionId: <id>` → `Port <n> opened for sessionId <id>.` → `Waiting for connections...`의 순서를 확인한다. 준비 전에 다른 문장이 오거나 세션 ID가 다르면 실패다. 그 뒤 `DescribeSessions`에서 Reason·Owner·Target이 일치하는 단일 `Connected` 세션의 ID가 출력의 ID와 같고, 출력의 포트가 승인한 `127.0.0.1` 수신 포트와 같을 때만 `수신 준비됨`으로 바꾼다. 60초 안에 확인하지 못하면 실패다. 수신 준비는 목적지 서비스의 응답과 별개이며, 목적지 거부는 그 연결을 연 화면 탭이나 클라이언트에서 드러난다.
- **실패 구분.** 사전 bind 검사와 플러그인의 bind 실패 출력은 `로컬 수신 주소 사용 중`, StartSession 거부는 `포트 전달 문서 실행 권한 거부`(문서 ARN이 들어 있는 접근 거부)·`권한 거부`·`포트 전달 문서 실행 거부`(`InvalidDocument` 등)·`자격 증명 만료`·`노드 연결 불가`, 출력 형식 불일치는 `SSM 플러그인 출력 형식을 확인할 수 없습니다`, 세션 소유권 불일치는 `SSM 세션 소유권 확인 불가`, 준비 전 종료는 종료 코드와 함께 표시한다. 준비 뒤 종료는 `끊김`이다.
- **수명과 정리.** Rust 터널 ID에 aws 자식 핸들, 프로세스 트리, 플러그인이 출력한 SSM 세션 ID, 소유 탭 집합을 묶는다. 마지막 소유 탭 해제·창 종료·문서 전환 때 작업 개체 종료(Windows) 또는 그룹 신호(Unix)로 이 트리만 정리하고, 리더 회수와 트리가 빈 것을 확인한다. Unix는 리더를 회수하기 전에만 그룹 신호를 보낸다. 포트나 PID 검색으로 다른 프로세스를 종료하지 않는다. 세션이 시작된 경우 같은 요청의 세션만 `terminate-session`과 History 조회로 별도 작업자에서 닫고, 확인하지 못하면 대상 노드를 로그에 남긴다.
- **원격 화면.** 준비된 SSM 행의 `desktop`은 RDP 원격 호스트면 `{ host: DNS 이름, port }`, 노드 자신이면 `{ host: "127.0.0.1", port }`다. 화면 탭은 같은 `preflight_desktop`·`open_desktop`으로 승인 당시의 저장 SSM 기록과 목적지를 다시 대조하고, 받아들인 쪽 소켓을 이 트리의 프로세스가 소유하는지 확인한 연결만 인계받는다(Windows 작업 개체의 프로세스 목록, Linux `/proc`의 자손).
- **검증 상태.** 가짜 CLI·플러그인 트리로 준비 판정, 수신 포트 불일치, 소유권 불일치, 문서 거부, bind 실패, 출력 형식 오류, 트리 정리와 바이트 왕복을 자동 시험한다. 이 PC의 실제 AWS CLI로 SSO 만료 상태가 `SSM: SSO 재로그인 필요`로 구분되는 것을 확인했다. 실제 AWS 포트 전달과 바이너리 보존(`app/src-tauri/tests/ssm_forward_bytes.py`)은 미실행이다.
