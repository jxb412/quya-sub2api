# Quya Codex Native Transport

这是 Quya Sub2API 的 OpenAI OAuth 出站传输插件源码，插件 ID 为 `io.quya.codex-native-transport`。插件只负责账号已经选定之后的上游网络连接；账号调度、OAuth 刷新、协议转换、重试、计费和用量统计仍由 Sub2API 主程序负责。

## 已实现

- Rust `reqwest 0.12.28` + `native-tls` + `hyper 1.8.1` + `h2 0.4.16`。
- HTTP/2 默认开启；可选强制 HTTP/1.1。
- 每账号独立连接池和 Cloudflare 基础 Cookie Jar，默认不读取进程代理环境变量。
- 支持 HTTP、HTTPS、SOCKS4a、SOCKS5、SOCKS5h 出站代理；Sub2API 的
  `socks5://` 按原 Go 传输行为使用代理端 DNS，避免灰度切换改变解析路径。
- 只保存 Cloudflare 基础 Cookie，不保存 ChatGPT 会话 Cookie。
- 仅接受 OpenAI OAuth 账号和批准的 `chatgpt.com`、`api.openai.com` 上游地址。
- `passthrough` 身份模式默认开启，不重复改写宿主已经生成的身份。
- 可选 `machine` 模式：按账号稳定映射已存在的 session/thread/window/prompt_cache_key，同时保留下游会话边界。
- 可选 Codex CLI/Desktop/OpenCode/Pi User-Agent Profile。
- 可选固定版本或从 NPM 同步 `@openai/codex` 版本文本。同步不会改变已经编译的 TLS/HTTP2 依赖。
- 严格配置校验、请求体大小限制、代理地址校验和敏感错误信息脱敏。
- 内置只读管理面板（`panel_addr` + `panel_token`，建议只监听回环地址或用反向代理暴露）。
- 账号智力巡检：借一条真实 Codex 流量做请求形状，对选定账号/套餐调用指定模型提问，
  按回答内容判定「智力是否合格」，可手动或按间隔循环，并可选自动暂停不合格账号。
- 降智账号 BPS 通道（实验，默认关）：把不合格账号的指定模型请求改道
  `https://bps.openai.com/basispoints/api/responses`。
- BPS 附件策略：绝对 https 图片地址原样透传（网关自己下载）；base64 图片与文件
  默认让这条请求整体走正常通道（`bps_skip_on_media`），不再把客户的图静默丢成占位文本。
- BPS 附件闸门扫全量输入：除 message 的 `content` 外，还扫 `function_call_output` /
  `custom_tool_call_output` 的 `output` 数组（Codex Desktop 的 `view_image` 之类工具会把
  base64 截图放进工具输出），漏扫的请求照样吃上游 422。
- BPS 工具桥接四选一（`bps_tool_mode`）：`text`（默认，一行 JSON 协议）、`native`
  （历史工具项原生回放）、`officejs`（借上游 `run_officejs` 当运货卡车）、`declared`
  （客户端工具写进 `input` 的 `additional_tools` 条目，由上游原生注册，模型直接发
  客户端工具名的 `function_call` / `custom_tool_call`）。
- 跨机状态同步（`sync_enabled`，默认关）：把每账号「自动降智处理」开关在多台
  服务器之间对齐，解决同一个账号在一台勾了、另一台没勾的问题。

## 账号智力巡检

- `intel_enabled` 是总开关，关掉后手动检测也不可用；`intel_loop_enabled` 只控制自动循环。
- 巡检请求复用插件最近看到的一条真实 ChatGPT Codex `/responses` 请求形状（url 头 body），
  因此需要先有真实业务流量经过插件；面板 `/api/status` 的 `template_ready` 反映这一点。
- 结论、检查时间、模型、延迟与最近回答落盘在 `intel_state_path`，重启后保留。
- `intel_plan_types` 可按套餐类型筛选（留空 = 全部类型）。
- 面板地址：`http://<panel_addr>/?token=<panel_token>`，账号页在 `/accounts`。
- 探针只借模板的「壳」，不借模板的「能力」：`tools` / `tool_choice` /
  `parallel_tool_calls` 会被剥掉，`text.format` 里的 json 约束会降级成纯文本。
  真实 Codex 模板带十几个工具，原样转发时模型经常先发一个 `function_call`
  而不产出正文，面板上就会成片出现「上游未返回回答正文」。
