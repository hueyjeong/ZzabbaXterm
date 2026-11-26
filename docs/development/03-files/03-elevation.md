# F03 sudo 직접 입력과 한 번 승인한 askpass 구분하기

선행: F01

**사용자 결과:** 평소처럼 터미널에서 `sudo` 암호를 직접 입력할 수 있다. 저장된 인증을 쓰려면 로컬 창에서 실제 대상·계정·명령을 보고 그 요청 한 건을 승인한다. 연결할 때 승인한 SSO·SSH 인증은 `sudo` 승인으로 이어지지 않는다.

**준비물:** 네이티브 Ubuntu 24.04 GNOME Wayland의 격리된 로컬 시험 계정, Windows 11·Ubuntu에서 F01로 접속할 비운영 Linux 시험 서버, 암호를 요구하며 `/usr/bin/id -u` 실행이 이미 허가된 sudo 시험 계정, F01의 시험용 Bitwarden 항목. 원격 헬퍼가 없는 대상과 관리자가 별도 승인해 설치·신뢰한 대상이 각각 필요하다. Windows는 Linux 원격 `sudo`의 클라이언트이며 Windows 로컬 승격이나 WSL은 이 시험의 대체물이 아니다. 앱은 서버에 헬퍼를 설치하거나 `sudoers`·계정을 수정하지 않는다.

| Given (조건) | When (행동) | Then (관찰) |
|---|---|---|
| 직접 입력 모드이거나 원격 헬퍼의 신원·채널을 확인할 수 없다 | 사용자가 시험 셸에서 `sudo -k /usr/bin/id -u`를 치고 직접 암호를 입력한다 | sudo가 터미널에서 직접 인증하고 성공 시 `0`이 나온다. 앱의 금고 호출·암호 자동 전송은 0회다. |
| 사용자가 신뢰한 명시적 승격 실행기를 열었다 | 실제 대상·호출 계정·실행 계정·정확한 명령·금고 항목을 확인하고 1회 승인한다 | 그 호출의 askpass 요청에만 암호를 한 번 답한다. 같은 명령을 다시 요청해도 새 로컬 승인이 필요하다. |
| 단순 셸 출력에 비밀번호 프롬프트와 똑같은 문구가 있다 | 출력만 받거나, 실제 승인 대기 중 취소·단절·대상 변경이 발생한다 | 출력만으로는 승인 창도 금고 조회도 없다. 대기 중 요청은 취소/만료되고 다음 탭이나 재연결에 암호가 전달되지 않는다. |

## 개발 순서

