# Cockpit Tools

本项目基于原项目 [CockpitTools](https://github.com/jlcodes99/cockpit-tools) 制作。

本仓库只描述本地新增能力与使用方式，不复述原项目的完整功能清单。仓库只包含构建项目所需的源码，不包含本地数据库、账号数据或运行时配置。

## 本地亮点

### `workbuddy2api`

把本地 WorkBuddy 账号池暴露成 OpenAI 兼容接口。应用启动后，网关会自动读取 WorkBuddy 账号库中的可用凭据，并负责 token 刷新、账号轮换和请求转发。

- 支持 `/v1/chat/completions`、`/v1/responses`、`/v1/models` 和 `/healthz`
- 支持流式 SSE 与非流式响应
- 支持 WorkBuddy 国内版和国际版账号
- 对 429、额度不足和上游 5xx 做分级冷却、熔断与账号切换
- 支持按账号配置并发上限
- 本地 API 端口：`7863`，管理端端口：`7864`

### `qoder2api`

把 QoderWork 登录态转换成 OpenAI Chat Completions 兼容接口。网关会刷新本地凭据，把客户端请求转发到 Qoder 上游，并把上游强制返回的 SSE 流按需透传或聚合成普通 JSON。

- 支持 `/v1/chat/completions`、`/v1/models`、`/api/status` 和 `/api/config`
- 支持流式 SSE 透传与非流式聚合
- 支持模型可见名与上游模型 ID 映射
- 会合并本机 Qoder、QoderWork 国际版账号和桌面端登录态
- 401、403 或额度不可用时会在账号池内继续尝试
- 本地端口：`7866`

## 环境要求

- Node.js 24
- Rust stable
- Tauri 对应当前操作系统的 WebView/构建依赖

## 启动与构建

开发模式：

```bash
npm install
npm run tauri:dev
```

Windows 本地发布构建：

```bash
npm install
npm run build:app
```

构建完成后，可执行文件位于 `target/release/cockpit-tools.exe`。

## 使用步骤

1. 启动 Cockpit Tools。
2. 在 `WorkBuddy` 或 `WorkBuddy 国际版` 页面添加账号。
3. 在 `QoderWork 国际版` 页面添加账号。
4. 进入网关面板复制 Base URL 和 API Key：
   - WorkBuddy：`WorkBuddy` 页面中的 `OpenAI兼容网关`
   - QoderWork：`Qoder` 页面中的 `Qoder API 网关`
5. 把 OpenAI SDK 的 `baseURL` 指向对应网关，`apiKey` 设置为面板中的本地 Key。

### 网关地址

| 服务 | 本机地址 | 容器访问地址 |
| --- | --- | --- |
| WorkBuddy2API | `http://127.0.0.1:7863/v1` | `http://host.docker.internal:7863/v1` |
| Qoder2API | `http://127.0.0.1:7866/v1` | `http://host.docker.internal:7866/v1` |

### 调用示例

```bash
curl -N http://127.0.0.1:7863/v1/chat/completions \
  -H "Authorization: Bearer <WORKBUDDY_API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "hy3",
    "messages": [
      { "role": "user", "content": "你好" }
    ],
    "stream": true
  }'
```

```bash
curl http://127.0.0.1:7866/v1/chat/completions \
  -H "Authorization: Bearer <QODER_API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qmodel_38max",
    "messages": [
      { "role": "user", "content": "你好" }
    ],
    "stream": false
  }'
```

## 安全提示

- 两个网关只应用于你本人拥有或已授权的账号。
- 网关会绑定局域网地址，不要直接暴露到公网；如需跨机访问，请使用防火墙、局域网隔离或反向代理保护。
- 不要把网关 API Key、账号数据、日志或运行时配置提交到仓库。
