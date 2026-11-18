# Quya 合并官方 Sub2API 0.2.22 / 0.2.23

日期：2026-11-18。候选版本：`0.4.27-quya.1`。

基于 Quya `36a5ac1e2049d3912b9b9aa952449b3f37add02a`，合并官方 `v0.2.23` 的提交 `5af9afb588c4b014b3df9db7d4073af1f8ae01b6`。该提交已包含 `v0.2.22`（`b0fa9f3e6eeab1968a53e1289b69daa4bb40ca2b`）；不是仅摘取价格更新。

## 模型及定价采用官方实现

根据运营方要求，最近两次 Gemini 4 和 GPT-6.2 增量中与官方重叠的实现以官方为准。价格服务、计费服务、内置价格 JSON、OpenAI 默认模型/能力，以及 Gemini 兼容思考转换代码均对齐 `v0.2.23`。不叠加旧的硬编码规则。

GPT-6.2 Standard 输入 / 缓存读取 / 输出价仍为 Astra `8 / 0.40 / 40`、Sol `2 / 0.10 / 10` 美元/百万 tokens。新增官方完整档位、缓存写入、长上下文和订阅套餐能力逻辑：

- Fast 为 2 倍，Flex 为 0.5 倍。
- Ultra Fast 在 2027-03-01 00:00 太平洋时间（北京时间当日 16:00）之前为 3 倍，之后为 6 倍，按请求定价时间计算。
- 超过 272K 输入的长上下文按官方条件执行：输入 2 倍、输出 1.5 倍；保留订阅账号豁免及分组/渠道自定义价规则。
- 官方缓存写入价格为基础输入的 1.25 倍。
- 分组可强制 Ultra Fast，渠道可配置 Ultra Fast 倍率；账号套餐能力按官方实现限制。

Gemini Carbon/Hydrogen 的计费和思考转换使用官方规则。保留已有 Hydrogen 裸模型名在原生 Gemini、Gemini CLI 和后台可选列表中的展示；没有新增官方未声明的路由别名。历史 `GEMINI4-PRICING-20261116.md` 中自有 xhigh/medium 保真、严格后缀识别、Argon 静态兜底 Priority 修正等描述不再作为当前版本行为承诺。官方内置目录与紧急硬编码兜底的 Argon/Cyber Priority 差异按上游现状保留。

## 保留的 Quya 功能

- 套餐用量默认不自动重置；安装与数据库初始化默认关闭。
- 按账号、账号类型及套餐范围风控；审核继续使用官方内容哈希。
- OpenAI API Key 账号的 Codex 客户端限制与 Responses-only 限制。
- 多 API Key 用量筛选、推理强度汇总、分钟级时间范围和客户端时区处理。
- 插件原始客户端身份与稳定会话、Codex 原生传输插件及 BPS 辅助服务。
- Quya 更新来源、镜像发布流水线及独立品牌文档。

针对官方更新与自有路径的交叉点，额外补齐两处兼容问题：

1. 调度精简快照保留 `codex_cli_only`、`codex_cli_only_allow_app_server`、`openai_responses_only`、`openai_inbound_capabilities`，让账号初筛使用完整限制；已有转发前检查继续保留。
2. 自有多 Key 模型统计路径同步官方修复：客户实际扣费 `actual_cost` 与账号成本 `account_cost` 分开聚合，按账号筛选时不会把客户实扣替换成账号成本。

## 数据库升级准备

旧 SQL 未修改或删除，新增 3 份迁移；两个 `251_` 文件名称不同，迁移器按完整文件名登记和排序，不要只按数字去重或改名：

| 迁移文件 | 结构变化 |
| --- | --- |
| `251_group_force_openai_ultrafast.sql` | `groups.force_openai_ultrafast BOOLEAN NOT NULL DEFAULT FALSE` |
| `251_group_usage_rollup_nonblocking_build.sql` | `usage_group_rollup_state.expired_before TIMESTAMPTZ NOT NULL`、`building_until DATE`、`build_dirty_from DATE`；新增/替换汇总失效函数，降低重建汇总与日志写入之间的锁冲突 |
| `252_channel_pricing_ultrafast_multiplier.sql` | `channel_model_pricing.ultrafast_multiplier NUMERIC(12,6)`，允许 NULL 或正数，并添加检查约束 |

没有新增表。迁移默认保持新功能关闭或沿用原倍率。生产目前 `DATABASE_INITIALIZATION_ENABLED=false`，部署时必须先通过项目迁移器应用完整待执行迁移并核对迁移记录，再以关闭初始化的正常配置运行新应用。不能仅切换镜像，也不能只手工加列而漏掉函数及校验和记录。汇总迁移设置了 5 秒锁等待上限；若遇到长事务超时，先确认事务和锁，再重试。

部署按运营方追加指令执行：先备份核心数据库数据、应用数据与配置、旧容器快照，验证备份后再迁移和升级。请求、运维和审核日志可排除，账务去重数据与业务账本必须保留。备份产物、实际迁移结果及生产验收另存本次运维记录，不将生产配置或凭据提交仓库。

## 合并验证

前端 ESLint、类型检查、37 个测试文件共 697 项测试及生产构建通过。后端全包单元测试中的服务、协议转换、风控、套餐、安装、迁移等包通过；仓储初次遇到 Windows `sh` 不在 PATH 及新增测试 fixture 问题，修正环境和 fixture 后相应仓储回归通过。完整 Linux CI、集成测试和安全扫描随合并 PR 执行，发布前核对结果。