- 模板里带 `X-OpenAI-Internal-Codex-Responses-Lite` 时，探针会自动补
  `reasoning.context = "all_turns"`（上游对 lite 请求的硬校验，缺了整池 400）。
- 200 但一个正文帧都没有（上游断流 / 分块截断 / 连接被重置）时会按
  `intel_prompt_retries` 重试；最终仍失败时，面板的「回答」列会给出可排查的摘要：
  SSE 事件序列 + 输出项类型 + 上游错误帧 + 断流原因，而不是只截
  `response.created` 里回显的 instructions。

## 降智账号 BPS 通道

面板里对单个账号点「降智处理」后，该账号智力不合格时不再被自动暂停，而是把
`bps_models` 列出的模型请求改走 `bps_endpoint`；直到该号恢复合格为止。

出站会按上游白名单重写请求体，这是实测出来的硬约束：

- 接受：`model` / `input` / `stream` / `reasoning` / `prompt_cache_key` / `instructions`，
  以及 `metadata`（**只允许 `task_id` + `turn_id` 两个键**）；
- 拒绝（422 `Invalid request body`）：非空 `tools`、`tool_choice`、`parallel_tool_calls`、
  `text`、`include`、`temperature`、`top_p`、`truncation`、`previous_response_id`、
  `store: true`、metadata 里多出的任何键、`input_image` 的 base64（`data:`）形态、
  带 `file_id` 的图片、以及任意 `input_file`；
- 例外：`input_image` 的 `image_url` 是绝对 https 地址且不带 `file_id` 时上游接受，
  由网关自己去下载那张图（实测 GitHub raw png 识别正确）。由 `bps_keep_https_images`
  控制（默认开），思路来自 `ranxi2001/sub2api` 的 `basispoints/images.go` 与
  `codex-basispoints-transport` 的 `keep_https_images`；
- `input` 里的 `reasoning` 项（encrypted_content 不是它的）会 400，会被剥掉。

因此这里把「客户端工具目录 + 调用协议」写成一条 developer 输入项：模型需要调用客户端工具时
只输出一行 JSON `{"__tool_call__":{"name":...,"arguments":{...}}}`，回程再把它翻成标准
`function_call`（含 `output_item.added` / `function_call_arguments.delta|done` /
`output_item.done`），客户端回放的 `function_call_output` 会在下一轮转成文本历史。
上游注入的工作簿工具调用与联网检索项会被压制，避免脏 item 流给客户端。

### 工具桥接方案（`bps_tool_mode`）

上游顶层 `tools` 一律 422，所以工具只能靠提示词或 `input` 里的条目转达。四种方案：

- `text`（默认）：给模型一份「工具目录」+ 一行 JSON 协议，回程把协议文本翻成标准
  `function_call`；历史里的工具项折成 `<tool_result>` 文本消息。兼容性最好，
  代价是模型把工具名当文本背写，命中率看模型心情。
- `native`：历史 `function_call` / `function_call_output` 按原生 item 形状回放
  （前缀缓存更稳），新调用仍走一行 JSON 协议。
- `officejs`：在 `native` 之上让模型用上游 Excel 插件的 `run_officejs` 当运货卡车，
  插件只从 `code` 字段里取真正的客户端工具调用，不执行任何工作簿代码；上游没有
  该工具时自动退化成一行 JSON 协议。
- `declared`：把客户端工具写成 `input` 里的
  `{"type":"additional_tools","role":"developer","id":"at_…","tools":[…]}` 条目，
  上游**原生注册**其中的 `function` / `custom` / `namespace` 工具，模型直接发客户端
  工具名的 `function_call`，历史与回程都不改写。三条实测硬约束：条目必须带
  `role: "developer"`（缺了 400 `Missing required parameter role`）；`strict: true`
  的函数必须带 `additionalProperties: false`（否则 400，插件自动降级成非 strict）；
  `namespace` 工具必须带 `description`（空串即可，否则 400
  `Missing required parameter: 'tools[0].description'`，插件自动补空串）；
  `id` 由工具表内容哈希派生（同一份工具表每轮同值，上游前缀缓存才能命中）。
  条目固定插在开头那几条 developer 消息之后，保证同一会话每轮前缀一致。
  回程只放行**名字在客户端工具表里**的 call，网关自己注入的工作簿工具调用照旧压制；
  不支持 `namespace` 子工具的上游会报错，所以这是需要实测再切的档位。

### 会话身份与出站特征（0.4.23）

