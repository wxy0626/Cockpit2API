# 本地网关

本文档只记录 `workbuddy2api` 和 `qoder2api` 两个本地网关的实际使用方式。

## WorkBuddy2API

WorkBuddy2API 把本地 WorkBuddy 账号池暴露成 OpenAI 兼容接口，由网关负责 token 刷新、账号轮换、冷却、熔断和请求转发。

### 端口

| 用途 | 地址 |
| --- | --- |
| OpenAI 兼容 API | `http://127.0.0.1:7863/v1` |
| 管理端 | `http://127.0.0.1:7864` |

### 接口

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `POST` | `/v1/chat/completions` | OpenAI Chat Completions |
| `POST` | `/v1/responses` | OpenAI Responses，入站翻译后转发 |
| `GET` | `/v1/models` | 模型列表 |
| `GET` | `/healthz` | 健康探活 |
| `GET` | `/api/status` | 网关状态 |
| `GET` | `/api/accounts` | 本地账号列表 |

### 鉴权

客户端使用 `Authorization: Bearer <API_KEY>` 访问。面板中的 API Key 为空时网关不鉴权，非空时统一校验 Bearer Token。

## Qoder2API

Qoder2API 把本机 Qoder、QoderWork 国际版账号和桌面端登录态合并为本地账号池，并以 OpenAI Chat Completions 兼容形式转发到 Qoder 上游。上游强制流式返回，网关会按客户端要求透传 SSE 或聚合为普通 JSON。

### 端口

| 用途 | 地址 |
| --- | --- |
| OpenAI 兼容 API | `http://127.0.0.1:7866/v1` |
| 状态 / 配置 | `http://127.0.0.1:7866/api` |

### 接口

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `POST` | `/v1/chat/completions` | OpenAI Chat Completions |
| `GET` | `/v1/models` | 模型列表，返回用户可见名和真实上游 ID |
| `GET` | `/api/status` | 账号池摘要和登录状态 |
| `GET` | `/api/config` | 当前 Base URL 与 API Key |

## 使用步骤

1. 启动 Cockpit2API。
2. 在 `WorkBuddy` 或 `WorkBuddy 国际版` 页面添加账号。
3. 在 `QoderWork 国际版` 页面添加账号。
4. 打开对应网关面板，复制 Base URL 和 API Key。
5. 将 OpenAI SDK 的 `baseURL` 指向对应网关，`apiKey` 设置为面板中的本地 Key。

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

## 安全边界

- 两个网关只应用于你本人拥有或已授权的账号。
- 不要把网关 API Key、账号数据、日志或运行时配置提交到仓库。
- 网关会绑定局域网地址，不要直接暴露到公网；跨机访问请使用防火墙、局域网隔离或反向代理保护。
