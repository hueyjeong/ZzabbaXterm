# F00 · 새 개발 PC에서 ZXT 창 열기

선행: 없음

**사용자 결과:** Windows와 Ubuntu에서 같은 앱 창이 열린다. 개발자는 어떤 폴더에서 실행하고, 오류가 화면 빌드·Rust 빌드·네이티브 실행 중 어디서 났는지 구분할 수 있다. 이 티켓은 환경 준비 예외다. 터미널 흉내를 내는 화면을 만들지 말고 다음 L01에서 바로 실제 셸을 연결한다.

### 인수 예

| Given | When | Then |
|---|---|---|
| `docs/`만 있고 `app/`가 없는 저장소와 준비된 네이티브 PC | 아래 절차대로 앱을 만들고 실행한다 | `ZXT`라는 독립된 데스크톱 창이 열리고 닫힌다. 일반 브라우저 탭은 합격 증거가 아니다. |
| 다른 개발자가 잠금 파일까지 포함된 같은 소스를 받았다 | 기존 `app/`에서 복구 절차로 실행한다 | 템플릿을 다시 생성하거나 기존 문서를 덮어쓰지 않고 같은 창이 열린다. |

### 준비물

- **Windows 11:** Microsoft C++ Build Tools의 **Desktop development with C++**, Windows SDK, WebView2 Runtime, rustup의 stable MSVC 도구 체인, 호환되는 Node.js LTS·npm. PowerShell을 새로 연다.
- **Ubuntu 24.04 데스크톱:** [Tauri 사전 요구 사항의 Debian/Ubuntu 목록](https://v2.tauri.app/start/prerequisites/#linux)대로 C/C++ 도구와 `libwebkit2gtk-4.1-dev` 등 네이티브 패키지를 준비한다. Rust stable·Node.js LTS·npm을 설치하고 GNOME Wayland 세션 안에서 실행한다. IME와 AI CLI 설치는 L03에서 한다.
- `node --version`, `npm --version`, `rustc -Vv`, `cargo --version`으로 도구와 Rust 호스트를 확인한다. Node/Vite/Vitest는 [Vitest 요구 버전](https://vitest.dev/guide/)을 함께 만족하는 조합을 쓴다. 시스템 설치 권한이 없으면 해당 OS를 `미실행(도구 설치 필요)`으로 기록한다.

### 순서

1. **초기 상태(수동 Red):** F00 구현 전에는 `app/package.json`과 실행할 앱이 없었다. F00에서 [공식 `create-tauri-app` React TypeScript 템플릿](https://v2.tauri.app/start/create-project/)으로 `app/`을 생성했다. 이후에는 저장소의 `app/`과 두 잠금 파일을 사용한다.
2. **의존성 복구:** 저장소 루트에서 `cd app`을 실행한 뒤 `npm ci`로 `package-lock.json`에 고정된 의존성을 설치한다.
3. **빌드 확인:** `npm run build`로 TypeScript 검사와 `dist/` 생성을 확인한다. 이어서 `cargo check --locked --manifest-path src-tauri/Cargo.toml`로 `Cargo.lock`에 고정된 Rust 의존성을 확인한다. `npm run tauri info`에서 네이티브 도구와 WebView 런타임을 확인한다. F00에는 테스트 파일이 없어 `npm test`는 테스트가 없다는 오류로 종료된다.
4. **실기기 시연:** `npm run tauri dev`로 `ZXT` 창이 열리는 것을 확인하고 닫은 뒤 개발 명령을 끝낸다. 두 번째 OS에서는 2단계부터 반복한다. 창 대신 브라우저만 열렸다면 `npm run dev`와 `npm run tauri dev`를 구분한다. 빈 창이면 Vite 출력 URL과 Tauri의 `devUrl`(포트 `5173`)을 비교한다. Rust 링크 오류가 나면 해당 OS의 사전 요구 사항을 확인한다.

**완료 증거:** 두 OS의 도구 버전, 실행한 명령과 빌드 결과, 실제 주 창 제목·종료 관찰. AWS 계정이나 AI 모델은 필요 없다. 다음은 [L01 · 실제 셸](../01-local/01-shell.md)이다.
