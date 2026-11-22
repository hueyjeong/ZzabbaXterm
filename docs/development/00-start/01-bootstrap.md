# F00 · 새 개발 PC에서 ZzabbaXterm 창 열기

선행: 없음

**사용자 결과:** Windows와 Ubuntu에서 같은 앱 창이 열린다. 개발자는 어떤 폴더에서 실행하고, 오류가 화면 빌드·Rust 빌드·네이티브 실행 중 어디서 났는지 구분할 수 있다. 이 티켓은 환경 준비 예외다. 터미널 흉내를 내는 화면을 만들지 말고 다음 L01에서 바로 실제 셸을 연결한다.

### 인수 예

| Given | When | Then |
|---|---|---|
| `docs/`만 있고 `app/`가 없는 저장소와 준비된 네이티브 PC | 아래 절차대로 앱을 만들고 실행한다 | `ZzabbaXterm`이라는 독립된 데스크톱 창이 열리고 닫힌다. 일반 브라우저 탭은 합격 증거가 아니다. |
| 다른 개발자가 잠금 파일까지 포함된 같은 소스를 받았다 | 기존 `app/`에서 복구 절차로 실행한다 | 템플릿을 다시 생성하거나 기존 문서를 덮어쓰지 않고 같은 창이 열린다. |

### 준비물

- **Windows 11:** Microsoft C++ Build Tools의 **Desktop development with C++**, Windows SDK, WebView2 Runtime, rustup의 stable MSVC 도구 체인, 호환되는 Node.js LTS·npm. PowerShell을 새로 연다.
- **Ubuntu 24.04 데스크톱:** [Tauri 사전 요구 사항의 Debian/Ubuntu 목록](https://v2.tauri.app/start/prerequisites/#linux)대로 C/C++ 도구와 `libwebkit2gtk-4.1-dev` 등 네이티브 패키지를 준비한다. Rust stable·Node.js LTS·npm을 설치하고 GNOME Wayland 세션 안에서 실행한다. IME와 AI CLI 설치는 L03에서 한다.
- `node --version`, `npm --version`, `rustc -Vv`, `cargo --version`으로 도구와 Rust 호스트를 확인한다. Node/Vite/Vitest는 [Vitest 요구 버전](https://vitest.dev/guide/)을 함께 만족하는 조합을 쓴다. 시스템 설치 권한이 없으면 해당 OS를 `미실행(도구 설치 필요)`으로 기록한다.

### 순서

1. **Red(수동):** 처음에는 `app/package.json`과 실행할 앱이 없다. 이 상태를 F00의 시작 조건으로 기록한다. 이미 `app/`가 있다면 내용을 보존하고 4단계의 복구 경로를 사용한다.
2. **최초 한 번만, 저장소 루트:** `npm create tauri-app@latest`를 실행한다. 프로젝트 이름은 `app`, 식별자는 팀이 승인한 고유 역도메인(개발 예: `dev.zzabbaxterm.desktop`), 프런트엔드는 `TypeScript / JavaScript` → `npm` → `React` → `TypeScript`를 고른다. [공식 생성 절차](https://v2.tauri.app/start/create-project/)와 비교한다. `app/src-tauri/Cargo.toml`과 프런트 패키지에서 Tauri 메이저가 2인지 확인한다. 생성한 메이저가 다르면 여기서 멈추고 공식 템플릿의 Tauri 2 호환 버전을 선택한다.
3. **`cd app` 후 Green:** `npm install`, `npm install -D vitest`, `npm pkg set "scripts.test=vitest run"` 순으로 실행한다. `src-tauri/tauri.conf.json`에서 `productName`과 `app.windows`의 주 창 제목을 `ZzabbaXterm`으로 맞춘다. Vite 개발 포트와 Tauri `build.devUrl`의 포트를 같게 두고, `beforeDevCommand`는 `npm run dev`, `beforeBuildCommand`는 `npm run build`, `frontendDist`는 `../dist`로 맞춘다. 템플릿 기본 포트를 유지해도 된다. 아직 테스트 파일이 없으므로 빈 테스트 실행을 통과 처리할 옵션은 넣지 않는다.
4. **재현 경로:** 최초 PC에서는 `cargo check --manifest-path src-tauri/Cargo.toml`로 Rust 의존성을 해석하고 `src-tauri/Cargo.lock`과 `package-lock.json`을 소스와 함께 보존한다. 두 번째 PC에서는 생성기를 건너뛰고 `app/`에서 `npm ci`를 실행한다. 두 PC 모두 `npm run build`로 `dist/` 생성을, `cargo check --locked --manifest-path src-tauri/Cargo.toml`로 고정된 Rust 의존성의 빌드를 확인한다. 프런트 빌드 성공은 아직 데스크톱 합격이 아니다.
5. **실기기 시연:** `app/`에서 `npm run tauri dev`를 실행한다. 주 창의 제목을 확인하고 닫은 뒤 개발 명령을 끝낸다. 두 번째 OS에서는 4단계 재현 경로부터 반복한다. 창 대신 브라우저만 열렸다면 `npm run dev`와 `npm run tauri dev`를 구분한다. 빈 창이면 먼저 Vite 출력 URL과 `devUrl`, Rust 링크 오류면 1차로 OS 사전 요구 사항을 확인한다.

**완료 증거:** 두 OS의 도구 버전, 실행한 명령과 빌드 결과, 실제 주 창 제목·종료 관찰. AWS 계정이나 AI 모델은 필요 없다. 다음은 [L01 · 실제 셸](../01-local/01-shell.md)이다.