BPS 出站的 `metadata` 与 `prompt_cache_key` 全部按内容派生，**不带随机数**，
口径与 ranxi2001/sub2api 的 `basispoints` 实现、ghcp_proxy 参考实现一致：

- `task_id` = `HMAC(种子, 账号作用域 + 会话锚点)`：同一会话多轮恒定。会话锚点 =
  客户端 `prompt_cache_key`，没给就用历史第一条 item 的指纹；
- `turn_id` = `HMAC(种子, 账号作用域 + 到最近一条 user 消息为止的前缀)`：同一回合
  重发（客户端重试、agent 迭代）恒定，进入新回合才变；
- `agent_iteration` = `1 + 本回合里的 *_call_output 数量`：**只有它逐轮递增**。
  上游据此把重试认成「同一个 turn」，不会把已完成的 plan 当成新 turn 重新规划
  （恒定的 turn_id + 递增的 iteration 也是缓存前缀能命中的前提）；
- `prompt_cache_key` 换成 `HMAC(种子, 账号作用域 + 客户端会话键)` 的 UUID 形态假名
  （默认开，`bps_pseudonym_prompt_cache_key=false` 可关掉做 A/B）：同会话多轮同值、
  跨账号不同值，客户端原始会话键不再外传；
- `HMAC` 种子取 `identity.installation_id_seed` —— 与身份 Profile（machine 假名化、
  per-account installation id）**同一份种子**，两套派生互不干扰但口径统一；
- `agent_iteration` 随 `bps_metadata_agent_iteration` 开关（默认开）。万一上游改口径
  拒绝这个键，关掉即可，其余两项不受影响。

出站客户端特征头统一由配置生成（出站与面板「BPS自检」同一份口径）：
`x-basispoints-auth-mode: chatgpt`、`accept-encoding: identity`、
`origin`（`bps_origin`，默认 `https://bps.openai.com`，留空 = 不发）、
`user-agent`（`bps_user_agent`，默认空 = 保持客户端 UA；`browser` = `Mozilla/5.0`
浏览器 UA 实验档；其它值原样发送）。换值后直接用面板「BPS自检」对比 2xx 率即可。

**排查日志**：宿主会吞掉插件的 stderr，所以 BPS 相关的结论（本地拒绝、上游 4xx
片段、挡位自愈）都会落到 `degrade_state_file` 同目录的 `bps-notes.log`（ISO-8601
UTC 时间戳 + 账号 id，超过 1MB 滚动成 `.1`）。

因为 BPS 会注入一整套 Office 插件系统提示，单次请求输入开销约 17k token，
所以建议只对确实被限流的账号开启，并先用面板的「BPS自检」确认能拿到 2xx。

### 推理挡位归一化与自愈（0.4.23）

上游对推理挡位是分模型的硬校验，客户端的能力表经常滞后，现网最大的两类 400 都是这个：

- **BPS 通道**：只认 `low/medium/high/xhigh`。出站把 `none`/`minimal` 折算成 `low`、
  `max`/`ultra`/`x-high`/`extra_high` 折算成 `xhigh`；**其它不认识的值本地拒绝**
  （不再静默回落 `medium`），这条请求改走该账号的正常通道并写进 `bps-notes.log`。
- **智力巡检探针**：与 BPS 通道**共用同一个归一化函数**。模板里的 `minimal` 原样
  发给 `gpt-6-astra` 会被 400，然后被误读成「账号被降智」——142 上一轮 53 个号就是
  这么被误判的。探针会归一化模板挡位，并把「模板挡位改成了什么」写进该行的 note
  （面板「回答」列下方），完全不认识的值退回 `medium` 并在 note 里说明。
- **正常通道自愈**（`effort_retry_enabled`，默认开）：上游回
  `Unsupported value: 'minimal' is not supported with the 'gpt-5.5' model. Supported
  values are: 'none', 'low', …` 时，按上游自己给的支持列表挑**最接近**的一档
  （ties 选更弱的一档，例如 `minimal` 在 `none`/`low` 之间选 `none`）重发一次，
  成功就记住这个「模型 + 挡位」组合，后面同类请求出站前直接换掉，不再撞 400。
  只在这类 400 上触发，与挡位无关的 400 原样回放给宿主，一个字节都不动。
  想关掉（例如想看上游原始报错）把 `effort_retry_enabled` 设为 `false`。

带附件的请求按两个开关处理（都在面板「降智账号 BPS 通道」里）：

