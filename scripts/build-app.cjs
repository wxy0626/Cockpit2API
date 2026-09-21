/**
 * build-app.cjs —— 应用发布构建的统一入口。
 *
 * 为什么不用 `npx tauri build`：应用 bin 与 generate_context! 已迁入
 * src-tauri/context（构建提速架构，见 .workbuddy/plans/2026-09-17-build-speed.md），
 * tauri CLI 在 src-tauri 包里找不到 main binary。--no-bundle 模式下 CLI 只是
 * "npm 构建 + cargo 构建"的编排器，本脚本直接等价复刻并跳过它的限制。
 *
 * 用法：npm run build:app
 */
const { spawnSync } = require('node:child_process');
const path = require('node:path');

const repoRoot = path.resolve(__dirname, '..');
const isWin = process.platform === 'win32';

function run(command, args) {
  console.log(`> ${command} ${args.join(' ')}`);
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    stdio: 'inherit',
    shell: isWin && command.endsWith('.cmd'),
  });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

// 1) 前端（含 sync-version；build:fast 跳过 tsc，日常改动用 npm run build 全量校验）
run('npm.cmd', ['run', 'build:fast']);

// 2) cargo —— feature 必须挂在 context crate 上（cfg 以调用 generate_context! 的 crate 为准）
run('cargo', [
  'build',
  '--release',
  '--features',
  'cockpit-app-context/custom-protocol',
]);

console.log('\n✅ 构建完成: target/release/cockpit2api.exe');
