# Gemini 4 Neon 与 Hydrogen 模型、档位和定价补齐

日期：2026-11-21。基线：`5fbd1d9b0af9`，已合并官方 v0.2.24 并含 Quya PRO 风控修复。

## 上游核对

- PR #8952：https://github.com/Wei-Shaw/sub2api/pull/8952
  参考提交：`0a4124a20c82c33a0d8fd923724593bf1e11416e`。
- PR #8960：https://github.com/Wei-Shaw/sub2api/pull/8960
  参考提交：`32c071266154a49b747bfef39d0b78df6c7144f2`。

核对时两个 PR 均为 OPEN、尚未合并，不能认为升级 v0.2.24 已包含这些变更。
基线已有 Hydrogen 原生/CLI 目录及基础定价；Neon 缺失，Antigravity 的
Hydrogen/Neon 分档及 Carbon 裸模型映射也未补齐。

## 本次行为

Gemini 原生及 CLI 目录新增 `gemini-4-neon`。Antigravity 默认映射、模型列表、
前端白名单与预设补齐 Neon/Hydrogen 的 low、medium、high、xhigh，以及 Carbon
的 low、medium、high；裸模型默认映射 high，避免把上游不支持的裸名直接发出。

Neon/Hydrogen 接受 thinkingLevel，包括 medium/xhigh；带档位的模型名不再叠加
thinkingBudget 或冲突档位。保留 Carbon 及较早 Gemini 模型的参数和档位边界。

采用 #8960 的显式配置语义：固定别名仍固定。例如 `gemini-4-neon →
gemini-4-neon-high` 始终走 high。需要让 Codex 请求的 reasoning.effort 控制档位时，
配置基础模型自映射 `gemini-4-neon → gemini-4-neon` 并显式加入需要的各档位自映射。
此时 Codex 目录可合并展示一个基础模型，并公布实际可路由的推理档位。
只有各档位、指定平台、独立模型路由或其他平台认领同名模型时，保持独立目录。
已有账号基础模型能力限制继续作为约束，避免合成档位元数据时扩大上下文窗口。

没有采用 #8952 中覆盖固定别名、联动修改 Claude 档位路由的逻辑，避免改变现有
管理员明确配置的映射。模型目录可见不代表供应商账号已获得该模型权限。

## 官方价格

已独立核对 Google 价格页：https://aistudio.google.com/docs/pricing#gemini-4-neon
以及模型页：https://aistudio.google.com/docs/models/gemini-4-neon

单位：美元 / 百万 tokens。

| 模型 | 档位 | 输入 | 缓存命中输入 | 输出（含思考） |
| --- | --- | ---: | ---: | ---: |
| gemini-4-neon | Standard | 3 | 0.15 | 15 |
| gemini-4-neon | Priority | 5.4 | 0.27 | 27 |
| gemini-4-hydrogen | Standard | 1.5 | 0.075 | 7.5 |
| gemini-4-hydrogen | Priority | 2.7 | 0.135 | 13.5 |

推理档位后缀默认继承基础价，精确模型/档位的自定义价格仍优先。
新兼容别名包含 tiered、tiered-high、tiered-xhigh、sonar，但不将这些别名加入
标准模型预设，也不扩展旧模型的定价别名规则。未知变体须配置自己的价格。
Neon 为隐式缓存，无额外缓存写入/存储价；标准同步输出上限 65,536、统一上下文
1,048,576。Google 的 long decode 扩展另有接口要求，本次未宣称支持该模式。

同时添加内置目录价和计费兜底价。默认配置会把新镜像的资源价格目录补入旧下载
目录缺少的模型，后台默认价查询、模型同步和广场展示因此也能得到 Neon 价格。
已存在的远程价、自定义覆盖价及渠道/分组定价继续优先。

测试服验收发现远程价库给 Hydrogen 提供了 $1.5/百万 tokens 的缓存创建价
（Priority 为 $2.7），而 Google 明确该模型仅支持隐式缓存。解析远程默认价时，
因此清除 Neon/Hydrogen 的缓存创建、Priority 创建、1小时创建及存储价，随后仍按
原顺序应用管理员显式 override；其他模型和输入/输出/缓存读取价格不受影响。
此修正同时作用于后台展示和实际计费，避免只改资源文件却被旧下载价覆盖。

## 验证与部署

回归覆盖 Standard/Priority、缓存命中、分组倍率、旧/缺失价格缓存、自定义档位价、
未知模型拒绝、模型目录、实际转发请求参数、Codex 能力与路由边界，以及已有
GPT-6.2 缓存和 PRO 风控修复。前端白名单测试和类型检查一并验证。

没有数据库迁移。镜像构建及测试部署结果记录在本轮部署报告中；部署仅限
154.217.250.100:18081，不以本地桩测试冒充真实供应商生成验收。
