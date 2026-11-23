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

**현재 상태:** 로컬 승격 경로 구현·자동 시험 완료. 원격 승격 실행 경로는 미구현이며 SSH 탭은 직접 입력만 사용한다.

구현한 로컬 경로:

- `app/src-tauri/src/elevation/`가 명시적 요청에서 대상(`내 PC`), 호출 계정(OS 사용자), 실행 계정(`root`), 정규화한 절대 실행 파일, 인자 배열, 32자 요청 ID를 고정한다. 승인은 30초 만료, 문서 세대·요청 ID·대상·계정·실행 파일·인자 전체 일치, 1회 소비다. 승인 창은 인자를 `1.`, `2.`처럼 경계별로 표시한다.
- `app/src-tauri/src/auth/sudo.rs`는 승인 뒤 그 자식에만 `SUDO_ASKPASS`·`ZZABBA_ASKPASS_MODE=sudo`를 설정하고 `sudo -A -- <실행파일> <인자…>`를 시작한다. 교환은 `sudo` 자식의 직계 askpass(자식 계보·helper 실행 파일·pidfd 생존)만 받고, 실제 askpass 요청이 온 뒤에만 금고를 조회해 1회 응답하고 ACK 뒤 수신을 닫는다. 캐시된 인증이면 자식 종료로 끝나 금고 조회는 0회다.
- `app/src-tauri/src/auth/transport.rs`의 helper는 `SUDO_ASKPASS` 모드를 추가로 허용하되 `SUDO_ASKPASS`가 현재 helper와 같고 `SSH_ASKPASS`·`SSH_ASKPASS_REQUIRE`가 없으며 `BW_SESSION`이 없을 때만 동작한다.
- `app/src-tauri/src/terminal/{ipc.rs,process.rs}`에 `elevation_open` 명령, 네이티브 "sudo 승격 승인" 창, `Launch::Elevated`, 탭 종료 시 교환 취소, 금고 오류 알림을 연결했다. `app/src/features/elevation/`은 실행 파일·인자(줄 단위)·금고 항목을 입력받고 인자 경계를 나눈 승인 카드를 표시한다. 터미널 출력이나 AI 제안은 이 명령을 부르지 않는다.

실측(`zzabbaxterm-demo-remote:latest`, Ubuntu 24.04.5. 컨테이너에 `sudo`가 없고 `apt-get install sudo`도 `E: Unable to locate package sudo`로 실패하여, Ubuntu 24.04.4의 `sudo` 1.9.15p5와 의존 라이브러리를 시험용으로 복사해 측정):

- 사용자 uid 1002로 `SUDO_ASKPASS=… sudo -A -- /usr/bin/id -u`를 실행하면 `0`이 출력된다. askpass 프로세스는 `uid=1002 euid=1002`, `ppid=<sudo>`, stdout은 FIFO 파이프다. 따라서 Linux 전송의 `peer.uid == geteuid()` 검사는 `sudo -A` helper에도 성립하며 자식 계보 정책으로 다시 설계할 필요가 없다.
- setuid 부모의 `/proc/<sudo>/exe`와 fd는 helper 사용자 권한으로 읽을 수 없으므로, sudo 경로는 루트 실행 파일·출력 파이프 대신 자식 계보와 helper 신원으로 검증한다. 이 때문에 helper는 `sudo`의 직계 자식이어야 한다.
- `SUDO_ASKPASS`, `SSH_ASKPASS`, `ZZABBA_ASKPASS_ENDPOINT`, `ZZABBA_ASKPASS_BROKER_PID`, `ZZABBA_ASKPASS_MODE`가 askpass에 전달된다. 캐시된 인증에서는 askpass 호출과 금고 조회가 0회다.

자동 시험 결과:

- `cargo test --manifest-path app/src-tauri/Cargo.toml --features stored-auth --lib elevation` → 14 passed, 0 failed(13개 F03 시험 + 이름이 겹친 기존 `local_ipc` 1개). 승인 대상 불일치·만료·다른 세션/계정/실행 파일/인자, 재응답 거부, 취소, 캐시 인증 0회 조회, 신뢰되지 않은 helper, 계보 밖 helper, 인자 경계 표시, `sudo -A` 인자·환경을 검사한다.
- `npx vitest run src/features/elevation/elevation.test.tsx` → 4 passed. 승인 카드가 인자를 나눠 표시하고, `[sudo] password for demo:` 출력만으로는 승격 요청이 생기지 않음을 검사한다. `tsc --noEmit` 통과.

남은 작업:

1. **원격 sudo 실행 경로 미구현.** 관리자가 설치·검증한 고정 helper와 F01이 검증한 같은 서버·계정의 별도 SSH 채널을 사용하는 실행·askpass 교환·`결과 미확인` 상태가 아직 없다. SSH 탭에서는 직접 입력만 가능하며, helper가 없는 대상도 직접 입력만 허용한다.
2. 원격 실측 불가. `zzabbaxterm-demo-remote`에 `sudo`가 없고 관리자 설치 helper도 없어 원격 승격 1회 승인 → `id -u` = `0` 시험을 실행하지 못했다. 컨테이너에 sudo를 설치하려던 `apt-get install sudo`는 `E: Unable to locate package sudo`로 실패했다.
3. 제품 UI로 실제 `sudo -k` + 암호 요구 계정에서 승인 1회·응답 1회·종료 코드 0을 시연하는 Linux 네이티브 실행 시험은 미실행이다. 자동 시험은 주입한 fixture helper·sudo·금고로 같은 계약을 검사한다.
4. 개발 빌드의 앱·helper가 root 소유가 아니면 `installed_helper()`가 실패하여 제품은 승인 창 대신 "직접 입력" 안내로 중단한다. 배포 패키지에서 앱과 helper를 root 소유·비쓰기로 설치해야 이 경로가 열린다.


**완료 증거:** 대상/계정은 별칭으로, 증거는 승인·조회·응답 횟수와 종료 상태로 남긴다. 암호, 세션 키, 전체 PTY 녹화는 수집하지 않는다. [F01](01-ssh.md)의 Bitwarden 취소, [F02](02-editor.md)의 파일 충돌·단절·래퍼·대기 결과, 이 티켓의 sudo 수동 입력·명시적 1회 승인 결과를 함께 대조한 뒤에만 [T4](../../ops-shell-blueprint.md#검증)를 완료로 기록한다.

참고: [sudo의 `-A`, `-k`, askpass 동작](https://www.sudo.ws/docs/man/sudo.man/)
