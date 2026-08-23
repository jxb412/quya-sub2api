# Sub2API 项目交接索引（中文）

> 这份文档用于新的 Codex/Claude 对话快速接手项目。它记录本地路径、远程仓库、重要目录、当前改动和下一步注意事项。
>
> 最后整理：2026-08-23（Asia/Shanghai）

## 1. 新对话直接使用的上下文

```text
当前项目：D:\btc\st\quya-sub2api
工作区：D:\btc\st
当前分支：main
origin：https://github.com/jxb412/quya-sub2api.git
upstream：https://github.com/Wei-Shaw/sub2api.git

这是基于 Wei-Shaw/sub2api 的个人维护版本。不要执行 git reset --hard、git checkout --、删除数据目录或随意重启线上服务。
当前工作区存在未提交的用户改动；处理任务前先执行 git status 和 git diff，保留这些改动。
```

## 2. 仓库与本地地址

| 项目 | 地址 |
|---|---|
| 本地项目根目录 | `D:\btc\st\quya-sub2api` |
| 工作区根目录 | `D:\btc\st` |
| 个人远程仓库（origin） | <https://github.com/jxb412/quya-sub2api> |
| 官方上游仓库（upstream） | <https://github.com/Wei-Shaw/sub2api> |
| 当前分支 | `main` |
| 当前基线提交 | `f5b7c1c`（`chore: initialize from upstream Sub2API main`） |

父目录中的相关项目（不是当前 Sub2API 源码）:

- `D:\btc\st\ipv6-lease-proxy`：IPv6 租约代理项目。
- `D:\btc\st\bd`：支付前端/接口补丁和测试资料。
- `D:\btc\st\sub2api_latest_inspect_20260815`：上游版本检查副本。
- `D:\btc\st\sub2api_gpt56_inspect_20260816`：GPT-5.6 版本检查副本。

## 3. 项目结构速览

```text
quya-sub2api/
├─ backend/
│  ├─ cmd/server/              Go 服务入口（main.go、wire_gen.go）
│  ├─ internal/handler/        HTTP/API 路由和请求处理
│  ├─ internal/service/        核心业务、调度、转发、计费、账号逻辑
│  ├─ internal/repository/     数据访问层
│  ├─ internal/pkg/            OpenAI/Anthropic/Gemini 等协议工具包
│  ├─ ent/schema/              Ent 数据模型定义
│  ├─ ent/                     Ent 生成代码（不要手工改生成文件）
│  ├─ migrations/              PostgreSQL 正向迁移
│  ├─ resources/model-pricing/ 模型价格和上下文窗口数据
│  └─ go.mod                   Go 版本和后端依赖
├─ frontend/
│  ├─ src/api/                 前端 API 封装
│  ├─ src/components/          Vue 组件
│  ├─ src/views/               页面视图
│  ├─ src/stores/              Pinia 状态
│  ├─ src/router/              路由
│  ├─ src/i18n/                中英文国际化
│  ├─ package.json             前端脚本和依赖
│  └─ pnpm-lock.yaml           前端锁文件（使用 pnpm）
├─ deploy/
│  ├─ docker-compose*.yml      Docker 部署组合
│  ├─ Dockerfile                镜像构建
│  ├─ config.example.yaml      配置模板
│  ├─ .env.example              环境变量模板
│  ├─ README.md                 部署、升级、迁移说明
│  ├─ EDGE_SECURITY.md          CDN/反向代理可信边界
│  └─ docker-entrypoint.sh      容器入口
├─ docs/                       功能文档；本文件是交接入口
├─ openspec/                   设计提案、规格和变更冻结资料
├─ skills/                     项目专用技能和管理员 CLI 说明
├─ DEV_GUIDE.md                本地开发、测试和常见坑
├─ README_CN.md                中文项目总说明
└─ Makefile                    常用构建、测试、代码生成命令
```

## 4. 首先阅读的文档

1. [README_CN.md](../README_CN.md)：项目功能和基础部署说明。
2. [DEV_GUIDE.md](../DEV_GUIDE.md)：本地环境、Go/pnpm、测试命令和 Windows 常见问题。
3. [deploy/README.md](../deploy/README.md)：Docker/二进制部署、升级、迁移和故障排查。
4. [deploy/DOCKER.md](../deploy/DOCKER.md)：Docker 镜像和 Compose 快速说明。
5. [deploy/config.example.yaml](../deploy/config.example.yaml)：配置键及默认值。
6. [backend/migrations/README.md](../backend/migrations/README.md)：迁移不可修改原则和执行规则。
7. [openspec/](../openspec/)：已经设计或冻结的功能变更，修改相关功能前先检查是否有对应 proposal。

支付和会员功能相关文档：

- `docs/PAYMENT_CN.md`
- `docs/PAYMENT.md`
- `docs/ADMIN_PAYMENT_INTEGRATION_API.md`
- `docs/COMPOSITE_GROUPS.md`

## 5. Codex/OpenAI 相关代码索引

| 功能 | 主要文件 |
|---|---|
| Responses 主转发 | `backend/internal/service/openai_gateway_forward.go` |
| 透传路径 | `backend/internal/service/openai_gateway_passthrough.go` |
| Chat Completions | `backend/internal/service/openai_gateway_chat_completions.go`、`openai_gateway_chat_completions_raw.go` |
| Anthropic `/v1/messages` 桥接 | `backend/internal/service/openai_gateway_messages.go`、`openai_messages_bridge.go` |
| 账号调度 | `backend/internal/service/openai_account_scheduler.go`、`openai_gateway_scheduling.go` |
| Codex 客户端识别/限制 | `openai_client_restriction_detector.go`、`backend/internal/pkg/openai/request.go` |
| Codex UA/originator | `openai_codex_identity.go`、`openai_gateway_service.go` |
| 指纹收敛 | `openai_codex_fingerprint.go`、`openai_codex_fingerprint_test.go` |
| 请求体转换 | `openai_codex_transform.go` |
| Responses WebSocket | `openai_ws_forwarder*.go`、`openai_ws_forwarder_payload.go` |
| Live DeviceCheck | `openai_live.go`、`openai_live_attestation.go`、`backend/internal/platform/liveattestation/` |
| 入口协议筛选（本轮新增） | `openai_inbound_routing.go`、`openai_inbound_routing_test.go` |

