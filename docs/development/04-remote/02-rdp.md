# R02 Windows RDP와 Linux xrdp를 같은 엔진으로 열기

선행: R01

사용자 결과: 가운데 탭에 실제 원격 데스크톱이 열리고, 키 입력과 클립보드가 선택한 데스크톱에만 전달된다. xrdp를 SSM 터널로 열어도 인증 대상은 원격 xrdp 서버다.

제목줄의 **RDP**는 직접 접속한다. 준비된 로컬 SSH 전달 행의 **RDP로 열기**는 Rust가 소유한 그 연결을 사용한다. 두 경로에서 원격 인증서 이름을 따로 확인하고 검증된 TLS 위에서 NLA/CredSSP NTLM을 사용한다. 서버가 NLA를 지원하지 않고 `PROTOCOL_SSL`(TLS 전용)만 고르는 경우에는 연결 설정의 **NLA를 지원하지 않는 서버(TLS 전용) 허용**을 켠 사용자만 같은 대상으로 한 번 재연결해 TLS 위 Client Info로 계정을 전달한다. 공식 SSM 전달은 현재 지원 범위 밖이다.

준비물: 네이티브 Windows 11·Ubuntu 24.04 GNOME Wayland 클라이언트, 승인된 비운영 Windows RDP 및 Linux xrdp 서버, 검증 가능한 원격 DNS 이름과 신뢰 CA, 시험용 원격 계정·필요한 경우 도메인, xrdp 관리 노드의 SSM 전달 권한. 정상 원격 접속 도구와 같은 계정으로 서비스·정책을 먼저 확인한다. 아래 명령은 구현 시점의 `app/` 기준이다.

## 인수 조건

- Given Windows RDP 또는 Linux xrdp / When 탭을 열고 원격 편집기에 시험 문구를 입력 / Then 서버가 보낸 프레임에 그 문구가 나타난다. xrdp 직접과 SSM 경유 모두 같은 원격 이름을 검증한다.
- Given TCP 상대가 루프백인 터널 / When 서버 인증서의 이름·유효기간·체인 중 하나가 틀림 / Then 서비스 자격 증명을 전송하기 전에 중단한다. 잘못된 비밀번호와 인증서 실패는 다른 상태다.
- Given 다른 탭이 열린 상태 / When 사용 중인 시험 터널을 종료하고 다시 연결 / Then 해당 탭만 끊기고 재접속 후 새 원격 프레임과 입력을 확인할 수 있다. 예전 세션의 입력은 재전송되지 않는다.
- Given NLA를 지원하지 않는 xrdp / When opt-in을 켜고 연결 / Then 첫 NLA 협상이 `SSL`을 받으면 연결을 닫고 같은 대상·같은 인증서 검증으로 TLS 전용 한 번만 재연결하며, 승인 카드와 연결 상태에 `TLS 전용(표준 RDP 보안 없음)`이 표시된다. opt-in이 꺼져 있으면 자격 증명을 보내지 않고 opt-in 이름을 포함한 오류로 끝난다.
- Given TLS 전용 연결 / When 인증서 이름·체인·유효기간 중 하나가 틀림 / Then 자격 증명을 Client Info로 보내기 전에 중단한다.

## TLS 전용 모드

- **정책**: 기본은 NLA 우선이다. 첫 연결은 `enable_credssp=true, enable_tls=false`로 `HYBRID|HYBRID_EX`만 요청하므로 서버가 TLS로 다운그레이드할 수 없다. 서버가 `SSL`을 고르면 연결을 닫고, opt-in이 켜진 경우에만 `enable_credssp=false, enable_tls=true`로 같은 대상에 한 번 재연결한다. 재시도는 한 번뿐이며 opt-in은 기본 꺼짐이다.
- **자격 증명**: TLS 전용에는 CredSSP가 없으므로 사용자·도메인·비밀번호는 검증된 TLS 위의 Client Info PDU로 전달한다. 승인 카드·금고 경로는 NLA와 같고, 비밀번호를 로그·argv·WebView에 남기지 않는다. 세션 활성화 후 `ConnectTimeAutoDetection`에서 config의 자격 증명을 비운다.
- **인증서**: TLS 전용도 원격 DNS 이름·체인·유효기간을 검증한다. 인증서 검증을 끄는 옵션은 없다. 자체 서명 서버는 시험용 CA를 OS 현재 사용자 신뢰 저장소에 설치해 검증하고, 사용이 끝나면 제거한다.
- **표시**: opt-in을 켜면 승인 카드와 연결 상태에 `NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)`을 표시한다. opt-in이 꺼진 채 서버가 `SSL`을 고르면 `RDP 서버가 NLA를 지원하지 않습니다. 연결 설정에서 'NLA를 지원하지 않는 서버(TLS 전용) 허용'을 켜야 합니다`로 중단한다.

