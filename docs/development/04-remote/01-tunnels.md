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
3. **Green — SSM.** [Session Manager 문서](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-sessions-start.html)의 `aws ssm start-session`에 선택한 프로필·리전·관리 노드 및 `AWS-StartPortForwardingSession`을 넘긴다. `localPortNumber`는 로컬 수신, `portNumber`는 노드의 서비스 포트다. 로컬 포워딩만 제공한다. CLI·플러그인 누락, 인증 만료, 문서 실행 거부, 목적지 거부를 나눠 표시한다. 서비스 로그인을 SSM 승인으로 대체하지 않는다.
4. **Green — 수명.** Rust가 발급한 ID에 생성 프로세스 핸들·SSM 세션 ID·소유 탭 집합을 묶는다. 각 요청에서 창과 탭의 소유권을 확인한다. 자식의 실제 준비 결과와 종료를 관찰한다. 사전 포트 검사만 믿지 말고 시작 중 바인드 실패도 처리한다. 수신 준비와 목적지 서비스의 실제 응답을 구분한다. 마지막 해제·창 종료 때 소유 자식과 플러그인을 정리하고 해당 SSM ID의 종료를 확인한다. 포트/PID 검색으로 다른 프로세스를 종료하지 않는다.
5. **자동 검증.** `cargo test --manifest-path src-tauri/Cargo.toml tunnels`와 `npx vitest run src/features/tunnels`에서 실제 실행한 사례 수를 기록한다. 로컬 수신기를 먼저 열어 바인드 충돌 후에도 기존 응답이 살아 있는지 검사한다.
6. **실기기 시연(T5 터널).** 두 OS에서 `-L`과 SSM은 로컬에서, `-R`은 원격 수신 측에서, `-D`는 SOCKS를 지정한 시험 클라이언트에서 승인된 목적지의 고유 문구를 읽는다. 같은 터널의 탭 닫기, 다른 터널의 강제 종료, 포트 충돌을 인수 조건대로 반복한다. OS·도구 버전·방향·익명화한 끝점·탭별 전후 응답·마지막 포트 해제와 SSM 종료 결과를 남긴다. 자원 부재는 `미실행(사유)`이다. 이 기록은 RDP·VNC 화면 시험과 별도다.
