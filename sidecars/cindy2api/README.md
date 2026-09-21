# cindy2api —— Cindy 反代网关 sidecar

把本机 **Cindy 账号**变成一台 **OpenAI 兼容网关**，供本机其他工具调用。

与 `sidecars/wb2api` 同族：独立可执行文件，**不随 CockpitTools 打包**
（`src-tauri/tauri.conf.json` 的 `externalBin` 只注册了 `cockpit-cliproxy`）。

## 启动

```powershell
powershell -File build-sidecar.ps1          # 构建到 bin/cindy2api.exe
.\bin\cindy2api.exe                          # 启动（首次运行自动生成 runtime/config.json）
```

启动后：

| 地址 | 用途 |
|---|---|
| `http://127.0.0.1:7865/v1` | OpenAI 兼容网关（需 `Authorization: Bearer <本地 Key>`） |
| `http://127.0.0.1:7865/api/*` | 管理接口（仅本机，供 CockpitTools 面板读取） |

本地 API Key 在启动日志与 `/api/config` 中给出，也可直接在 CockpitTools 的
「WorkBuddy → 供应商」页看到。

## 账号从哪来

**不需要输入账号密码，也不需要 Cindy 保持运行。** 网关直接读取本机 Cindy 桌面端
的数据目录（含 dev / 隔离 profile）：

| 步骤 | 位置 | 内容 |
|---|---|---|
| 1 | `{userData}/Local State` | `os_crypt.encrypted_key`，DPAPI 保护的主密钥 |
| 2 | `{userData}/safe-storage/owner_<ownerId>_api_key.enc` | v10 AES-256-GCM 加密的网关 key |
| 3 | `{userData}/owners/<ownerId>/model-access-credentials.json` | 该账号的推理 endpoint（明文） |

账号按 `ownerId` 去重（实测同一 owner 的 key 跨 profile 完全一致）。key 轮换后
网关每分钟自动重读一次并跟随。

## 配置（runtime/config.json）

| 字段 | 默认 | 说明 |
|---|---|---|
| `listen` | `127.0.0.1:7865` | 监听地址。要容器/局域网访问改成 `0.0.0.0:7865` |
| `api_key` | 自动生成 | 网关对外的本地 key，与上游 key 完全隔离 |
| `max_account_attempts` | `3` | 单个请求最多尝试几个账号 |
| `check_interval_seconds` | `300` | 账号巡检间隔 |
| `refresh_interval_seconds` | `60` | 重新读取本机凭据的间隔 |
| `upstream_timeout_seconds` | `0` | 单次上游请求超时；0 = 不限（流式场景） |

出站代理走环境变量：`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY`。

## 接口

| 路径 | 说明 |
|---|---|
| `GET /health` | 账号计数 `{"accounts":"1/3"}` |
| `GET /api/config` | 接入信息（`baseUrl` / `lan_base_url` / `config`） |
| `GET /api/status` | 账号池状态（`total` / `healthy` / `accounts`） |
| `GET /api/accounts` | 账号池详情 + 聚合模型 |
| `GET /api/models` | 聚合模型清单（OpenAI 格式） |
| `POST /api/check` | 立即重新检测所有账号 |
| `POST /api/refresh` | 重新从本机 Cindy 目录发现账号 |
| `GET /v1/models` | 模型清单（需本地 key） |
| `POST /v1/chat/completions` | 主通道，支持 `stream: true/false`（需本地 key） |

响应头 `X-Gateway-Account` / `X-Gateway-Upstream` 可看出请求最终落到哪个账号。

## 实现要点（都来自实测）

1. **上游本来就是 OpenAI 兼容 API** —— 所以本网关不做任何协议翻译，只是
   「账号池 + 凭据注入 + 转发」。
2. **key 与 endpoint 同租户不可拆开**：跨租户调用返回 `401 Invalid proxy server token`，
   因此凭据按 owner 成对保存与使用。
3. **换号只能发生在尚未写出响应字节之前**：一旦开始回传，错误只能如实传递。
4. **流式必须逐块 Flush**：否则 `net/http` 会缓冲到请求结束才吐给客户端。
5. **凭据只存在于内存**：配置里只有网关自己的本地 key，上游 key 不落盘、不入前端。

## 限制

- 上游模型清单随账号权限变化，按实时透传，不做模型名映射
- 额度与计费由上游账号承担；共享给外部工具是否合规请自行判断
- 不主动调用上游的 key 轮换接口（会作废 Cindy 桌面端正在使用的 key）

## 添加账号

账号有两个来源，界面上对应「添加账号」对话框的两个页签：

| 页签 | 机制 | 前提 |
|---|---|---|
| **OAuth 授权** | 复刻 Cindy 桌面端的托管回调授权：浏览器授权 → 轮询取授权码 → PKCE 兑换令牌 → 换取 `{endpoint, apiKey}` | 账号需绑定 **Apple / Google**（或企业 SSO） |
| **本机导入** | 直接扫描本机 Cindy 桌面端登录态（`owner_*_api_key.enc`） | 无 —— 零交互，100% 可用 |

> **关于 OAuth 的硬限制**：Cindy 服务端的 `authorize` 端点只接受 `social` / `sso`
> 两类（实测 `GET /api/auth/providers`：国际版为 `[apple, google]`，中国大陆版为 `[apple]`），
> **没有"邮箱验证码版"的授权流**。因此手机号/邮箱注册且未绑定社交账号的 Cindy 账号，
> 请走「本机导入」。

授权相关接口：

| 路径 | 说明 |
|---|---|
| `GET /api/login/providers?region=global\|cn` | 该区域支持的登录方式（转发上游） |
| `POST /api/login/oauth/start` | 创建授权会话并返回授权地址；请求体 `openBrowser` 省略或 `true` 时由 sidecar 拉起系统浏览器，传 `false` 时只返回地址（交给宿主应用用应用内无痕窗口打开） |
| `POST /api/login/oauth/poll` | 轮询授权结果；成功后自动兑换并落成账号 |
| `POST /api/accounts/remove` | 移除**授权添加**的账号（本机账号不可删） |

手动添加的账号落在 `runtime/accounts.json`（权限 `0600`），与自动发现的账号在账号池中
以 `source` 字段区分（`local` / `oauth`）。

