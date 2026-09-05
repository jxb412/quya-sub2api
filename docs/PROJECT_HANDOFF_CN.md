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
| 当前发布 | `v0.2.2`（合并上游 `v0.2.1`） |

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

安装完成后恢复为 `false`。如果上游更新包含数据库迁移，只在维护窗口临时打开
`DATABASE_INITIALIZATION_ENABLED=true`，完成验证后关闭。

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

`release.yml` 只在 `v*` 标签上发布稳定版本。CI 通过后创建例如 `v0.2.2`，
会构建二进制、GitHub Release 和：

```text
ghcr.io/jxb412/sub2api:0.2.2
ghcr.io/jxb412/sub2api:latest
```

Docker 服务器使用固定版本标签更容易回滚。内置更新检查适用于二进制/systemd
部署；Docker 部署仍需拉取镜像并重建应用容器。

`v0.2.2` 基于上游 `v0.2.1`，除本项目已有迁移外新增 4 个幂等迁移文件：

- `232_channel_cache_write_1h_pricing.sql`：为 4 张渠道定价表新增 `cache_write_1h_price`。
- `232_group_force_openai_fast.sql`：新增 `groups.force_openai_fast`。
- `232_group_reasoning_effort_over_limit.sql`：新增 `groups.max_reasoning_effort_over_limit`。
- `233_group_free_openai_fast.sql`：新增 `groups.free_openai_fast`。
- `232_add_usage_log_upstream_request_id.sql`：新增用量记录的上游请求标识。
- `233_add_usage_log_upstream_request_id_index_notx.sql`：创建对应非事务索引。
- `234_channel_max_reasoning_effort_multiplier.sql`：新增渠道 reasoning effort 倍率配置。
- `234_group_codex_models_manifest_config.sql`：新增 Codex 模型清单配置。

生产环境保持 `DATABASE_INITIALIZATION_ENABLED=false` 时，升级前应在备份后手工执行
上述迁移，并在 `schema_migrations` 中登记对应文件和校验值；不要为了迁移重启
PostgreSQL。

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
