# Quya Sub2API 开发指南

本文档只记录本项目的开发、测试、同步和安全注意事项，不记录其他仓库、个人
服务器密码或临时排障数据。

## 项目与工具链

| 项目 | 值 |
|---|---|
| 个人仓库 | `https://github.com/jxb412/quya-sub2api` |
| 上游仓库 | `https://github.com/Wei-Shaw/sub2api` |
| 后端 | Go 1.26.6、Gin、Ent |
| 前端 | Vue 3、TypeScript、pnpm |
| 数据库 | PostgreSQL、Redis |
| 本地项目目录 | `D:\btc\st\quya-sub2api` |

不要把生产凭据、API 秘钥、OAuth token、数据库密码或代理密码写入仓库。

## 目录边界

- `backend/`：网关服务、协议处理、账号调度、计费、数据访问和迁移。
- `frontend/`：管理后台和用户页面。
- `deploy/`：Docker/systemd 部署文件、环境模板和配置模板。
- `docs/`：功能、支付、API、运维和安全文档。
- `openspec/`：项目设计提案和变更记录。

Ent 生成代码位于 `backend/ent/`，修改数据模型时编辑
`backend/ent/schema/`，然后运行代码生成，不要手工修改生成文件。

## 本地检查

```powershell
cd D:\btc\st\quya-sub2api\backend
go test -tags=unit ./...
go test -tags=integration ./...
golangci-lint run ./...

cd ..\frontend
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test:run
pnpm build
```

CI 使用 `.github/workflows/backend-ci.yml`、`security-scan.yml` 和
`release.yml`。本地缺少 Go、pnpm 或数据库时，应明确标记对应检查未执行。

## 自定义功能边界

修改 OpenAI/Codex 调度时重点检查：

- `backend/internal/service/openai_inbound_routing.go`
- `backend/internal/service/openai_account_scheduler.go`
- `backend/internal/service/openai_gateway_scheduling.go`
- `backend/internal/service/openai_gateway_forward.go`
- `backend/internal/service/openai_client_restriction_detector.go`

修改风控账号类型筛选时重点检查：

- `backend/internal/service/content_moderation.go`
- `backend/internal/handler/admin/content_moderation_handler.go`
- `frontend/src/views/admin/RiskControlView.vue`

修改更新源时检查 `backend/internal/service/update_service.go`。当前发布源是
`jxb412/quya-sub2api`，不要误改回上游仓库。

## 安装和数据库安全

生产部署默认关闭：

```dotenv
SETUP_ENABLED=false
AUTO_SETUP=false
DATABASE_INITIALIZATION_ENABLED=false
```

这会关闭首次安装、自动安装、启动迁移和启动阶段的数据库引导写入，但不会阻止
正常业务访问数据库。

只有新安装或确认需要执行迁移时才显式开启。新安装需要三个变量同时为 `true`；
普通升级通常只临时开启 `DATABASE_INITIALIZATION_ENABLED=true`，完成后恢复为
`false`。已应用迁移文件不可修改，必须创建新的递增迁移。

## 上游同步流程

个人改动必须先提交，工作区保持干净后再同步：

```powershell
git status --short
git fetch origin
git fetch upstream
git switch -c sync/upstream-YYYY-MM-DD origin/main
git merge --no-ff upstream/main
```

解决冲突后运行后端和前端检查，提交同步分支并创建 PR，再合并到个人 `main`。
不要使用 `git reset --hard upstream/main` 或直接覆盖个人分支。

## 发布流程

1. 合并代码并等待 CI、安全扫描通过。
2. 创建稳定版本标签，例如 `v0.1.181`。
3. `release.yml` 构建 GitHub Release、二进制和 GHCR 镜像。
4. Docker 使用 `ghcr.io/jxb412/sub2api:0.1.181` 更新应用容器。
5. 更新前备份数据库和配置，更新后检查 `/health`、日志和关键 API。

Docker 镜像更新不需要服务器操作系统重启，但替换单个应用容器可能造成短暂中断。

## 操作底线

- 不要未经确认连接或修改生产服务器。
- 不要删除 PostgreSQL/Redis 数据卷。
- 不要把生产配置提交到 Git。
- 不要把真实 attestation、OAuth token 或完整上游凭据写入日志。
- 发现数据库迁移、认证、计费或账号调度冲突时，先停在同步分支处理。
