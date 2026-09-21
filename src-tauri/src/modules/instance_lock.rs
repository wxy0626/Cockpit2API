//! 应用单实例互斥锁（跨 dev/release 配置生效）。
//!
//! 原理：占住 127.0.0.1 的一个固定端口作为实例锁——
//! 先启动的实例持有监听器；后启动的绑定失败即判定已有实例在运行。
//! 相比 tauri-plugin-single-instance（按应用标识符加锁，dev/release 标识符不同可并存），
//! 本锁对所有构建形态统一互斥。
//!
//! 选型说明：锁端口刻意避开网关(7863/7864)/旅行(57890)/vite(1420)/应用API(1456)等既有端口。

use std::net::TcpListener;
use std::sync::OnceLock;

/// 实例锁监听器：持有不关闭，进程退出时由系统自动释放
static INSTANCE_LOCK: OnceLock<TcpListener> = OnceLock::new();

/// 实例锁端口（127.0.0.1 本地回环，不对外）
const INSTANCE_LOCK_PORT: u16 = 27863;

/// 尝试获取实例锁；获取失败说明已有实例在运行，调用方应立即终止本次启动
pub fn acquire() -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", INSTANCE_LOCK_PORT)).map_err(|_| {
        format!(
            "端口 {} 已被占用：另一个 Cockpit Tools 实例正在运行",
            INSTANCE_LOCK_PORT
        )
    })?;
    let _ = INSTANCE_LOCK.set(listener);
    Ok(())
}