## 구현 순서

1. **Red.** `app/src-tauri/src/rdp/`에 시험 CA로 발급한 이름 일치·불일치·만료 인증서와 다른 CA 사례를 넣고, 검증 실패 후 인증 단계 호출이 0인지 검사한다. `app/src/features/rdp/`에는 실제 프레임 수신 전 연결 표시, 폐기한 세션의 프레임, 비활성 탭 입력 억제를 Vitest 사례로 둔다.
2. **Green — 접속.** [IronRDP](https://github.com/Devolutions/IronRDP)의 네이티브 클라이언트 예제를 따라 `connector`·`session`·그래픽 디코더를 Rust에서 구동한다. TCP 주소(직접 또는 R01 루프백)와 TLS 검증용 원격 서버 이름을 별도 필드로 둔다. OS 신뢰 저장소 또는 승인된 시험 CA로 검증한 뒤 대상이 요구한 TLS/NLA 협상을 완료한다. 서비스용 계정·도메인과 사용자가 선택한 비밀 참조는 로컬 인증 컴포넌트에서만 해석한다. 인증서 전부 허용, 비밀번호 인자, WebView로의 금고 값 반환은 제외한다. 정상 서버가 요구하는 협상을 지원하지 못하면 협상 단계를 기록하고 구현 결함으로 남긴다.
3. **Green — 화면과 입력.** Rust가 발급한 세션 ID별로 프레임을 디코딩해 크기·변경 사각형·픽셀 바이트를 Tauri 채널로 보내고 탭의 canvas에 그린다. Rust에서 사각형과 길이를 확인한다. 포커스된 canvas의 키 누름/뗌과 포인터를 원격 좌표·RDP 입력 이벤트로 변환한다. 셸 PTY나 AI 입력을 경유하지 않는다. 탭 폐기 시 입력 핸들·터널 소유권을 함께 해제한다.
4. **Green — 주변 기능.** 협상한 CLIPRDR에서만 사용자가 복사한 시험 텍스트의 형식과 본문을 양방향 교환한다. Display Control 채널로 크기 변경을 요청하고 서버가 보낸 새 화면 크기로 canvas를 갱신한다. CSS 확대는 해상도 변경이 아니다. 단절 시 입력을 잠그고 재접속에서 경로와 인증서를 다시 검증해 새 ID의 프레임만 그린다.
5. **자동 검증.** `cargo test --manifest-path src-tauri/Cargo.toml rdp`와 `npx vitest run src/features/rdp`에서 신원·프레임·입력 경계를 검증한다. 조작한 인증서로도 첫 인증 단계에 진입하면 수정 후 재시험한다.
6. **실기기 시연(T5 RDP).** 각 클라이언트에서 `Windows RDP 직접`, `Linux xrdp 직접`, `Linux xrdp / SSM`을 별도 행으로 기록한다. 첫 프레임 → 키·클릭 → 서로 다른 무해한 문자열의 양방향 복사 → 서버 해상도 변경 전후 → 끊기·재접속 후 새 문자열 입력을 촬영한다. SSM을 끊는 동안 별도 탭의 입력도 기록한다. OS·서버·IronRDP 버전과 협상 채널을 붙인다. 서버 정책/협상과 기준 클라이언트에서 모두 막힌 기능만 해당 행의 `서버 미지원(근거)`이다. 앱에서만 실패하면 `실패(단계)`, 시험 환경이 없으면 `미실행(사유)`이다. 계정 비밀과 클립보드 원문은 로그에 넣지 않는다.

## 인증서 신뢰와 지문 고정

- **판정 순서**: 서버 인증서가 OS/CA 체인으로 검증되고 원격 DNS 이름이 맞으면 승인 창 없이 진행한다. 체인 검증이 실패하면(자체 서명 xrdp 등) 네이티브 승인 창에 원격 DNS, 대상 `host:port`, 주체, SAN, SHA-256 지문을 보여 주고 1회 승인을 받는다.
- **저장**: 승인한 지문은 `${appData}/rdp-trust.json`에 `(원격 DNS, 대상 host:port) → { fingerprint, approved_at }`으로 원자적 쓰기한다. 손상된 JSON은 빈 상태로 시작하며 조용히 신뢰하지 않는다.
- **변경**: 지문이 저장값과 다르면 연결을 중단하고 새 지문으로 다시 승인을 요구한다. 승인 전에는 자격 증명을 보내지 않는다. 자동 갱신은 없다.
- **범위**: TLS 전용(opt-in)과 NLA 경로 모두 같은 규칙을 쓴다. 인증서 검증을 끄는 옵션은 없다. 2026-11-23 실측 시점에 이 Windows 사용자 신뢰 저장소에는 시험 CA가 남아 있지 않다(`certutil -user -store Root`에서 0건).

## 실측 기록

- xrdp 0.9.24(Ubuntu 24.04 패키지, `security_layer=negotiate`)는 `HYBRID|HYBRID_EX`만 요청하면 `RDP_NEG_RSP selectedProtocol = 0x0`(RDP 표준 보안)을 돌려주고, `SSL`을 요청하면 `0x1`(TLS 전용)을 돌려준다. 즉 NLA를 모르는 서버는 SSL로 다운그레이드하지 않고 RDP 표준 보안을 고른다. 앱은 NLA가 아닌 선택을 모두 NLA 미지원으로 보고 opt-in이 켜진 경우에만 `SSL` 요청으로 한 번 재협상한다.
- 시험 컨테이너 `zzabba-demo-remote`(xrdp 0.9.24, SSH `127.0.0.1:2299`)에 `SAN=DNS:xrdp.demo.test,DNS:localhost`인 시험 CA 서명 인증서를 설치하고, 그 CA를 Windows 현재 사용자 신뢰 저장소에 넣은 뒤 `localhost:13389`(SSH 로컬 전달 → 컨테이너 `127.0.0.1:3389`)로 앱에서 연결했다. 승인 카드와 연결 상태에 `NLA, 미지원 시 TLS 전용(표준 RDP 보안 없음)`이 표시됐고, xrdp 로그에 `requested [HYBRID|HYBRID_EX|RDP], selected [RDP]` 뒤 `requested [SSL|RDP], selected [SSL]`, `TLSv1.3`, Client Info 수신과 capability 교환이 기록됐다. 인증서 이름 불일치는 자격 증명 입력 전에 중단된다.
- 압축 비트맵의 행 밀림(shear) 원인: xrdp가 RLE/RDP6 스트림을 원본 폭 8px로 보내는데 목적지 rect는 7px다. IronRDP는 원본 폭으로 디코드한 버퍼를 **rect 폭 7px stride로 적용**해 행마다 1px씩 밀렸다. `frame::normalize_bitmap`이 원본 폭으로 디코드한 뒤 각 행을 목적지 폭으로 잘라 내고 uncompressed 32bpp fast-path PDU로 다시 인코딩하도록 고쳤다. `check_bitmaps`의 압축 폭 일치 검사는 복원했고, 픽셀 값으로 밀림을 검사하는 `xrdp_compressed_bitmap_crops_source_stride_without_shear` 시험을 추가했다.
- 자동 로그온: TLS 전용에서 `config.autologon=true`를 켜 Client Info에 `INFO_AUTOLOGON`을 실었다. xrdp가 sesman 로그인 창을 띄우지 않고 바로 세션을 시작한다.
- 컨테이너 세션 설정: `xrdp.ini`의 `[Xorg]`가 `port=tcp://127.0.0.1:3389`(자기 자신)로 잘못 잡혀 있어 `port=-1`로 고쳤고, `/etc/xrdp/startwm.sh`가 `twm`과 `xterm`을 띄우도록 바꿨다.
- 최종 실측: `localhost:13389`(SSH 전달)로 연결해 지문 1회 승인 후 **로그인 창 없이 1024×768 데스크톱과 xterm이 표시**됐다. 원격 xterm에 `echo XRDP_OK`를 입력해 `XRDP_OK`가 출력되는 것을 확인했다. 스크린샷: 지문 승인 창 `C:\Users\deus\AppData\Local\Temp\omp-computer-15d902b0d2079de9.png`, 데스크톱 `C:\Users\deus\AppData\Local\Temp\omp-computer-15d903215cc79dee.png`, 키 입력 `C:\Users\deus\AppData\Local\Temp\omp-computer-15d9034e0b879df3.png`. 재연결 시 지문 승인 창은 뜨지 않고 연결 승인·계정 창만 나타났다(`C:\Users\deus\AppData\Local\Temp\omp-computer-15d9035d8fc79df4.png`).
- 확인 방법: 컨테이너의 `127.0.0.1:3389`에 X.224 `Connection Request` + `RDP_NEG_REQ`(`requestedProtocols`를 0x0b, 0x0a, 0x08, 0x02, 0x01로 각각)를 보내고 응답의 `selectedProtocol`을 읽는다. 0x0b·0x01은 0x1, 0x0a·0x08·0x02는 0x0이었다.
- 자동 시험: `cargo test rdp::connect::tests`의 `tls_only_retries_once_and_activates_with_client_info_credentials`, `tls_only_stops_before_credentials_when_the_certificate_name_differs`, `rejects_non_nla_selection_without_opt_in_and_legacy_modes_without_resolving_credentials`가 협상 매트릭스·자격 증명·인증서 중단을 고정한다. `cargo test rdp::ipc::tests`와 Vitest `src/features/rdp`가 승인 문구·opt-in 전달·표시를 확인한다.