- `bps_keep_https_images`（默认开）：绝对 https 图片地址原样发给上游，由网关下载；
- `bps_skip_on_media`（默认开）：其余带附件的请求（base64 图片、文件、带 file_id 的图片）
  整体走该账号的正常 Codex 通道 —— 代价是这一条请求不吃降智兜底，换来客户截图 / 文件不丢。
  闸门扫的是**全量输入**：message 的 `content` 与工具输出（`function_call_output` /
  `custom_tool_call_output`）的 `output` 数组；命中就整条改走正常通道，不再交给上游 422。

两个开关都关掉时恢复旧行为：附件被删掉、原地写一行「附件已省略」占位文本。

动态工具（Codex 的 `tool_search`）走 BPS 必然失效：BPS 会把客户端 `tools` 白名单
摘掉、改用 `additional_tools` 重新注册，回程还会抑制 `tool_search_*` 项，客户端的
工具发现链路就断了（表现为工具调用不动、只回消息）。所以命中动态工具的请求单独有一个
闸门：

- `bps_skip_on_dynamic_tools`（默认开）：请求里出现下列任一形态就整条走该账号的正常
  Codex 通道，不让 BPS 经手 —— 顶层 `tools[]` / `namespace` 子工具 / Responses Lite
  `additional_tools.tools[]` 里的 `tool_search`（含 `tool_search_preview`）声明，
  以及 `input[]` 里的 `tool_search_call` / `tool_search_output` 历史项。
  关掉即恢复旧行为（动态工具请求也进 BPS）。

  **注意**：宿主在把请求交给传输插件之前会自己做一轮 item 归一化 —— 例如一个没有配
  `tool_search_call` 上下文的孤立 `tool_search_output`，宿主会直接摘掉，插件那一侧
  根本看不到（实测 items 只剩 `message`）。成对出现（真实回放形态）时两个 item 都会
  原样带到插件，闸门按预期命中。排查这类「为什么没跳过」时先看宿主这一层。

`previous_response_id` 是第二道同类闸门（0.4.31 起）：

客户端门禁是第三道闸门（0.4.33 起；0.4.34 起身份来源改为宿主透传，见下）：

- `bps_official_client_only`（默认开）：只有**官方 Codex 客户端家族**的请求才允许走
  BPS，其余客户端（WorkBuddy、OpenClaw、`OpenAI/JS`、浏览器、`curl` 等）一律走该
  账号的正常通道。判定口径与 sub2api `internal/pkg/openai/request.go` 的
  `IsCodexOfficialClientByHeaders` 完全一致：

  1. UA 前缀集（前缀或包含匹配）：`codex_cli_rs/`、`codex-tui/`、`codex_vscode/`、
     `codex_vscode_copilot/`、`codex_app/`、`codex_chatgpt_desktop/`、`codex_atlas/`、
     `codex_exec/`、`codex_sdk_ts/`；
  2. `Codex ` 家族前缀（保留尾随空格，避免退化成裸 `codex`）；
  3. UA 尾部括号组 `(name; version)` 的 name 命中官方 originator 集合 ——
     `CODEX_INTERNAL_ORIGINATOR_OVERRIDE` 只改 UA 前缀不改尾部，靠这一层能恢复被
     override 的真实客户端；
  4. 或者 `originator` 头命中官方集合（`codex_cli_rs` / `codex-tui` / … / `Codex Desktop`）。

  **客户端身份从哪里来（0.4.34 起，关键）**：宿主在把请求交给插件之前会作出站身份
  收口（指纹收敛 / 统一出口，`enforceCodexIdentityHeaders`），把 `user-agent` 与
  `originator` 强制改写成网关规范 Codex 身份 —— 插件直接读请求头只能看到官方形态，
  门禁会永远放行（0.4.33 在现网就是这个状态）。因此 0.4.34 改为优先读宿主透传的
  客户端原始身份：

  - `x-sub2api-client-user-agent`：客户端自报 User-Agent（宿主在收口前抓到）；
  - `x-sub2api-client-originator`：客户端自报 originator。

  宿主写了这两个头就以它们为准（判定口径与宿主侧
  `openai.IsCodexOfficialClientByHeaders(userAgent, originator)` 逐字一致）；旧宿主
  不写则回退到请求头 `user-agent` / `originator`，行为等同 0.4.33。这两个头是宿主
  私有透传头，插件只读**绝不出站**（`ordered_headers` 按 `x-sub2api-` 前缀整段剥离，
  正常通道与 BPS 通道都不例外）。

  为什么必须加这道门：BPS 是 ChatGPT 官方 Excel/Work 插件的内部端点，上游会给每条
  请求注入自己那套约 1.7 万 token 的系统上下文与账号级人设。官方 Codex 客户端自带
  完整会话与系统提示，注入只是白耗缓存；而第三方聊天客户端（尤其 `instructions`
  为空的）会被那套上下文盖掉 —— 实测客户问「今天星期几」，`gpt-6-astra` 走 BPS 时
  返回 `How can I help you today?`（`prompt_tokens` 17627），同一问题换成不在
  `bps_models` 里的 `gpt-6-sol` 立刻正常回答「星期二」（`prompt_tokens` 19）。
  更严重时会返回上游自带人设的问候（带账号主人名字），客户会以为我们串号。

  命中门禁时该账号面板原因显示「非官方 Codex 客户端，跳过 BPS 走正常通道」，并在
  `bps-notes.log` 留一行客户端身份（UA + originator）；同一账号同一种客户端身份
  每小时只写一行，避免高频客户端把日志灌满。关掉即恢复旧行为（所有客户端都进 BPS）。