1. **Red — 암호를 보내지 않는 경우부터 고정한다.** `app/src/features/elevation/elevation.test.ts`에 두 대상 간 탭 전환, 사용자 취소, AI 제안·셸 출력으로는 사용자 승인이 성립하지 않는 사례를 적는다. `app/src-tauri/src/elevation/`의 Rust 시험은 승인 대상과 다른 세션 ID·계정·명령 인자, 만료된 요청, 중복 응답, 신뢰되지 않은 헬퍼에 대해 금고 조회/응답 쓰기 횟수 0을 검사한다. 일반 PTY 출력에 `[sudo] password for demo:`를 넣어도 승격 요청은 생기지 않아야 한다.
2. **Green — 직접 입력을 기본으로 둔다.** 메인 PTY의 `sudo`는 OS 프로그램이다. 사용자 키 입력은 기존 PTY에만 전달하고 AI 제안이나 채팅으로 복제하지 않는다. 앱은 셸 화면을 스크래핑하거나 `sudo` 문자열·에코 해제 여부로 암호 입력 시점을 추측하지 않는다. 헬퍼 없음·검증 실패는 명시적 경로를 중단하고 직접 입력을 선택하게 한다. 취소된 명령을 앱이 수동 경로로 다시 실행하지 않는다.
3. **Green — 로컬 승격 요청을 실행 전에 고정한다.** Rust가 사용자의 명시적 로컬 요청에서 백엔드 세션 ID·대상·호출 계정·실행 계정·실행 파일의 절대 경로와 인자 배열을 고정한다. `app/src/features/elevation/`은 이 정보를 인자 경계까지 구별해 표시하고 승인/취소만 돌려준다. 셸 문자열을 재해석하지 않는다. 로컬 승인 후에만 해당 자식에 한해 `SUDO_ASKPASS`를 신뢰한 실행기의 절대 경로로 설정하고 `sudo -A -- <실행파일> <인자…>`를 시작한다(기본 실행 계정 root 기준). sudo가 캐시된 인증으로 끝났으면 금고 조회는 0회다. 캐시가 있어도 앱의 다음 명시적 요청은 다시 승인받는다.
4. **Green — 헬퍼가 요청한 한 번에만 답한다.** 로컬은 사용자 전용 Unix 소켓과 OS 상대 프로세스 자격 확인을 사용한다. 대상 탭에서 시작한 자식 계통의 askpass, 미사용 요청 ID와 짧은 만료 시간, 고정한 실행 정보가 모두 맞을 때만 승인 기록을 한 번 소비한다. 원격에서는 관리자와 사전 확인한 고정 헬퍼(소유자·변경 권한·배포본 확인)를 F01이 검증한 **같은 서버·계정의 별도 SSH 채널**에서 명시적으로 실행한다. 헬퍼가 승인 정보를 같은 호출의 `sudo -A`에 결합하고 그 askpass 응답만 전용 채널로 교환한다. 메인 PTY·공용 수신 포트·셸 프롬프트에는 암호를 쓰지 않는다. 승인 후에도 실제 askpass 요청이 올 때만 F01의 로컬 인증 처리기로 선택한 항목을 조회한다. 원격에는 금고 세션 키가 아닌 해당 sudo의 1회 응답만 보내고 즉시 버퍼를 비운다. 취소·타임아웃·채널 종료에는 승인과 미사용 응답을 폐기하고 해당 헬퍼만 회수한다. 채널이 끊긴 뒤 최종 명령 상태는 `결과 미확인`이다.
5. **자동 검증 뒤 직접 입력과 취소를 시연한다.** 명령은 후속 구현 시 사용한다. `app/`에서 `npx vitest run src/features/elevation/elevation.test.ts`, `cargo test --manifest-path src-tauri/Cargo.toml elevation`을 실행하고 실제 실행 건수를 남긴다. `npm run tauri dev`로 Ubuntu 로컬, Windows·Ubuntu의 Linux 원격 탭을 연다. 각 시험 셸에서 `printf '[sudo] password for demo: '`만 실행해 조회 0회를 확인한다. 표 첫 행의 실제 `sudo`로 직접 입력을 시험한 뒤, 격리된 시험 셸에서 `sudo -k`로 그 셸의 인증 캐시를 무효화한다. 명시적 경로에서도 헬퍼가 사용할 실행 문맥에서 캐시가 비어 있는지 확인한다. NOPASSWD나 기존 캐시로 얻은 `0`만으로 암호 전달 경로를 통과 처리하지 않는다.
6. **승인·단절·재사용을 판정한다.** 로컬과 신뢰한 원격 헬퍼에서 `/usr/bin/id -u`를 각각 한 번 승인한다. 승인 1건, askpass 응답 1건, 종료 코드 0과 출력 `0`을 확인한다. 다음 요청은 취소하여 금고 조회·응답 쓰기 0회를 확인한다. 승인 대기 중 해당 원격 탭만 닫고 다른 탭을 열어 옛 요청이 소멸했는지 본다. 원격 헬퍼가 없는 대상은 수동 입력으로만 성공해야 한다. 재현에 필요한 권한이 없으면 `미실행(사유)`으로 남긴다.

**현재 상태:** 로컬·원격 승격 경로 구현·자동 시험 완료. 원격은 Windows·Linux 클라이언트에서 Linux 서버의 관리자 설치 helper를 사용한다.

구현한 로컬 경로:

- `app/src-tauri/src/elevation/`가 명시적 요청에서 대상(`내 PC`), 호출 계정(OS 사용자), 실행 계정(`root`), 정규화한 절대 실행 파일, 인자 배열, 32자 요청 ID를 고정한다. 승인은 30초 만료, 문서 세대·요청 ID·대상·계정·실행 파일·인자 전체 일치, 1회 소비다. 승인 창은 인자를 `1.`, `2.`처럼 경계별로 표시한다.
- `app/src-tauri/src/auth/sudo.rs`는 승인 뒤 그 자식에만 `SUDO_ASKPASS`·`ZXT_ASKPASS_MODE=sudo`를 설정하고 `sudo -A -- <실행파일> <인자…>`를 시작한다. 교환은 `sudo` 자식의 직계 askpass(자식 계보·helper 실행 파일·pidfd 생존)만 받고, 실제 askpass 요청이 온 뒤에만 금고를 조회해 1회 응답하고 ACK 뒤 수신을 닫는다. 캐시된 인증이면 자식 종료로 끝나 금고 조회는 0회다.
- `app/src-tauri/src/auth/transport.rs`의 helper는 `SUDO_ASKPASS` 모드를 추가로 허용하되 `SUDO_ASKPASS`가 현재 helper와 같고 `SSH_ASKPASS`·`SSH_ASKPASS_REQUIRE`가 없으며 `BW_SESSION`이 없을 때만 동작한다. SSH 저장 암호 자식은 상속 환경의 `SUDO_ASKPASS`·`ZXT_ASKPASS_MODE`를 제거한다.
- `app/src-tauri/src/terminal/{ipc.rs,process.rs}`에 `elevation_open` 명령, 네이티브 "sudo 승격 승인" 창, `Launch::Elevated`, 탭 종료 시 교환 취소, 금고 오류 알림을 연결했다. `app/src/features/elevation/`은 실행 파일·인자(줄 단위)·금고 항목을 입력받고 인자 경계를 나눈 승인 카드를 표시한다. 터미널 출력이나 AI 제안은 이 명령을 부르지 않는다.
- 승인 30초는 모달을 닫은 뒤 `Pending`을 만들 때 시작한다. 헬퍼·금고 상태 확인과 사용자가 창을 읽는 시간은 예산에 넣지 않는다.

구현한 원격 경로:

- 서버 helper는 `app/src-tauri/src/bin/zxt-elevate.rs`이며 관리자가 `/usr/local/libexec/zxt-elevate`에 root:root·0755·그룹/월드 쓰기 없이 설치한다. 앱은 이 파일을 설치하지 않는다. `--verify`는 자기 소유자·권한·경로와 실행 계정을 출력하고, `serve`는 run 프레임을 받아 `sudo -A -- <실행파일> <인자…>`를 띄우며, `bridge`는 전용 SSH 채널에서 askpass 요청을 중계한다. askpass 모드는 자기 자신이 `SUDO_ASKPASS`일 때만 FIFO를 통해 1회 응답한다.
- 앱은 F01과 같은 `ConnectionPlan`(같은 서버·계정·고정 호스트 키)에서 **확인·run·bridge 세 개의 별도 SSH `-T` 채널**을 연다. 계획은 승격 `State`가 보관해 run/bridge가 여는 임시 `ssh_config`·`known_hosts`가 세션 수명보다 먼저 삭제되지 않는다. 확인 채널은 helper 설치·계정을 먼저 검증하고 실패하면 승인 창 없이 "직접 입력"을 안내한다. 승인 카드는 서버(`계정@호스트:포트`)·호출 계정·실행 계정·실행 파일·인자를 표시하고, 승인 뒤 저장 프로필이 바뀌지 않았는지 다시 대조한다.
- run 채널은 메인 PTY에서 `serve`를 실행하고 `ZXT-ELEVATE 1 <ID> <난수> <b64 실행 파일> <인자 수> <b64 인자…>` 프레임을 쓴다. 프레임에는 암호가 없다. bridge 채널은 `REQUEST 2 <ID> <난수>`를 받은 뒤에만 금고를 조회하고 `PASSWORD 2 <ID> <난수> <b64 암호>`를 1회 쓴다. 메인 PTY·공용 포트·셸 프롬프트에는 암호를 쓰지 않는다.
- helper는 승인 ID·난수·프레임 순번(1/2)을 검증하고, 잘린 프레임·순번 역행·다른 승인 ID·다른 난수를 거부한다. askpass는 `pw` FIFO 응답을 15초까지만 기다려 bridge가 응답 없이 죽어도 sudo가 매달리지 않는다.
- bridge가 암호를 건네기 전에 끊기면 앱은 run 자식을 중단하고 최종 상태를 `결과 미확인`으로 표시한다. 암호를 이미 건넸거나 run이 정상 종료했으면 실제 종료 코드를 유지하고 bridge 손실은 알림으로만 남긴다. 정상 종료하지 못한 FIFO 디렉터리는 다음 `serve`가 6시간 뒤 정리한다.
- 원격은 직접 SSH의 Agent·키 파일(배치 로그인)만 지원한다. 수동 암호·저장 암호 SSH·SSH-over-SSM은 "터미널에서 직접 입력하세요" 안내로 끝난다. helper가 없거나 `--verify`가 실패하면 금고 조회 0회다.
- Windows에서는 로컬 sudo 경로가 없으므로 "내 PC" 승격 메뉴를 숨기고, 원격 승격 메뉴만 `storedAuth` capability에 따라 표시한다.

