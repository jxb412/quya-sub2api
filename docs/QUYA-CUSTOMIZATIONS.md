# Quya 自定义功能维护说明

更新日期：2026-11-28。本文供代码维护和上游合并使用，不是客户使用文档，也不表示所述候选代码已经部署。

当前历史包含官方 `v0.2.25` 合并提交 `48394859c`，随后保留 Quya 的订阅重置、分组零新增提示词等功能。本次编写时的基线为 `eca3e12f4`，其上新增账号禁用同步调用及非流式响应恢复。后续发布应以实际提交、镜像摘要和验证记录确定版本。

官方仓库：https://github.com/Wei-Shaw/sub2api

本项目仓库：https://github.com/jxb412/quya-sub2api

## 维护原则

- 官方已完整覆盖的模型、价格和协议修复以官方实现为主；Quya 的业务开关、调度限制、订阅规则及其他定制继续保留。
- 判断是否被官方替代，要比较行为和测试，不能仅凭相似函数名删除本地逻辑。
- 账号限制必须贯穿调度快照、候选过滤、粘性会话、批量调度和最终转发；仅保留后台开关并不足够。
- 客户请求模式、上游传输方式和计费来源分别记录。客户端非流式不等于上游也必须非流式。
- 本文只包含仓库内代码信息，不保存运行环境的凭据、账号名单或连接配置。

## 2026-11-28：账号禁用同步调用

目的：让不适合承接同步请求的 OpenAI 账号只接收允许的请求，同步请求由其他合格账号承接。

后台新增、编辑和批量编辑账号时，提供“禁用同步调用”，位置邻近“仅允许 Responses 入口”。支持 OpenAI OAuth、SetupToken 和 API Key 账号。配置保存于已有 JSON 字段 `accounts.extra.openai_disable_sync`，默认关闭；没有该字段的旧账号维持原行为。

行为边界：

- Responses、Chat Completions、Messages 按客户端原始 `stream` 判断。`stream=false` 或省略时属于同步调用；开启限制的账号退出候选池。
- 后续把请求转换为上游 SSE 或 WS，不会把原来的同步请求变成可使用受限账号的请求。
- 原生 WS 不属于 HTTP 同步请求。旧式 compact 同步调用受限制；原生流式 compact 按其客户端模式判断。
- token 计数辅助请求、独立图片接口和独立视频接口不在本次限制范围内；通过 Responses 发起的图片生成遵循该 Responses 请求的模式。
- 没有符合条件的账号时返回现有无可用账号结果，不绕过设置。`previous_response_id` 等已有绑定约束继续生效，不能为了换号破坏续聊关系。
- 新建和单个编辑可以从完整 `extra` 对象删除关闭的标记；批量编辑采用增量合并，关闭时必须显式写入 `false`。

主要代码：

- `backend/internal/service/openai_inbound_routing.go`
- `backend/internal/service/openai_account_scheduler.go`
- `backend/internal/service/openai_gateway_scheduling.go`
- `backend/internal/service/openai_http_ws_policy.go`
- `backend/internal/handler/openai_gateway_handler.go`
- `backend/internal/handler/openai_chat_completions.go`
- `backend/internal/handler/openai_gateway_count_tokens.go`
- `backend/internal/repository/scheduler_cache.go`
- `frontend/src/components/account/CreateAccountModal.vue`
- `frontend/src/components/account/EditAccountModal.vue`
- `frontend/src/components/account/BulkEditAccountModal.vue`

合并关注点：调度缓存中的 `openai_disable_sync` 必须保留；重构转换顺序时不能丢失客户端原始模式；批量关闭必须有覆盖旧值的语义。新增开关与 `openai_responses_only`、`codex_cli_only` 相互独立，可叠加使用。

## 2026-11-28：非流式请求的 SSE 中断恢复与计费

目的：上游已经输出有效内容，但连接在完整终态到达前结束时，尽量保留已收到的结果，并让计费沿用现有流程。普通 Responses 转发和透传路径均处理这种情况。

这里处理的是“客户端等待一个完整 JSON，而上游返回 SSE”的已有兼容链路。本次代码未增加把所有 API Key 非流式请求统一强制改成 SSE 的新开关。调用上游 SSE 后再聚合为 JSON，会增加网关聚合开销，客户端仍需等待整次请求结束；它不能自动消除客户端或代理的总超时。

恢复规则：

