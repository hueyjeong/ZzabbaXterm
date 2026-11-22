import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readFileSync } from 'node:fs';
import { basename, join } from 'node:path';

const formats = { win32: 'nsis', linux: 'deb', darwin: 'app' };
const format = formats[process.platform];
if (!format) throw new Error(`Unsupported packaging platform: ${process.platform}`);

const manifest = 'src-tauri/Cargo.toml';
const config = 'src-tauri/tauri.bundle.conf.json';
const tauri = 'node_modules/@tauri-apps/cli/tauri.js';
const target = execFileSync('rustc', ['--print', 'host-tuple'], { encoding: 'utf8' }).trim();
const suffix = process.platform === 'win32' ? '.exe' : '';
const { target_directory: targetDir } = JSON.parse(execFileSync('cargo', [
  'metadata', '--locked', '--no-deps', '--format-version', '1', '--manifest-path', manifest,
], { encoding: 'utf8' }));
const { bundle: { externalBin } } = JSON.parse(readFileSync(config, 'utf8'));
const run = (program, args) => execFileSync(program, args, { stdio: 'inherit' });

run('cargo', [
  'build', '--release', '--locked', '--target', target, '--manifest-path', manifest,
  ...externalBin.flatMap((path) => ['--bin', basename(path)]),
]);
mkdirSync('src-tauri/binaries', { recursive: true });
for (const path of externalBin) {
  const name = basename(path);
  copyFileSync(join(targetDir, target, 'release', `${name}${suffix}`), `src-tauri/${path}-${target}${suffix}`);
}

run(process.execPath, [
  tauri, 'build', '--config', config, '--target', target, '--bundles', format, '--no-sign',
  '--', '--locked', '--no-default-features',
]);
