//! WorkBuddy 区域（realm）配置：国际版与国内版共用同一套网关协议，差异全部收敛在这里。
//!
//! 设计原则：
//! 1. 上层模块（oauth / account / gateway）不要硬编码域名或存储路径，一律从这里取。
//! 2. 两区域的账号池**物理隔离**（不同目录），deviceId 派生也带 realm 前缀，防止串号风控。
//! 3. 新增第三个区域时，只需在这里加一个常量，无需复制模块代码。

/// WorkBuddy 单个区域的全部配置
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkbuddyRealm {
    /// 区域标识，如 "cn" / "intl"，用于命令参数与日志
    pub id: &'static str,
    /// 网关域名（OAuth 与各业务接口的 host）
    pub api_endpoint: &'static str,
    /// 网关接口前缀
    pub api_prefix: &'static str,
    /// 网页主域，用于出站请求的 Origin / Referer 头
    pub web_origin: &'static str,
    /// 授权登录页域名（拼接 /login?state=...）
    pub login_origin: &'static str,
    /// 账号索引文件名（必须各区域不同，防止串号）
    pub accounts_index_file: &'static str,
    /// 账号存放目录名（必须各区域不同）
    pub accounts_dir: &'static str,
    /// 网关请求里的 platform 参数值
    pub platform_tag: &'static str,
    /// 该区域可用的模型白名单
    pub models: &'static [&'static str],
}

/// 国内版静态模型兜底白名单。
///
/// 动态模型列表优先；静态表只负责冷启动、缓存失效或上游模型列表暂时拉取失败时，
/// 继续把已确认属于国内版的模型路由到正确账号池，避免客户端持有旧模型列表时被本地误拒绝。
const MODELS_CN: &[&str] = &[
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "glm-5.1",
    "glm-5v-turbo",
    "kimi-k2.7",
    "minimax-m3",
    "hy3",
    "hy3-preview",
    "hy3-preview-agent",
    "deepseek-v4-pro",
    "deepseek-v4-flash",
    "kimi-k3-1",
];

/// 国际版模型白名单（16 项）。
/// 来源：`ardeyouxipianyi/workbuddy2api-hub` README 声明的"按官方桌面端主界面清洗收敛"清单。
/// ⚠️ 非本机实测，拿到国际版账号后应以带 token 的 /v1/models 复核。
const MODELS_INTL: &[&str] = &[
    "deepseek-v4.1-flash",
    "gpt-6-astra",
    "hy4-preview-f",
    "hy4-preview",
    "hy3",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.3-codex",
    "gemini-3.5-flash",
    "glm-5.3",
    "glm-5.2",
    "kimi-k3",
    "kimi-k2.6",
];

/// 国内版区域配置
pub const REALM_CN: WorkbuddyRealm = WorkbuddyRealm {
    id: "cn",
    api_endpoint: "https://copilot.tencent.com",
    api_prefix: "/v2/plugin",
    web_origin: "https://www.codebuddy.cn",
    login_origin: "https://copilot.tencent.com",
    accounts_index_file: "workbuddy_accounts.json",
    accounts_dir: "workbuddy_accounts",
    platform_tag: "workbuddy",
    models: MODELS_CN,
};

/// 国际版区域配置
pub const REALM_INTL: WorkbuddyRealm = WorkbuddyRealm {
    id: "intl",
    api_endpoint: "https://www.workbuddy.ai",
    api_prefix: "/v2/plugin",
    web_origin: "https://www.workbuddy.ai",
    login_origin: "https://www.workbuddy.ai",
    accounts_index_file: "workbuddy_intl_accounts.json",
    accounts_dir: "workbuddy_intl_accounts",
    platform_tag: "workbuddy",
    models: MODELS_INTL,
};

/// 按区域标识取配置；未知标识一律回退到国内版，避免上层到处写 unwrap。
pub fn realm_by_id(id: &str) -> &'static WorkbuddyRealm {
    match id.trim().to_lowercase().as_str() {
        "intl" | "global" => &REALM_INTL,
        _ => &REALM_CN,
    }
}

/// 拼接区域网关的完整接口地址，替换原先各处手写的 format!("{}{}...")。
pub fn api_url(realm: &WorkbuddyRealm, path: &str) -> String {
    format!("{}{}{}", realm.api_endpoint, realm.api_prefix, path)
}