- `bps_skip_on_previous_response_id`（默认开）：请求体里出现非空
  `previous_response_id` 就跳过 BPS。原因是 BPS 上游严格白名单会把这个字段 422
  掉，插件只能剥掉再发 —— 对「靠服务端状态续写」的客户端（每轮只发增量 input +
  上一轮 id，历史在上游）剥掉就等于丢历史，表现就是对话上下文接不上。
  官方 Codex 的 HTTP 请求体里没有这个字段（`ResponsesApiRequest` 无该字段，构造时
  是 `store=false` + 全量 input），所以正常 Codex 客户端不受影响；只有 WebSocket
  增量请求才带它，而本插件只处理 HTTP。
- `bps_previous_response_pin_seconds`（默认 21600 = 6 小时，0 = 只跳本条不钉会话）：
  命中闸门后把整条会话（账号 + 会话键：`prompt_cache_key` / `session_id` /
  `thread-id` / `x-codex-window-id`）钉在正常通道，窗口内一律不走 BPS。
  只跳单条会造成「同一会话前几轮走 BPS、后面几条走正常通道」的撕裂 —— 上游会话状态
  分成两套，客户端照样接不上上下文；钉住之后该会话只在一侧继续。
  钉会话状态在内存里（进程级），与既有的 BPS 会话粘滞同样是 per-process。

回程（响应方向）有两个默认开启的整理项，抄自 `codex-basispoints-transport`：

- `bps_scrub_echo`（默认开）：上游在 `response.created` / `in_progress` / `completed`
  等事件里把它自己的 47 KB Excel 系统提示（`instructions`）与 21 个网关工具（`tools`）
  原样回显，每个事件约 73 KB。开启后把这些键换回客户端**请求里的原值**（客户端没发
  `instructions` 时回填 `null`），既不让网关内部提示词泄露给客户端，也省掉这部分下行
  带宽；客户端看到的工具集与自己声明的完全一致。
- `bps_normalize_usage`（默认开）：BPS 的 usage 多一个
  `input_tokens_details.cache_write_tokens`（实测一次 17634 输入里 17566 是 cache_write），
  宿主把它当 Anthropic 式「缓存写入」从输入里扣掉，结果一条 17k 输入的请求只按几十个
  输入 token 计费。删掉 `cache_write_tokens` / `cache_creation_tokens` 后按普通输入计费，
  与走正常 Codex 线路的计费口径一致。**注意**：这意味着降智通道下的计费会比之前更接近
  真实用量（之前那条线路是少收的）；如果暂时不想改变计费口径，把这个开关关掉即可。

## 跨机状态同步

两台服务器共用同一批账号（同一份 PG）时，同一个账号在两台上是同一个上游身份，
面板里那个「自动降智处理」开关理应两台一致。开启 `sync_enabled` 后：

- `sync_peers` 写对端面板地址（`http://host:8848`，逗号分隔），`sync_token` 是共享密钥
  （留空则复用 `panel_token`，两台用同一个强随机串）；
- 本机改动立刻推一次全量快照，另外按 `sync_interval_seconds`（默认 30s，0 = 只推不对账）
  定期对齐；
- 对端收到快照只写自己的状态、**不再回推**，所以两台互推不会震荡；
- 每个账号按 `enabled_at_ms` 取新（last-write-wins）。两台都是旧格式（没有时间戳）时，
  以「勾上降智处理」的那台为准，不会被空状态抹掉；
