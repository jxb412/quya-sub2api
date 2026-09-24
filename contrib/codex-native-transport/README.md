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

## 账号智力巡检

- `intel_enabled` 是总开关，关掉后手动检测也不可用；`intel_loop_enabled` 只控制自动循环。
- 巡检请求复用插件最近看到的一条真实 ChatGPT Codex `/responses` 请求形状（url 头 body），
  因此需要先有真实业务流量经过插件；面板 `/api/status` 的 `template_ready` 反映这一点。
- 结论、检查时间、模型、延迟与最近回答落盘在 `intel_state_path`，重启后保留。
- `intel_plan_types` 可按套餐类型筛选（留空 = 全部类型）。
- 面板地址：`http://<panel_addr>/?token=<panel_token>`，账号页在 `/accounts`。

## 降智账号 BPS 通道

面板里对单个账号点「降智处理」后，该账号智力不合格时不再被自动暂停，而是把
`bps_models` 列出的模型请求改走 `bps_endpoint`；直到该号恢复合格为止。

出站会按上游白名单重写请求体，这是实测出来的硬约束：

- 接受：`model` / `input` / `stream` / `reasoning` / `prompt_cache_key` / `instructions`，
  以及 `metadata`（**只允许 `task_id` + `turn_id` 两个键**）；
- 拒绝（422 `Invalid request body`）：非空 `tools`、`tool_choice`、`parallel_tool_calls`、
  `text`、`include`、`temperature`、`top_p`、`truncation`、`previous_response_id`、
  `store: true`、metadata 里多出的任何键、图片附件；
- `input` 里的 `reasoning` 项（encrypted_content 不是它的）会 400，会被剥掉。

推理档位只能走顶层 `reasoning_effort`（`low` / `medium` / `high` / `xhigh`）：客户端发来的
`reasoning.effort` 会被折算过去，`max`（含 `maximum` / `x-max`）折成 `xhigh` 而不是掉到
`medium`，避免把高质量请求静默降级；上游没有 `max` 挡位，实测直发会 422。

因此这里把「客户端工具目录 + 调用协议」写成一条 developer 输入项：模型需要调用客户端工具时
只输出一行 JSON `{"__tool_call__":{"name":...,"arguments":{...}}}`，回程再把它翻成标准
`function_call`（含 `output_item.added` / `function_call_arguments.delta|done` /
`output_item.done`），客户端回放的 `function_call_output` 会在下一轮转成文本历史。
上游注入的工作簿工具调用与联网检索项会被压制，避免脏 item 流给客户端。

同一会话按 `prompt_cache_key`（缺省回退 `session_id` / `thread-id`）派生稳定的
`task_id` / `turn_id`，第二轮起可命中上游缓存（实测 `cached_tokens` ≈ 99%）。
因为 BPS 会注入一整套 Office 插件系统提示，单次请求输入开销约 17k token，
所以建议只对确实被限流的账号开启，并先用面板的「BPS自检」确认能拿到 2xx。

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
