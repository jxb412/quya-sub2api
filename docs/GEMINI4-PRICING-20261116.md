# Gemini 4 价格及模型支持更新

> 本文为 2026-11-16 的历史实施记录。`0.4.27-quya.1` 起 Gemini 模型、定价和思考转换以官方 `v0.2.23` 为准；部分自有补充已被替换，见 [合并说明](UPSTREAM-0.2.23-MERGE-20261118.md)。

日期：2026-11-16（Asia/Shanghai）。基于 Quya `0.4.24-quya.1` / `d61ec9c976e2e911cbc8d8d1680f0379be945364`。

## 上游来源和范围

整合以下两个仍处于 OPEN 状态的上游 PR 的代码增量，并补齐档位定价和回归覆盖；不代表上游已经合并：

- [#8661](https://github.com/Wei-Shaw/sub2api/pull/8661)，Carbon 接入，head `d70a23f84b7329ca76dacae590df132993d415fc`。
- [#8746](https://github.com/Wei-Shaw/sub2api/pull/8746)，Hydrogen 计费，head `c36cd546bb268f835a38983b77219f2c754c7381`。

沿用既有价格目录、缓存、分组和渠道定价机制。没有新增数据库迁移，不需要建表或加字段。本次代码分支未发布生产容器。

GitHub 安全检查另检出基线 Go 1.27.3 的 `GO-2026-6859`（net/http MaxBytesReader 竞态导致 HTTP/1 服务崩溃）。构建工具链、镜像基础版本和 CI 版本断言同步升至修复版本 Go 1.27.4；不修改应用依赖版本或数据库结构。

## 覆盖的 13 个名称

| 基础模型 | 同价的思考档位别名 | Standard 输入 / 输出 / 缓存读取 | Priority 输入 / 输出 / 缓存读取 |
| --- | --- | --- | --- |
| `gemini-4-argon` | `-high`、`-low`、`-medium` | 2 / 10 / 0.10 | 3.60 / 18 / 0.18 |
| `gemini-4-carbon` | `-high`、`-low`、`-medium` | 2 / 10 / 0.10 | 3.60 / 18 / 0.18 |
| `gemini-4-hydrogen` | `-high`、`-low`、`-medium`、`-xhigh` | 1.50 / 7.50 / 0.075 | 2.70 / 13.50 / 0.135 |

单位为美元 / 百万 Token 的基础价格，输出包括思考 Token。思考档位改变用量，不自动改变单 Token 价格。最终用户扣费仍按分组/渠道自定义价和适用倍率计算；基础价不等于供应商采购实扣。

三者均无 200K 长上下文附加费，隐式缓存不收显式缓存存储费。Argon Cyber Permissive 保留独立标准价 4 / 20 / 0.20，不享受 Argon 优惠。

**优惠到期：**[Google 官方价格说明](https://aistudio.google.com/docs/pricing)明确 Argon、Carbon 的上述优惠到 **2026-11-30**；从 **2026-12-01** 起 Standard 变成 4 / 20 / 0.20，Priority 变成 7.20 / 36 / 0.36。Hydrogen 无此限时条款。本次继承 PR 的静态优惠价，没有添加按日期自动翻倍逻辑；到期前需复核并更新有效价格目录/override 与内置兜底价，不能假定远程同步会自动覆盖内置缺项。

## 修复和配置说明

- 补齐 Carbon/Hydrogen 目录及硬编码兜底价，Hydrogen 包含 `xhigh` 的确定性价格别名；支持 `models/`、Vertex 资源路径。
- 同名显式档位价优先于基础模型价；未发布的未知后缀不按家族名模糊套价。
- 修复 Argon/Cyber 在硬编码兜底时 Priority 使用默认 2 倍的问题，统一为对应价卡的 1.8 倍。
- 原生 Gemini、CLI 和前端模型白名单增加官方 Carbon/Hydrogen 裸名。不会据此声明供应商支持任意带档位后缀的路由。
- Gemini 兼容入口保留 Argon/Carbon/Hydrogen 的 `medium`，Hydrogen 的 `xhigh` 不再被降为 `high`；未指定档位时保留上游默认值。Carbon 的 Antigravity 适配沿用 PR 的 `thinkingLevel` 转换。
- Google 官方接口使用裸模型名并通过 `generationConfig.thinkingConfig.thinkingLevel` 指定档位；中转供应商若公布带后缀模型，可通过账号模型映射接入，费用按上述别名规则解析。
- 修复 Chat Completions 兼容转换把 `xhigh` 经 Anthropic 中间格式降为 `high` 的问题，并保留最终实际转发档位用于日志/用量记录。该版本没有原生 Gemini 的直接 Responses 入口；本次没有添加未经验证的 Antigravity Hydrogen 路由。

分组价优先于渠道价，二者优先于默认目录/兜底。分组或渠道配置一个裸模型的自定义价不会自动覆盖其所有思考档位；需要自定义时，可在同一价卡的模型列表中明确填写该家族所需的裸名和档位名称。这样也不会把未来未知变体包含在通配规则内。

## 生产只读核对

2026-11-16 北京时间 15:30–15:37 核对 `<PRODUCTION_PRIMARY>` 的 `sub2api-sb`：运行版本仍为 `0.4.24-quya.1`，健康正常。动态价格缓存 255 条不包含这三个家族，容器内置价格目录包含 Argon；Carbon/Hydrogen 缺失。配置文件、环境变量、分组、渠道及账号统计自定义价中均未找到可补齐这九个 Carbon/Hydrogen 名称的配置。

因此生产现状为 Argon 四个名称有价，Carbon/Hydrogen 九个名称待本次代码部署后补齐。不能仅凭 JSON 中没有逐一列出 `-high` 等名称判断缺价，应同时核对基础价和别名解析。本次只读检查未调用付费模型，也未更新线上价格或容器。

## 验证

- Go 的 Antigravity、domain、Gemini、Gemini CLI 包测试通过。
- 价格回归验证全部 13 个名称及 Cyber，覆盖目录/无远程价格、路径前缀、Standard/Priority、缓存、分组倍率、长上下文、显式档位价及分组/渠道覆盖优先级。
- 兼容网关通过模拟上游检查思考档位透传及思考 Token 计入输出用量。
- 前端模型选择及默认价格组件共 31 项测试、类型检查、ESLint 检查通过；后端服务编译通过。

本地审查证据保存在 `<LOCAL_AUDIT_ARTIFACTS>`。此更新未进行真实 Gemini 付费调用；价格匹配覆盖不等同于供应商账号调用权限或在线可用性验收。