기능·배포 조건:

- 로컬·원격 승격 명령과 UI는 `stored-auth` feature와 Windows/Linux에서만 컴파일된다. `connection_capabilities`는 `storedAuth` camelCase 키로 응답하며(백엔드 직렬화 시험 고정), 기본 빌드·`--no-default-features`·macOS에서는 false여서 승격 메뉴가 보이지 않고 남은 호출도 한국어 거부 메시지를 받는다. Windows에서는 로컬 메뉴만 플랫폼 조건으로 숨긴다.
- 로컬은 앱과 같은 디렉터리의 `zxt-askpass`가 root 소유·비쓰기여야 한다. 배포 번들에 `zxt-askpass`를 포함해야 한다. 원격 서버에는 `zxt-elevate`를 관리자가 별도로 설치해야 하며, 앱은 sudoers·helper를 수정하지 않는다.

실측(`zzabbaxterm-sudo-remote:latest`, Ubuntu 24.04.5 기반. `zzabbaxterm-demo-remote:latest`에 `apt-get install sudo`로 실제 sudo를 넣고 `demo`를 sudo 그룹에 추가; 컨테이너 `zzabba-sudo-remote`, `127.0.0.1:2300`, 계정 `demo` uid 1001, 암호 요구 sudo; 옛 이름 ZzabbaXterm 시절 측정):

- 사용자 uid 1001로 `SUDO_ASKPASS=… sudo -A -- /usr/bin/id -u`를 실행하면 `0`이 출력된다. askpass 프로세스는 `uid=1001 euid=1001`, `ppid=<sudo>`, `SUDO_ASKPASS`·`SSH_ASKPASS`·`ZXT_*` 환경을 상속한다. 따라서 Linux 전송의 `peer.uid == geteuid()` 검사는 `sudo -A` helper에도 성립하며 자식 계보 정책으로 다시 설계할 필요가 없다.
- setuid 부모의 `/proc/<sudo>/exe`와 fd는 helper 사용자 권한으로 읽을 수 없으므로, sudo 경로는 루트 실행 파일·출력 파이프 대신 자식 계보와 helper 신원으로 검증한다. 이 때문에 helper는 `sudo`의 직계 자식이어야 한다.
- helper는 `/usr/local/libexec/zxt-elevate`에 root:root 0755로 설치했고, `--verify`가 `zxt-elevate 1`, `user=demo`, `owner=0`, `mode=755`, `path=/usr/local/libexec/zxt-elevate`를 출력했다.
- Windows 클라이언트에서 앱과 같은 `ConnectionPlan`·`verify`·`Launch`·`Bridge` 코드로 원격 `sudo -A -- /usr/bin/id -u`를 실행했다. sudo 캐시를 지운 뒤 승인 1건, 금고 `status`+`get` 각 1회, 암호 프레임 1회, 종료 코드 0, stdout `0`, stderr 없음, 출력에 암호 없음을 확인했다. `state.sent`도 1회였다.
- 자동 시험(`elevation_remote_real_container_approves_once_and_runs_sudo`, ignored)은 `.omc/demo-remote/sudo/remote.json`의 호스트·포트·키·호스트 키·암호를 읽는다. 컨테이너·키·암호는 저장소에 커밋하지 않는다.