- 没有有效终态但存在可用输出时，聚合为标准 Responses 对象，HTTP 状态为 `200`，响应 `status` 为 `incomplete`，不伪造 `completed`。
- 按输出索引和条目 ID 合并完成条目与未结束的文本；保留开始事件中的文本前缀和后续增量，完整 done 文本优先，避免重复拼接。
- 保留合法完成条目的扩展字段及已有加密字段。未完成的函数参数不包装成可执行的完整调用；函数调用需要完整标识、名称和合法参数 JSON 等条件。
- 只有 created、心跳、`[DONE]`，或只有不透明 reasoning/compaction 数据时，不返回空白成功结果，继续按现有错误路径处理。
- 本地补出的响应 ID 不冒充上游响应 ID，不用它建立上游续聊绑定。
- 生成可能已经发生时，不自动重发请求，以免重复生成和重复扣费。
- 已经进入 compact SSE 桥接的客户端收到 `response.incomplete`，不补造 `response.completed`。

计费规则：

- 优先使用上游实际报告的 token。显式数值 `0` 有效，空 `usage: {}` 不等于报告了零用量。
- 可恢复输出但缺少上游用量时，根据实际发送的请求体和已收到的语义输出，本地估算输入、输出 token；不虚构缓存命中、不可见 reasoning 或图片 token。
- 实际或估算用量均进入已有模型价格、分组/渠道价格、余额和订阅扣费流程，不引入第二套账本或收费公式。
- 管理员 Ops 诊断记录 `usage_source` 为 `upstream`、`local_estimate` 或 `unavailable`，并记录是否恢复输出和数值用量。此条诊断不保存提示词或输出正文。
- 客户响应保留标准 `usage`，不注入私有估算说明字段或提示文本。客户端仍应检查标准 `status` 判断结果完整性。
- 本地估算仅覆盖能够观察和编码的内容，不能证明供应商实际扣费；供应商对账仍需供应商侧记录。

主要代码：

- `backend/internal/service/openai_nonstream_recovery.go`
- `backend/internal/service/openai_nonstream_recovery_output.go`
- `backend/internal/service/openai_nonstream_usage_estimate.go`
- `backend/internal/service/openai_gateway_response_handling.go`
- `backend/internal/service/openai_gateway_passthrough.go`
- `backend/internal/service/openai_gateway_forward.go`

合并关注点：普通和透传路径必须一起维护；正常终态、明确失败终态、中断无输出、中断有输出应分别验证；计费不能遗漏或重复。SSE 识别依据真实物理行框架，不能把 JSON 字符串中的 `data:` 当作 SSE。

## 已保留的主要定制

| 功能 | 保留原因与实际行为 | 主要源码 | 合并注意事项 |
| --- | --- | --- | --- |
| 账号入口和客户端限制 | `openai_responses_only` 限制公共入口；`codex_cli_only` 限制客户端身份；二者含义不同 | `backend/internal/service/openai_inbound_routing.go`；`backend/internal/repository/scheduler_cache.go` | 保留快照字段和转发前检查，不能把 API Key 账号排除在策略之外 |
| 显式 `ctx_pool` 的 HTTP 转上游 WS | 配置选择 ctx_pool 时，通过上游 Responses WS 池承接支持的 HTTP 生成请求，客户端 JSON/SSE 形式保持原样 | `backend/internal/service/openai_http_ws_policy.go`；`backend/internal/service/openai_http_ws_transport.go`；`backend/internal/service/openai_ws_protocol_resolver.go` | 仅旧布尔开关不代表显式选择 ctx_pool；会话隔离和禁止重复派发必须保留 |
| GPT-6.2 提示词缓存适配 | Sol/Astra 系列保留稳定 developer 前缀适配，显式客户 instructions 保留，续聊不重复补默认前缀 | `backend/internal/service/openai_codex_transform.go` | 不能扩散到旧模型/compact；零新增提示词优先；缓存命中仍由上游决定 |
| OAuth 客户端身份和指纹 | 维护 UA、版本、originator 一致性；可选指纹策略维持账号稳定身份 | `backend/internal/service/openai_codex_identity.go`；`backend/internal/service/openai_codex_fingerprint.go`；`backend/internal/service/setting_gateway_runtime.go` | 不把单请求追踪 ID 固化成可复用连接身份，也不取消必要的会话隔离 |
| OAuth 429 处理 | 区分配额耗尽和暂时限流，保留有限的同账号重试、冷却和换号预算 | `backend/internal/service/openai_account_runtime_block_fastpath.go`；`backend/internal/handler/openai_gateway_handler.go` | 不无限重试，不承诺消除上游限流；已派发 WS 的防重放边界优先 |
| Codex 独立上下文兼容 | 对严格匹配的缺失 call_id 的 browser/terminal 明文上下文转为用户文本，保留外部上下文包装 | `backend/internal/handler/openai_page_context.go` | 不放宽普通工具调用校验，不误认同名未配对调用；此逻辑不能解密上游加密内容 |
| 风控账号范围和 PRO 家族 | 支持按账号、凭据类型和套餐筛选；PRO 包括 pro20/pro200/pro500 等已知枚举与规范化写法 | `backend/internal/service/content_moderation.go`；`backend/internal/service/content_moderation_pro_plan_test.go`；`frontend/src/views/admin/RiskControlView.vue` | 不是任意 `pro*` 前缀匹配，避免把 professional 等类型误算为 PRO |
| 官方审核内容哈希 | 已按官方内容哈希逻辑处理文本/图片缓存，保留本地审核账号范围 | `backend/internal/service/content_moderation.go`；`backend/internal/service/content_moderation_input.go` | 不恢复早期 API Key/会话隔离哈希；审核服务 Key 健康状态的哈希是另一概念 |
| 模型与价格补充 | 保留 Gemini Neon/Hydrogen 等补充、reasoning 档位与别名、隐式缓存默认费用修正 | `backend/internal/service/pricing_service.go`；`backend/internal/service/billing_service.go`；`backend/internal/service/model_pricing_resolver.go`；`backend/resources/model-pricing/model_prices_and_context_window.json`；`frontend/src/composables/useModelWhitelist.ts` | 与官方重叠部分采用官方完整价格结构；管理员显式覆盖优先；不能把供应商成本当成客户费用 |
| 图像模型别名 | 保留 gpt-image-2.6-flare/sunburst 等模型识别和相应图像兼容能力 | `backend/internal/pkg/openai/constants.go`；`backend/internal/service/openai_images26_test.go`；`backend/internal/service/billing_service.go` | 维护模型列表、识别和计费的一致性，不能仅保留前端标签 |
| 共享 Redis 并发状态 | 实例启动时保留其他实例的有效并发状态 | `backend/internal/repository/concurrency_cache.go`；`backend/internal/repository/concurrency_cache_startup_test.go` | 不能恢复启动全局清空共享并发状态的行为 |
| 用量查询过滤 | 保留多 API Key 和 reasoning 档位等查询条件的一致应用 | `backend/internal/repository/usage_log_repo.go` | 列表、统计、汇总必须使用一致过滤条件 |
| 初始化与更新来源 | 显式控制首次安装及数据库初始化；发行版更新源指向 Quya 仓库 | `backend/cmd/server/main.go`；`backend/internal/repository/ent.go`；`backend/internal/service/update_service.go` | 普通升级不能变成首次安装；关闭初始化时不会自动执行待处理迁移 |