- **只同步开关本身**：线路统计（`normal_ok` / `bps_ok` …）是每台自己的流量，不参与同步；
- 对端不可达只影响这条同步链路（面板 `/api/status` 的 `sync` 字段能看到失败原因），
  客户端请求完全不受影响。

同步链路走的是插件自己的面板 HTTP 端口，因此两台必须互相能访问对方的 `panel_addr`
（跨公网时建议只放行对端 IP，或改用反向代理 + 强 token）。

两种部署方式：

- **双向（推荐）**：两台都 `sync_enabled=true`、`sync_push=true`，
  `sync_peers` 各写对方。任意一台面板上改开关都会同步到另一台，实测两台互推不震荡。
- **主从（一台为准）**：主服务器（如 142）`sync_enabled=true`、`sync_push=true`、
  `sync_peers` 写跟随端；跟随端 `sync_enabled=true`、`sync_push=false`、`sync_peers` 留空。
  跟随端只接收、不推送，本机面板上的改动不会外传（`/api/status` 的 `sync_push` 字段可以看到）。

## 默认策略

```json
{
  "per_account_fingerprint": true,
  "per_account_cookie_jar": true,
  "one_id_per_request": false,
  "identity": {
    "fingerprint_mode": "passthrough",
    "profile": "passthrough",
    "version_auto_sync": false
  }
}
```

`machine`、`one_id_per_request` 和自动版本同步都不是默认开启项。`one_id_per_request` 会破坏远端会话延续和缓存亲和，只应作为短期故障排查开关。

## 构建

本机开发需要 Rust stable、`perl`、`musl-gcc`（Linux 目标）和 Python 3。生产 Linux 包由仓库的 `codex-native-transport.yml` 工作流构建。

```bash
cd contrib/codex-native-transport
cargo fmt --check
cargo test
cargo build --release --target x86_64-unknown-linux-musl
python3 tools/package.py \
  --runtime linux-amd64=target/x86_64-unknown-linux-musl/release/plugin \
  --output-dir dist \
  --sign-key /secure/path/codex-native-transport-seed.hex \
  --key-id quya-codex-native-transport-v1
```

签名私钥是 32 字节种子，必须只保存在发布者本地或 GitHub Actions Secret，不能放入仓库、`.s2plugin`、服务器配置或日志。

```bash
python3 tools/ed25519_tool.py keygen /secure/path/codex-native-transport-seed.hex
python3 tools/ed25519_tool.py pubkey /secure/path/codex-native-transport-seed.hex
```

## 在 Sub2API 中安装

1. 在 Sub2API 后台进入“插件管理”，上传 CI 生成的 `codex-native-transport-<version>.s2plugin`。
2. 生产配置保持 `plugins.allow_unsigned: false`。
3. 将签名公钥加入 `plugins.trusted_publishers`，键名必须和 `signature.json.key_id` 完全一致：

```yaml
plugins:
  allow_unsigned: false
  trusted_publishers:
    quya-codex-native-transport-v1: "<工具输出的 Base64 公钥>"
```

4. 安装后先确认状态为“已安装/兼容”，再创建 OpenAI OAuth 能力绑定。
5. 先用 `rollout_percent: 1` 灰度一个账号，检查插件健康状态、请求成功率、SSE 和代理连通性。
6. 测试通过后再逐步提高灰度比例。插件只能有一个 OpenAI OAuth 出站能力绑定，不能和另一个同能力插件同时启用。
7. 首次启用建议保留 `fingerprint_mode: passthrough`；确认主程序已经正确生成身份后，再单独测试 `machine`。

插件不会初始化或修改业务数据库表。宿主的插件管理功能需要现有 `229_plugins.sql` 和 `230_plugin_artifacts.sql` 表；这两项属于 Sub2API 主程序迁移，不由插件自行执行。

## 停用和回滚

先在后台停用能力绑定，再停用插件。停用失败时主程序会拒绝静默切回另一种网络行为，便于发现配置问题。回滚时上传之前经过签名的包并重新灰度，不要直接覆盖插件安装目录。

## 安全边界

插件是独立进程，但不是操作系统沙箱。它会接触宿主传来的 OAuth Authorization、请求体和代理地址，并拥有运行 Sub2API 用户的文件和网络权限。生产环境应使用低权限用户、限制文件权限和出站网络，并避免向插件进程注入无关密钥。

该插件只能让出站网络栈更接近指定版本的 Codex 依赖，不能生成官方设备证明，也不能保证上游接受为官方 Codex 客户端。
