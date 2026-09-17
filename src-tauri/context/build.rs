// 双层设计：
//   1. CWD 切到上一级 src-tauri/，复用主 tauri.conf.json（tauri-build 只在当前目录找配置）
//   2. 子进程模式：tauri-build 输出的 cargo:rerun-if-changed 清单（resources/icons）
//      用 HashSet 遍历，**顺序随机** —— cargo 按输出文本哈希做指纹，顺序一变整个
//      下游就“无辜失效”，表现为每次 cargo build 都全量重编。这里捕获子进程输出，
//      把 rerun-if-changed 行排序后输出，使指纹确定化。
use std::io::Write;
use std::process::Command;

fn main() {
    if std::env::var("COCKPIT_CONTEXT_BUILD_CHILD").is_ok() {
        let target = std::env::var("TARGET").unwrap_or_default();
        if target.ends_with("-windows-msvc") {
            // 原先在 src-tauri/build.rs：为应用主程序保留 8 MiB 栈（Windows 递归场景）
            println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        }

        #[cfg(target_os = "macos")]
        {
            use swift_rs::SwiftLinker;
            SwiftLinker::new("12.0")
                .with_package("MacosNativeMenuSwift", "../../native/macos-native-menu")
                .link();
            println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
        }

        // 防呆守卫：release 构建必须启用 custom-protocol（内嵌前端资源）。
        // 漏带时产物会加载 devUrl 导致白屏（ERR_CONNECTION_REFUSED），
        // 与其让问题留到运行时，不如在这里直接报错并给出正确命令。
        let custom_protocol = std::env::var_os("CARGO_FEATURE_CUSTOM_PROTOCOL").is_some();
        let is_release = std::env::var("PROFILE").as_deref() == Ok("release");
        if is_release && !custom_protocol {
            panic!(
                "

========== 构建配置错误 ==========
                 release 构建缺少 cockpit-app-context/custom-protocol feature，
                 产物将加载 devUrl(http://localhost:1420) 导致白屏。

                 正确命令：
                   npm run build:app
                 （等价于 vite 构建 + cargo build --release
                   --features cockpit-app-context/custom-protocol）
                 ==================================
"
            );
        }

        tauri_build::build();
        return;
    }

    // 父进程：重新执行自己（子进程模式），捕获 stdout，排序 rerun-if-changed 后透传
    let exe = std::env::current_exe().expect("无法定位自身 build 脚本二进制");
    let output = Command::new(exe)
        .env("COCKPIT_CONTEXT_BUILD_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("重新执行 build 脚本失败");

    std::io::stdout().write_all(&normalize(&output.stdout)).unwrap();
    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }
}

/// 把 cargo:rerun-if-changed 行排序，其余指令保持原顺序
fn normalize(stdout: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(stdout);
    let mut rerun: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("cargo:rerun-if-changed="))
        .collect();
    rerun.sort();

    let mut result: Vec<String> = Vec::new();
    let mut rerun_iter = rerun.iter();
    let mut rerun_inserted = false;
    for line in text.lines() {
        if line.starts_with("cargo:rerun-if-changed=") {
            if !rerun_inserted {
                for r in &rerun {
                    result.push((*r).to_string());
                }
                rerun_inserted = true;
            }
            continue; // 原位置的乱序行跳过，用排序后的整块替代
        }
        result.push(line.to_string());
    }
    let _ = rerun_iter.next();

    let mut bytes = result.join("\n").into_bytes();
    bytes.push(b'\n');
    bytes
}