`ctx_pool` 连接采用首次尝试加三次重试，每次获取连接的截止时间为五秒。只有生成尚未派发时，耗尽连接重试才允许 HTTP 兜底；一旦尝试写入 `response.create`，不再自动通过另一个 WS、HTTP 或账号重放。compact、后台任务和协议不兼容场景遵循各自现有分支，不能一律强转。

GPT-6.2 的最初价格补丁已经由后续官方完整档位/上下文/缓存计费实现承接。再次合并时，不应把最早的固定价格补丁覆盖回去；提示词缓存适配是否仍需保留，要单独核对，不能与价格更新混为一谈。

## 分组“零新增提示词”

配置为 `groups.openai_zero_prompt_injection`，默认关闭，适用于 OpenAI 分组。开启后，网关停止添加自身默认 Codex 指令、强制模板、Todo 指引、Spark 图像限制提示、自动图片桥接提示等新增文本，同时保留客户原有 system/developer/instructions 和工具描述。

必要的 OAuth 结构转换仍存在，例如 system 转 developer、上游要求的空 instructions。零新增模式不是逐字节透传，也不能撤销供应商服务端自行添加的提示词。默认工具描述、合成的空工具结果和移动图片说明等补充文本同样受该策略约束。

策略在认证分组确定后形成请求快照，重试、换号和 HTTP/WS 转换不能重新加入默认提示。已有 WS 会话继续使用原快照，修改设置后需新连接采用新策略。模型目录不得由网关再嵌入默认指令，但供应商原本返回的模板不是网关能够证明或消除的注入。

主要源码：`backend/internal/service/openai_prompt_policy.go`、`backend/internal/service/openai_codex_transform.go`、`backend/internal/service/openai_codex_models_prompt_policy.go`、`backend/internal/pkg/apicompat/`、`backend/ent/schema/group.go`。

合并时重点检查：GPT-6.2 稳定前缀、跨协议转换、工具结果修复、WS 续聊和模型目录。认证、审核、模型映射、计费及会话约束继续工作。

## 原生订阅重置与已撤销的 PHP 扩展

### 仍然保留：Sub2API 原生订阅重置

用户订阅页的重置功能由 `subscription_reset_enabled` 控制，默认关闭。服务端每次操作读取开关，读取失败不允许重置；与自动周期重置的冲突也在服务端限制。

