const { spawn, spawnSync } = require('node:child_process');

function resolveMacosSdkRoot() {
  if (process.platform !== 'darwin') {
    return process.env.SDKROOT;
  }
  // Some shells/Xcode setups leave SDKROOT pointing at iPhoneOS, which breaks
  // swift-rs / macos-native-menu (targets arm64-apple-macosx).
  const current = process.env.SDKROOT || '';
  if (current && /MacOSX\.platform|MacOSX[^/]*\.sdk/i.test(current)) {
    return current;
  }
  const probed = spawnSync('xcrun', ['--sdk', 'macosx', '--show-sdk-path'], {
    encoding: 'utf8',
  });
  if (probed.status === 0) {
    const path = String(probed.stdout || '').trim();
    if (path) return path;
  }
  return current || undefined;
}

const env = {
  ...process.env,
  COCKPIT_TOOLS_PROFILE: process.env.COCKPIT_TOOLS_PROFILE || 'dev',
  COCKPIT_TOOLS_API_PORT: process.env.COCKPIT_TOOLS_API_PORT || '1456',
  VITE_COCKPIT_TOOLS_PROFILE: process.env.VITE_COCKPIT_TOOLS_PROFILE || 'dev',
};
const macosSdkRoot = resolveMacosSdkRoot();
if (macosSdkRoot) {
  env.SDKROOT = macosSdkRoot;
}
const extraArgs = process.argv.slice(2);

const isWindows = process.platform === 'win32';

const syncResult = spawnSync('npm', ['run', 'sync-version'], {
  stdio: 'inherit',
  env,
  // Windows 下 npm/tauri 是 .cmd 垫片，新版本 Node 禁止无 shell 直接 spawn，必须走 shell
  shell: isWindows,
});

if (syncResult.status !== 0) {
  process.exit(syncResult.status ?? 1);
}

// ---------------------------------------------------------------------------
// 以异步方式托管 tauri dev 进程树，并在一切退出路径上对整树强杀回收：
// `tauri dev` 是一棵进程树（CLI → vite + cargo/rustc + 应用窗口），
// Windows 下任一环节被强杀都不会自动带走其他成员，因此包装层必须兜底清理，
// 否则 vite(1420 端口) / cargo / rustc 会残留为孤儿进程。
// ---------------------------------------------------------------------------

let tauriProcess = null;
let cleaningUp = false;

/// 对 tauri dev 整棵进程树执行强制回收（Windows 用 taskkill /T，其他平台 SIGTERM）
function killTauriTree() {
  if (!tauriProcess || cleaningUp) {
    return;
  }
  cleaningUp = true;
  if (isWindows) {
    // /T = 连子进程一起杀，/F = 强制；stdlib spawnSync 同步执行保证在 exit 钩子里也能生效
    spawnSync('taskkill', ['/PID', String(tauriProcess.pid), '/T', '/F'], {
      stdio: 'ignore',
      shell: true,
    });
  } else {
    try {
      tauriProcess.kill('SIGTERM');
    } catch {
      // 进程已退出则忽略
    }
  }
}

// 任何退出路径（正常退出 / Ctrl+C / 托盘关闭应用导致 CLI 退出 / 崩溃）都兜底清理
process.on('exit', () => killTauriTree());
process.on('SIGINT', () => {
  killTauriTree();
  process.exit(0);
});
process.on('SIGBREAK', () => {
  killTauriTree();
  process.exit(0);
});

tauriProcess = spawn(
  'tauri',
  ['dev', '--config', 'src-tauri/tauri.dev.conf.json', ...extraArgs],
  {
    stdio: 'inherit',
    env,
    shell: isWindows,
  },
);

tauriProcess.on('exit', (code) => {
  // CLI 自行退出（应用托盘退出 / 崩溃 / 被强杀）：兜底树杀，确保 vite/cargo 不残留
  killTauriTree();
  process.exit(code ?? 0);
});