자동 시험 결과:

- `cargo test --manifest-path app/src-tauri/Cargo.toml --features stored-auth --lib elevation` → 22 passed, 0 failed, 2 ignored(원격 컨테이너 실측·askpass 기한). 승인 대상 불일치·만료·다른 세션/계정/실행 파일/인자, 재응답 거부, 취소, 캐시 인증 0회 조회, 신뢰되지 않은 helper, 계보 밖 helper, 원격 확인·프레임 절단·순번 역행·다른 승인 ID·채널 단절(`결과 미확인`), 금고 오류 시 run 중단, 정상 종료 코드 유지를 검사한다.
- `cargo test … --lib elevation_remote_real_container -- --ignored` → 1 passed. 계획 원본을 버린 뒤에도 임시 설정이 남는지 단언하고, 승인 1회·응답 1회·`id -u` 0을 고정한다.
- `cargo test … --lib elevation_remote_askpass_times_out -- --ignored` → 1 passed. bridge가 응답하지 않으면 15초 뒤 askpass가 실패해 run이 40초 안에 종료한다.
- `cargo test … --lib auth` → 26 passed(상속 `SUDO_ASKPASS`·`ZXT_ASKPASS_MODE`가 SSH 모드를 깨지 않는 회귀 포함), `--lib ssh` → 57 passed, `--lib terminal` → 44 passed, `capabilities_serialize` → 1 passed. 기본 빌드 `cargo check --lib` 통과.
- `npx vitest run` → 272 passed, 28 files. `tsc --noEmit` 통과. 승인 카드가 인자를 나눠 표시하고, `[sudo] password for demo:` 출력만으로는 승격 요청이 생기지 않으며, 저장 인증 capability가 없으면 승격 메뉴가 보이지 않음을 검사한다.
- helper 단위 시험은 Linux에서 `rustc --edition=2024 --test`로 2 passed(run 프레임·askpass·password 프레임 검증).

남은 위험:

1. **Tauri 창·PTY 경로의 GUI 자동 시험은 미실행.** 원격 실측은 앱의 계획·확인·run·bridge 코드를 Windows에서 실행했지만 run 채널은 파이프로 돌렸다. 제품 탭은 같은 run 명령을 로컬 PTY에서 실행하고 프레임을 PTY에 쓴다. 프레임은 암호가 아니지만 로컬 터미널 에코에 보일 수 있다. 계획 수명은 `State` 보관과 실측 단언으로 고정했다.
2. 원격 실측은 실제 Bitwarden 항목 대신 컨테이너 암호를 돌려주는 시험 금고를 썼다. 제품의 금고 조회 경로(`Bitwarden::password`)는 같다.
3. helper는 관리자가 root:root·비쓰기로 설치해야 한다. 앱은 설치하지 않으며, 원격은 직접 SSH의 Agent·키 파일만 지원한다. 수동 암호·저장 암호 SSH·SSM·helper 없음은 "직접 입력" 안내로 끝난다.
4. SSH 채널이 강제로 끊기면 서버의 sudo/FIFO 정리가 늦어질 수 있고, 앱은 `결과 미확인`으로 끝낸다. 로컬은 앱과 같은 디렉터리의 `zxt-askpass`를 배포 번들에 포함하고 root 소유·비쓰기로 설치해야 한다.


**완료 증거:** 대상/계정은 별칭으로, 증거는 승인·조회·응답 횟수와 종료 상태로 남긴다. 암호, 세션 키, 전체 PTY 녹화는 수집하지 않는다. [F01](01-ssh.md)의 Bitwarden 취소, [F02](02-editor.md)의 파일 충돌·단절·래퍼·대기 결과, 이 티켓의 sudo 수동 입력·명시적 1회 승인 결과를 함께 대조한 뒤에만 [T4](../../ops-shell-blueprint.md#검증)를 완료로 기록한다.

참고: [sudo의 `-A`, `-k`, askpass 동작](https://www.sudo.ws/docs/man/sudo.man/)