### 当前已实现的入口限制

- `codex_cli_only`：仅判断客户端是否属于 Codex 家族，不自动等同于 Responses-only。
- `openai_responses_only`：独立限制 `/v1/responses`、compact 和 WebSocket，排除 `/v1/chat/completions`、`/v1/messages`。
- 两个开关同时开启，才表示“官方 Codex 客户端 + Responses 专用账号”。
- 旧版和高级调度器、HTTP/WS/消息桥接均有二次限制和 failover 处理。

### 当前设备证明状态

- Codex PR #20619 的 `attestation/generate` / `x-oai-attestation` 目前尚未在 Sub2API 中实现。
- 当前同名头只用于 Live DeviceCheck，不等于 Desktop attestation。
- 普通 HTTP、passthrough、WS 的 OpenAI 请求头白名单目前没有 Desktop attestation 透传。
- 不要伪造、缓存、跨账号复用或记录完整 attestation 值。
- 后续若实现，应只对 ChatGPT OAuth/Codex 上游原样透传真实客户端提供的值，并在 failover 换账号时清除。

## 6. 当前工作区未提交改动

### 新增文件

- `backend/internal/service/openai_inbound_routing.go`
- `backend/internal/service/openai_inbound_routing_test.go`

### 后端修改

- `backend/internal/handler/admin/content_moderation_handler.go`
- `backend/internal/handler/content_moderation_helper.go`
- `backend/internal/handler/gateway_handler.go`
- `backend/internal/handler/openai_chat_completions.go`
- `backend/internal/handler/openai_gateway_count_tokens.go`
- `backend/internal/handler/openai_gateway_handler.go`
- `backend/internal/handler/openai_images.go`
- `backend/internal/handler/security_audit_helper.go`
- `backend/internal/securityaudit/coordinator_legacy.go`
- `backend/internal/securityaudit/prompt_types.go`
- `backend/internal/service/account.go`
- `backend/internal/service/content_moderation.go`
- `backend/internal/service/openai_account_scheduler.go`
- `backend/internal/service/openai_client_restriction_detector.go`
- `backend/internal/service/openai_gateway_chat_completions.go`
- `backend/internal/service/openai_gateway_forward.go`
- `backend/internal/service/openai_gateway_messages.go`
- `backend/internal/service/openai_gateway_scheduling.go`

### 前端修改

- `frontend/src/api/admin/riskControl.ts`
- `frontend/src/components/account/BulkEditAccountModal.vue`
- `frontend/src/components/account/CreateAccountModal.vue`
- `frontend/src/components/account/EditAccountModal.vue`
- `frontend/src/i18n/locales/en/admin/accounts.ts`
- `frontend/src/i18n/locales/en/admin/channels.ts`
- `frontend/src/i18n/locales/zh/admin/accounts.ts`
- `frontend/src/i18n/locales/zh/admin/channels.ts`
- `frontend/src/views/admin/RiskControlView.vue`

接手前必须先查看：

```powershell
cd D:\btc\st\quya-sub2api
git status --short
git diff --stat
git diff -- backend/internal/service/openai_gateway_forward.go
```

不要把这些改动当成可以丢弃的临时文件，也不要用 `git reset --hard` 或 `git checkout --` 清理。

## 7. 本地环境和验证命令

技术栈：Go 1.26.6、Gin、Ent、PostgreSQL 16、Redis、Vue 3、TypeScript、pnpm。

后端：

```powershell
cd D:\btc\st\quya-sub2api\backend
go test -tags=unit ./...
go test -tags=integration ./...
go run ./cmd/server/
go generate ./ent
golangci-lint run ./...
```

前端（必须使用 pnpm，不要用 npm 替代锁文件）：

```powershell
cd D:\btc\st\quya-sub2api\frontend
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test:run
pnpm build
```

当前这次检查环境曾缺少 Go 和 `vue-tsc`，因此不能把完整后端/前端测试描述为已通过。修改后应在具备依赖的环境重新验证。

## 8. 数据库、部署和线上安全

- 数据库迁移目录：`backend/migrations/`。
- 已应用的迁移文件不可修改；需要新文件递增迁移。
- 生产配置不在仓库中，使用部署目录的模板生成实际配置；不要把生产密钥写入本文件或 Git。
- Docker 入口：`deploy/docker-compose.yml`、`deploy/docker-entrypoint.sh`。
- 部署和升级前先备份 PostgreSQL、Redis（如使用）以及配置文件。
- 用户明确要求不要随便重启服务器；任何线上部署、重启、数据库写入都要单独确认。
- CDN/反代可信 IP 设置先阅读 `deploy/EDGE_SECURITY.md`，不要直接信任客户端提交的转发头。

## 9. 新对话建议开场模板

```text
请接手 D:\btc\st\quya-sub2api 项目。
先阅读 docs/PROJECT_HANDOFF_CN.md、README_CN.md、DEV_GUIDE.md 和 deploy/README.md。
当前是 jxb412/quya-sub2api 的 main 分支，origin 是个人仓库，upstream 是 Wei-Shaw/sub2api。
先执行 git status 和 git diff，不要丢弃未提交改动，不要重启或部署线上服务。
本次任务范围是：<在这里填写新任务>。
```
