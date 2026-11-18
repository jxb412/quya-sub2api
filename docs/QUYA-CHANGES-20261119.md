# Quya ctx_pool 完善记录 — 2026-11-19

## 目的与部署范围

解决 HTTP/SSE 客户端通过账号 `ctx_pool` 转发到上游 WS 时的连接复用、
长对话、搜索工具与续聊恢复问题。关键定制代码使用
`2026-11-19` 注释说明原因及功能，后续合并官方时应结合本文件审查。
本轮部署仅限 `154.217.250.100:18081` 测试环境，不更新正式服务器。

## 原因、功能与对应代码

| 改动 | 原因 | 功能与边界 | 主要文件（backend/internal/service） |
| --- | --- | --- | --- |
| 同会话连接复用 | 每轮变化的 turn metadata 被当作连接身份，导致多余握手 | 每轮 metadata 放入 response.create；硬身份继续隔离；routing hint 沿用官方软亲和规则 | openai_http_ws_transport.go |
| 完整上下文自动恢复 | HTTP 桥接原来只保存 response→connection，不保存可用于恢复的完整历史 | 成功轮次保存已证明完整的 input/output；原连接失效时，在新生成发送前重建完整 input 并重连；HTTP 兜底使用同一完整历史 | openai_http_ws_recovery.go、openai_http_ws_replay_store.go、openai_plugin_transport.go |
| OAuth 续聊 ID 保留 | 传统 HTTP 归一化会提前删除 previous_response_id，HTTP→WS 因而拿不到续聊关系 | ctx_pool 路径保留该字段；传统 HTTP 路径维持原规则 | openai_gateway_forward.go |
| HTTP 兜底后的下一轮续聊 | 仅缓存 WS 成功响应，会导致 HTTP 子轮成功后下一轮找不到完整父历史 | 复用既有响应终态解析点保存 HTTP 兜底成功轮次；失败、取消及缺少完整 output 的响应不入库 | openai_http_ws_fallback_history.go、既有 Responses/透传/Chat/Messages 解析点 |
| 工具调用 ID 对齐 | OAuth 当前输入已规范化调用 ID，原始缓存历史可能仍使用旧前缀 | 仅在官方 ID 规范化结果与当前工具输出精确对应时对齐父调用；API Key 的原始 ID 不改 | openai_http_ws_recovery.go |
| 搜索能力切换 | 官方首轮能力记录未识别桥接使用的 RawMessage map | 补充记录；中途开启 web_search 时按完整历史恢复到新连接 | openai_ws_search_activation.go、openai_http_ws_transport.go |
| 大请求兼容 | 事件读取上限曾被误用于 HTTP 请求；直接取消限制又可能超过供应商 WS 消息容量 | 合法大请求不再本地直接报错；仅 ChatGPT Codex OAuth 最终 WS 编码超过 16 MiB 时，在发送前用 HTTP；计算包含 HTML 转义、metadata 与末尾换行 | openai_http_ws_payload_limit.go、openai_http_ws_policy.go |
| 复用与握手诊断 | 无法区分新建/复用，以及握手 403 与本地等待失败 | 日志增加 reused 和固定握手分类；不输出原始响应正文、提示词、认证头或会话值 | openai_ws_handshake_diagnostics.go、openai_http_ws_policy.go |

## 与官方和 King 的关系

官方当前 `2af3f1e15c98c893a89255a037f551046883b8c0` 的 WS ingress、
连接池及搜索能力逻辑已逐文件核对。本轮保留官方原生 WS 流程，
在 Quya 的 HTTP→WS 桥接层复用其完整输入构造、工具调用上下文检查规则。
“能证明历史完整则自动恢复，不能证明就不猜测/删除前文”是恢复前提。

King 参考目录为用户提供的 2026-11 微信文件目录。其源码声称 OAuth
实测超过 16 MiB 会收到 close 1009，但随包没有原始测试记录；因此本轮将
16 MiB 作为该端点的保守兼容预算，不能宣称已独立测出供应商上限。
其他 API Key 上游不因此被限制。未整体替换官方连接池，也未引入 King
会改变用户既定重试次数的冷却策略。

## 恢复、隔离与计费边界

- 正常建连仍为首次尝试加三次重试，每次最多五秒，然后 HTTP 兜底。
- 恢复只发生在本次 response.create 尚未尝试写出时；写后结果不明，
  不自动再生成，避免重复执行或重复扣费。
- 历史按账号、认证作用域、上游地址及稳定身份隔离；不同客户或凭据不能读取。
- 只有完整且成功的响应可推进可恢复历史；失败、缺失父历史或不完整输出
  不能被标成完整历史。工具调用结果必须有对应的调用上下文。
- 缓存只在进程内，最多 512 条、合计 64 MiB、每条 16 MiB、最长 15 分钟；
  重启、过期或淘汰后可能无法恢复旧的增量续聊。完整上下文请求照常转发。
- 用量和费用继续由既有解析与计费流程处理；重建上下文本身不生成第二条请求。
- 本轮没有数据库迁移、表结构或定价变更。

## 验证要求

回归覆盖动态 metadata 复用、凭据/客户隔离、原连接断开后的完整历史恢复、
工具调用配对、搜索能力开启、恢复后 HTTP 兜底、长输入及转义后的真实消息长度、
无完整历史时的处理、已写出后的禁止重放，以及旧模型和原生 WS 兼容。
部署后通过测试环境的健康检查、版本检查和有额度限制的实际调用核对。
实际运行结果与镜像 digest 记录在本轮部署报告中。
