# Quya Sub2API 项目交接与运维说明

本文档是 `jxb412/quya-sub2api` 的项目交接入口，只记录本项目的目录、自定义
改动、注意事项、更新方式和部署方式。其他代理项目、支付前端、检查副本和临时
会话内容不属于本项目，不在这里记录。

## 仓库信息

| 项目 | 地址或说明 |
|---|---|
| 本地目录 | `D:\btc\st\quya-sub2api` |
| 个人仓库 | `https://github.com/jxb412/quya-sub2api` |
| 上游仓库 | `https://github.com/Wei-Shaw/sub2api` |
| 默认分支 | `main` |
| 发布镜像 | `ghcr.io/jxb412/sub2api:<version>` |
| 当前开发版本 | `0.4.13`（合并上游 `v0.2.15`，待发布） |

`origin` 是个人仓库，`upstream` 是上游仓库。个人改动必须提交到个人仓库，
不能直接把上游分支覆盖到个人 `main`。

## 项目结构

```text
quya-sub2api/
├─ backend/       Go 网关、处理器、服务、数据访问和数据库迁移
├─ frontend/      Vue 3 管理后台和用户页面
├─ deploy/        Docker/systemd 部署、配置和环境模板
├─ docs/          功能、支付、API、运维和安全文档
├─ openspec/      项目设计提案和变更记录
├─ README_CN.md   中文项目入口
├─ DEV_GUIDE.md   开发与同步指南
└─ Makefile       常用命令
```

## 当前自定义功能

- OpenAI 请求在账号调度前按 Responses、Chat Completions、Messages 入口筛选。
- `openai_responses_only` 独立限制 Responses、compact 和 WebSocket 入口。
- `codex_cli_only` 只判断 Codex 客户端身份，不自动等同 Responses-only。
- 调度器、sticky 路由和 failover 会跳过不符合入口条件的账号。
- 风控中心支持按 `pro`、`plus`、`team`、`free` 账号类型单选或多选。
- 内置更新检查和回滚源改为 `jxb412/quya-sub2api`。
- 首次安装和数据库启动初始化默认关闭。

主要代码位置：

- OpenAI 调度：`backend/internal/service/openai_*`
- Codex 转换和身份：`backend/internal/service/openai_codex_*`
- 风控：`backend/internal/service/content_moderation.go`
- 后台风控页面：`frontend/src/views/admin/RiskControlView.vue`
- 更新服务：`backend/internal/service/update_service.go`
- 启动和安装：`backend/cmd/server/main.go`、`backend/internal/setup/`
- 数据库连接和迁移：`backend/internal/repository/ent.go`、`backend/migrations/`

## 默认安全开关

生产 Docker Compose 默认使用：

```dotenv
SETUP_ENABLED=false
AUTO_SETUP=false
DATABASE_INITIALIZATION_ENABLED=false
```

含义：

- 不启动首次安装向导。
- 不自动创建数据库、管理员和配置文件。
- 不在服务启动时执行迁移、JWT 密钥补写或简单模式默认数据写入。
- 业务运行仍会正常读写已有数据库。

真正新安装时，先备份环境文件，再同时设置：

```dotenv
SETUP_ENABLED=true
AUTO_SETUP=true
DATABASE_INITIALIZATION_ENABLED=true
```

安装完成后恢复为 `false`。

### 执行数据库迁移

`DATABASE_INITIALIZATION_ENABLED` 是唯一控制启动期迁移的开关：置为 `true` 时，服务
启动会对目标库执行 `backend/migrations/` 中所有未登记的迁移，并按
`sha256(TrimSpace(内容))` 登记到 `schema_migrations`；置为 `false` 时，启动日志会打印
`database initialization disabled; skipping startup migrations and bootstrap writes`
并跳过全部迁移。生产环境默认保持 `false`，需要执行迁移时用下面任一方式。

方式 A：手工执行 + 登记（本项目生产升级实际使用，可控性最好）

1. 备份 PostgreSQL 与 compose/env。
2. 在维护窗口执行本次新增的迁移 SQL（本项目迁移均为幂等语句）。
3. 按 `sha256(TrimSpace(内容))` 计算校验和，写入
   `schema_migrations(filename, checksum)`。
4. 检查所有连接同一数据库的应用节点。

方式 B：临时开启启动迁移（2026-10-09 已在测试机验证）

已验证结论：对**已安装**的库，把 `DATABASE_INITIALIZATION_ENABLED` 改成 `true` 并重建
容器，启动时会自动重放缺失的迁移、写入 `schema_migrations`（`applied_at` 为本次启动
时间），且不再打印 skip 日志。

注意事项：

- 生产 compose 在 `environment:` 中把该变量写死为 `false`，只改 `.env` 不会生效；
  必须改 compose 后再用 `docker compose up -d` 重建容器。
- 该开关同时放开启动期的 bootstrap 写入（JWT 密钥补写、SIMPLE 模式默认分组与管理员
  并发写入）。迁移完成后要改回 `false` 并重建容器。
- 对**空库**无效：缺少 `/app/data/.installed` 安装锁且 `SETUP_ENABLED=false` 时，进程
  会直接以 1 退出并打印
  `first-run setup is disabled; set SETUP_ENABLED=true for an explicit installation`。
- 多节点共用同一数据库时，迁移由 PostgreSQL advisory lock 串行化；即便如此也要在所有
  节点上同时改回 `false`，避免节点间行为不一致。

## 部署

生产推荐使用 `deploy/docker-compose.local.yml`，数据保存在部署目录下：

```bash
cp deploy/.env.example deploy/.env
# 编辑 deploy/.env，设置数据库密码、JWT_SECRET、TOTP_ENCRYPTION_KEY
cd deploy
docker compose -f docker-compose.local.yml up -d
docker compose -f docker-compose.local.yml ps
docker compose -f docker-compose.local.yml logs --tail=100 sub2api
```

