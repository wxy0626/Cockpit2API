# Cockpit2API

本项目基于原项目 [CockpitTools](https://github.com/jlcodes99/cockpit-tools) 制作。

本仓库只描述本地新增能力与使用方式，不复述原项目的完整功能清单。仓库只包含构建项目所需的源码，不包含本地数据库、账号数据或运行时配置。

## 文档

- [项目概览](docs/PROJECT.md)
- [本地网关说明](docs/LOCAL_GATEWAYS.md)

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

### `cindy2api`

新增 Cindy 平台网关和账号管理能力。网关可以从本机 Cindy 登录态发现账号，也能通过手机号、邮箱验证码或 OAuth 授权添加账号。

- 提供 OpenAI 兼容的 `/v1/models` 和 `/v1/chat/completions`
- 支持流式与非流式请求、账号池轮换和健康检查
- 支持 Cindy 国内手机号登录、国际邮箱验证码登录、OAuth 授权和本机导入
- 需要人机验证时自动唤起独立验证窗口
- 网关本地 Key 与上游凭据隔离，凭据不进入前端
- 本地端口：`7865`

### WorkBuddy 自动化与稳定性

把 WorkBuddy 的日常操作拆成可配置、可观测的后端模块，同时收编原生网关。

- 自动签到、自动旅行和成长中心任务可分别开关
- 任务执行日志可查看、清空和手动触发
- 任务调度与执行都在 Rust 后端，前端只负责配置和展示
- 原生网关支持单实例锁、端口释放后自动接管、token 刷新和账号轮换
- 单账号并发上限可内联配置，并参与选号权重

### 协议与 OAuth 兼容

- WorkBuddy 网关补齐 Responses 与 Chat Completions 的协议翻译，支持 Codex `spawn_agent` 链路
- OAuth 授权统一使用本机 Chrome 可信用户配置，减少第三方登录的设备信任问题

### 构建增量提速

重构 Tauri 构建边界，把前端资源、应用 bin 和 Tauri Context 迁到独立的 `context` crate，避免前端产物变化牵连主业务库全量重编。

- `build.rs` 会排序 `rerun-if-changed` 输出，避免构建脚本指纹随机失效
- Windows 构建使用 `rust-lld`，Release 使用 `opt-level = 2` 和更高并行度
- TypeScript 开启增量检查，日常构建提供 `build:fast`
- `npm run build:app` 是发布构建统一入口，自动带上正确的 `custom-protocol` feature

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

构建完成后，可执行文件位于 `target/release/cockpit2api.exe`。

## 使用步骤

1. 启动 Cockpit2API。
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

## 许可

本项目采用 [CC BY-NC-SA 4.0](https://creativecommons.org/licenses/by-nc-sa/4.0/) 公开许可，完整条款见 [LICENSE](LICENSE)。

- 允许在非商业场景下使用、修改和分享
- 分发时必须保留署名，并说明基于 CockpitTools 与 Cockpit2API 的来源
- 衍生项目必须继续使用相同许可协议
- 商业使用需另行获得权利人授权
