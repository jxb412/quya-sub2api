# Quya 0.4.28-quya.1：合并官方 v0.2.24

日期：2026-11-20。基线为用户指定恢复的 `d8c54af29ac5271e7bfcb99b2d48df2396ff1896`（`0.4.27-quya.1-ctxpool.d8c54af29ac5`）。

官方发布：https://github.com/Wei-Shaw/sub2api/releases/tag/v0.2.24

合并官方提交：`5c2982a81b46e2142d0143d2f83c1ce9c4da1d24`。

## GPT-6.2 缓存处理

官方 v0.2.24 尚未修复 https://github.com/Wei-Shaw/sub2api/issues/8930 。
与 v0.2.23 比较，官方的 `openai_codex_transform.go`、
`openai_codex_instructions_template.go` 和 `openai_gateway_forward.go` 没有变化；
普通 Codex 转换仍将默认指令放入顶层 `instructions`。

因此本次保留 Quya 的 GPT-6.2 Sol/Astra 稳定指令前缀补丁：默认稳定提示词和
适用的模板放在 `input` 的 developer 消息内，客户显式顶层指令仍保持原样，
续聊不重复添加默认前缀。旧模型和 compact 的处理不变。是否实际命中缓存以
上游返回 usage 为准，不保证命中率。

## 本次引入的官方功能及修复

- HTTP 请求及原生 WS 帧拒绝重复 JSON 字段，阻止计费模型与上游实际模型分叉；
  修复 BOM、multipart 等情况下的模型白名单绕过。
- Codex 文献搜索普通 403 不再造成 OAuth 账号逐个停调；明确账号失效仍处理。
- HTTP 408 切换账号、OAuth 凭据轮换保存、OAuth 模型调度过滤修复。
- Antigravity 裸 429 的冷却范围限制为当前请求；该修复不等于解决所有 OpenAI 429。
- Messages 的原生 tool_search、搜索来源输出和缓冲 Responses 输出补全。
- Exa 搜索、Claude 压缩请求缓存 TTL、用量刷新和请求解析性能改进。

## 自定义行为保留

- 账号显式选择 ctx_pool 时 HTTP/SSE 使用上游 WS，首次连接加三次重试，
  每次连接五秒，发送生成前可回退 HTTP；尝试发送后禁止自动重放。
- 调度快照保留 `openai_inbound_capabilities`、`openai_responses_only`、
  `codex_cli_only`、`codex_cli_only_allow_app_server`。
- 现有 Gemini/GPT 定价、自定义账号限制和其他基线修改保留。
- 未带入基线之后的 `7529fc190f12` 等上下文恢复改动，也未加入共享 Redis 启动清理修复。

## 构建与部署边界

本次没有新增或修改数据库 migrations、Ent schema。测试镜像工作流从
`backend/cmd/server/VERSION` 读取 `0.4.28-quya.1`，添加提交对应的 ctxpool 后缀，
避免镜像沿用旧版本标签。

只部署测试服务 `154.217.250.100:18081`，沿用独立数据库
`sub2api_ctxpool_test` 与 Redis DB2；不得因此更新正式服务器或切换 Redis。
测试部署前确认实际运行基线及备份原 Compose，失败时使用原镜像恢复。

回归重点：两个 GPT-6.2 模型的最终转发内容、旧模型兼容、模板/续聊边界、
ctx_pool 重试及发送后禁止重放、调度自定义限制、重复字段拒绝及官方新增协议转换。
实际执行结果及镜像 digest 记录在本轮部署报告中，不以源码测试代替真实上游缓存验收。