更新镜像前先确认持久化挂载：

```bash
docker inspect sub2api --format '{{range .Mounts}}{{println .Source "->" .Destination}}{{end}}'
```

不要执行 `docker compose down -v`。只更新应用容器：

```bash
docker pull ghcr.io/jxb412/sub2api:<version>
docker compose -f docker-compose.local.yml up -d --no-deps sub2api
```

拉取镜像不会中断业务；替换应用容器可能造成短暂中断，进行中的流式请求可能
断开。服务器操作系统不需要重启。

## 发布与更新

`release.yml` 只在 `v*` 标签上发布稳定版本。CI 通过后创建例如 `v0.4.6`，
会构建二进制、GitHub Release 和：

```text
ghcr.io/jxb412/sub2api:0.4.6
ghcr.io/jxb412/sub2api:latest
```

Docker 服务器使用固定版本标签更容易回滚。内置更新检查适用于二进制/systemd
部署；Docker 部署仍需拉取镜像并重建应用容器。

`0.4.6` 基于本项目 `0.4.5`，合并上游 `v0.2.5`。上游主要变化包括 OpenCode
平台、站点类型开关、订阅/API Key 批量管理、Codex 配额窗口修复、Responses Lite
namespace 修复、OpenAI WebSocket 连接池与执行作用域修复，以及用量费用精度改进。

本次新增两个幂等迁移文件：

- `238_opencode_go_platform.sql`：在现有平台约束中增加 `opencode_go`。
- `238_purge_unlimited_user_platform_quotas.sql`：删除三档额度均为 `NULL` 的无效平台额度行。

生产环境保持 `DATABASE_INITIALIZATION_ENABLED=false` 时，应用不会自行执行这两项
迁移。升级前必须先备份 PostgreSQL，在维护步骤中执行并登记迁移（执行方式见上文
「执行数据库迁移」），再同时替换所有连接同一数据库的应用节点；不需要重启
PostgreSQL。

本次在隔离环境完成了从 `0.4.5` 数据库原地升级到 `0.4.6` 的验证：两项迁移均已
登记，四个相关平台约束包含 `opencode_go`，原管理员数据和登录状态保持正常。

`0.4.13` 基于本项目 `0.4.12`，合并上游 `v0.2.15`（146 个提交）。上游主要变化：新增
Cline、Command Code 平台，平台白名单由数据库 CHECK 约束改为应用层校验；平台清单
统一由 platform profile 驱动转发、探测与账号表单；修复加密 reasoning 签名被拒、
Anthropic thinking 缺少 signature、Chat 与 Responses 互转、WebSocket 长连接计费、
Grok 空流 failover 等问题。

OAuth 历史回放 `web_search_call` 的修复改用官方实现（`dab3b87ea`、`64caa9af8`、
`85e311095`，含 Responses Lite 的 `additional_tools` 变体），不再保留本地早期移植
版本，相关文件与上游逐字节一致。

本次新增一个幂等迁移：

- `242_drop_platform_check_constraints.sql`：删除 `user_platform_quotas_platform_check`
  与 `composite_model_routes_target_platform_check` 两个 CHECK 约束，平台白名单改由
  应用层校验，新增平台不再需要数据库迁移。

生产环境保持 `DATABASE_INITIALIZATION_ENABLED=false` 时，应用不会自行执行该迁移。
升级前先备份 PostgreSQL，在维护步骤中执行并登记迁移（执行方式见上文
「执行数据库迁移」），再同时替换所有连接同一数据库的应用节点；旧约束未删除前，
写入新平台值会被数据库拒绝。

本次合并后在测试机隔离目录完成了源码级验证：`go build ./...`、`go test -tags=unit`
（58 个包）与 `go test -tags=integration`（需要 testcontainers 的
`postgres:18.1-alpine3.23`、`redis:8.4-alpine` 镜像）全部通过；前端
`pnpm install --frozen-lockfile`、`vue-tsc`、eslint 与关键 vitest（31 个文件、
579 个用例）全部通过。

## 上游同步

```powershell
git status --short
git fetch origin
git fetch upstream
git switch -c sync/upstream-YYYY-MM-DD origin/main
git merge --no-ff upstream/main
```

冲突时保留个人自定义逻辑并逐项测试。重点检查启动、更新源、OpenAI 调度、风控、
部署 Compose 和迁移文件。不要使用 `git reset --hard` 或 `git checkout --` 清理
工作区。

## 验证命令

```bash
cd backend
go test -tags=unit ./...
go test -tags=integration ./...
golangci-lint run ./...

cd ../frontend
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test:run
pnpm build
```

本地缺少 Go、pnpm、PostgreSQL 或 Redis 时，应明确记录检查未执行。

## 线上操作底线

- 不要未经确认重启服务器或生产服务。
- 不要删除或更换 PostgreSQL、Redis、`/app/data` 数据卷。
- 不要把生产密钥、OAuth token、API 秘钥、代理密码或完整 attestation 写入 Git 和日志。
- 应用、数据库、Redis、配置和镜像更新前都要保留可恢复备份。
- 已应用 migration 不可修改；新增结构必须创建新的递增 migration。

## 相关文档

- [中文项目入口](../README_CN.md)
- [开发指南](../DEV_GUIDE.md)
- [部署说明](../deploy/README.md)
- [Docker 说明](../deploy/DOCKER.md)
- [边缘安全](../deploy/EDGE_SECURITY.md)
- [配置模板](../deploy/config.example.yaml)
- [支付说明](PAYMENT_CN.md)
- [组合分组](COMPOSITE_GROUPS.md)
- [异步图片任务](ASYNC_IMAGE_TASKS.md)
