// 릴리스 워크플로가 쓰는 버전 도구다. app/ 에서 실행한다.
//   node scripts/release-version.mjs next <patch|minor|major> [직접 지정한 버전]  → 다음 버전을 출력한다
//   node scripts/release-version.mjs apply <버전>                                → 버전이 들어 있는 네 파일을 고친다
import { readFileSync, writeFileSync } from 'node:fs';

const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const files = {
  tauri: 'src-tauri/tauri.conf.json',
  cargo: 'src-tauri/Cargo.toml',
  lock: 'src-tauri/Cargo.lock',
  npm: 'package.json',
  npmLock: 'package-lock.json',
};

function current() {
  const version = JSON.parse(readFileSync(files.tauri, 'utf8')).version;
  if (!SEMVER.test(version)) throw new Error(`tauri.conf.json 버전 형식 오류: ${version}`);
  return version;
}

function next(bump, explicit) {
  const [major, minor, patch] = current().split('.').map(Number);
  if (explicit) {
    if (!SEMVER.test(explicit)) throw new Error(`버전 형식 오류: ${explicit}`);
    const [a, b, c] = explicit.split('.').map(Number);
    if (a < major || (a === major && (b < minor || (b === minor && c <= patch)))) {
      throw new Error(`버전 ${explicit}이 현재 버전 ${current()}보다 커야 합니다`);
    }
    return explicit;
  }
  if (bump === 'major') return `${major + 1}.0.0`;
  if (bump === 'minor') return `${major}.${minor + 1}.0`;
  if (bump === 'patch') return `${major}.${minor}.${patch + 1}`;
  throw new Error(`올릴 단계 오류: ${bump}`);
}

/** 정확히 한 곳만 바뀌어야 한다. 0곳이나 여러 곳이면 파일 구조가 바뀐 것이므로 멈춘다. */
function replaceOnce(path, pattern, replacement) {
  const text = readFileSync(path, 'utf8');
  const matches = text.match(new RegExp(pattern.source, pattern.flags.includes('g') ? pattern.flags : `${pattern.flags}g`));
  if (!matches || matches.length !== 1) throw new Error(`${path}: 버전 위치를 정확히 하나 찾지 못했습니다(${matches?.length ?? 0}곳)`);
  writeFileSync(path, text.replace(pattern, replacement));
}

function apply(version) {
  if (!SEMVER.test(version)) throw new Error(`버전 형식 오류: ${version}`);
  replaceOnce(files.tauri, /^(  "version": )"[^"]+"/m, `$1"${version}"`);
  replaceOnce(files.npm, /^(  "version": )"[^"]+"/m, `$1"${version}"`);
  // package-lock.json은 최상위와 packages[""] 두 곳에 같은 버전이 있다.
  replaceOnce(files.npmLock, /^(  "version": )"[^"]+"/m, `$1"${version}"`);
  // Windows 체크아웃은 줄 끝이 CRLF일 수 있다.
  replaceOnce(files.npmLock, /^(    "": \{\r?\n      "name": "[^"]+",\r?\n      "version": )"[^"]+"/m, `$1"${version}"`);
  replaceOnce(files.cargo, /^(\[package\]\r?\n(?:[^[\r\n][^\n]*\n)*?version = )"[^"]+"/m, `$1"${version}"`);
  replaceOnce(files.lock, /^(name = "zxt-app"\r?\nversion = )"[^"]+"/m, `$1"${version}"`);
}

const [command, ...args] = process.argv.slice(2);
if (command === 'next') console.log(next(args[0], args[1] || undefined));
else if (command === 'apply') apply(args[0]);
else throw new Error('사용법: release-version.mjs next <patch|minor|major> [버전] | apply <버전>');