剩余时间严格大于 31 × 24 小时只是入口条件。服务端继续核对本地已使用的订阅购买码、用户、分组、购买备注、实际 30/31 天期限、订阅前后关系及激活回执，不能仅按剩余天数生成新权益。核验范围是本地已有记录，并不意味着实时核验外部商城的支付、退款或商品状态。

确认重置后，放弃当前周期剩余额度，提前启用最早一份符合条件的续购，按该份实际期限重新开始；后续续购继续保留排队。回执和幂等键阻止重复点击消耗下一份续购。用户级事务锁协调购买码、传统商城操作、退款和自助重置；允许符合回执条件的在途旧请求结算到旧周期，缓存版本防止旧额度覆盖新周期。

保留的接口包括用户 `GET /api/v1/subscriptions/:id/reset-preview`、`POST /api/v1/subscriptions/:id/reset`，以及管理员订阅的 `activate-renewal` 和 `renewal-activation` 路由。

主要源码：

- `backend/internal/service/subscription_native_reset.go`
- `backend/internal/service/subscription_renewal_activation.go`
- `backend/internal/service/subscription_store_legacy.go`
- `backend/internal/service/setting_subscription_reset.go`
- `backend/internal/server/routes/subscription_reset_guard.go`
- `frontend/src/views/user/SubscriptionsView.vue`
- `frontend/src/views/admin/SettingsView.vue`

### 已撤销：为 PHP 循环扩大新增的原子重开接口

提交 `eca3e12f4` 撤销了 `d7f2f0f93`。因此，`replace-store` 原子撤销并重开接口、对应服务/处理器以及 `contrib/php-store-cycle/` 补丁不属于当前保留功能；该提交附带的旧购买处理扩展也不能当成仍在生效。

这次撤销没有删除上面列出的原生订阅重置。PHP 循环脚本单独维护，沿用原有商城 MySQL 数据与 Sub2API 传统分配/撤销/充值接口；修复 PHP 脚本不应自动扩大为依赖新后端接口或升级后端容器。

## 数据库边界

| 功能 | 数据库变化 | 升级时的核对事项 |
| --- | --- | --- |
| 原生订阅重置 | `backend/migrations/253_quya_subscription_renewal_activations.sql` 新增 `subscription_renewal_activations` 回执表 | 包含新旧订阅、用户/分组、购买码、幂等键、请求哈希和 JSON 回执等；共享数据库的所有相关实例需支持相同回执和周期语义 |
| 分组零新增提示词 | `backend/migrations/254_group_openai_zero_prompt_injection.sql` 为 `groups` 新增默认 false 的布尔字段 | 核对迁移记录和实际字段；不能只更新前端后假设后端已支持 |
| 本次禁用同步调用 | 无新增表或列，使用既有 `accounts.extra` | 核对调度快照序列化、缓存更新和旧账号默认值 |
| 本次中断恢复与计费 | 无新增表或列，复用现有 usage/计费/Ops 流程 | 核对正常计费和恢复计费均只记一次，检查管理员诊断来源 |

上述 253/254 是已保留功能的迁移，不是本次同步调用修复新产生的迁移。实际环境是否执行过，必须独立核对数据库迁移记录和结构；不能由本文推断。关闭数据库初始化时，应用不会替部署流程补做迁移，也不应为方便升级重新初始化现有业务库。

## 合并与发布验证

每次相关重构至少覆盖以下行为：

1. 开关默认值、新建/编辑/批量开关、调度快照字段；同步请求跳过受限账号，流式请求正常选中；没有可选账号时不绕过限制。
2. Responses、Chat Completions、Messages、compact 和 WS 的客户端模式判断；粘性路由、续聊绑定和最终转发检查保持一致。
3. 正常 SSE 完成、显式失败、缺终态有输出、缺终态无输出、文本前缀/增量/完成覆盖、完整与残缺函数调用、透传及错误 Content-Type。
4. 实际 usage 优先、显式零用量、缺用量估算；余额/订阅仅扣一次，Ops 来源可区分且不写入正文；compact 不伪造完成事件。
5. 零新增提示词在 OAuth、API Key、跨协议转换、WS、旧模型和 GPT-6.2 下均生效；开关关闭时维持原行为。
6. 原生订阅预览、重复点击、购买记录变化、多份续购、旧请求延迟结算及共享缓存；已撤销接口不应重新变为 PHP 的依赖。
7. 共享 Redis 下单实例重启不删除另一实例有效并发记录；上游模型/价格更新不覆盖管理员价格配置。

本轮发布范围以测试环境部署验证为主；生产实例只准备版本、迁移差异及恢复材料，不能把准备视为已获正式升级授权。本文不记录发布完成状态，具体提交、镜像和实测结果由发布记录给出。
