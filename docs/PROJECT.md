# Cockpit2API 项目概览

本仓库是 [Cockpit2API](https://github.com/wxy0626/Cockpit2API)，基于 [CockpitTools](https://github.com/jlcodes99/cockpit-tools) 制作，只维护本地新增能力和必要的构建说明。

## 项目范围

- 新增 `workbuddy2api`：WorkBuddy 账号池的 OpenAI 兼容网关。
- 新增 `qoder2api`：QoderWork 登录态的 OpenAI Chat Completions 网关。
- 仓库只包含构建项目所需的源码，不包含本地数据库、账号数据或运行时配置。

## 快速开始

```bash
npm install
npm run tauri:dev
```

Windows 本地发布构建：

```bash
npm run build:app
```

构建产物位于 `target/release/cockpit-tools.exe`。

## 测试

```bash
npm run typecheck
npm test
npm run test:rust:core
```

## 文档地图

- `README.md`：项目入口和本地亮点。
- `docs/LOCAL_GATEWAYS.md`：两个本地网关的端口、接口和安全说明。

## 许可

本项目采用 [CC BY-NC-SA 4.0](https://creativecommons.org/licenses/by-nc-sa/4.0/)，完整条款见 [LICENSE](LICENSE)。
