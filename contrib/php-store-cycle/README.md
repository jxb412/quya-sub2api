# PHP商城自动重开兼容修复（2026-11-26）

原来的PHP任务先撤销旧套餐、再创建新套餐。新增的Sub2API购买来源校验拒绝某些合法旧历史记录时，前一请求的撤销已经提交。本修复同时兼容旧历史，并将循环任务切换为一个数据库事务。

## Sub2API

新增管理员接口 `POST /api/v1/admin/subscriptions/:id/replace-store`。请求包含 `user_id`、`group_id`、`validity_days`、新套餐 `notes`，以及准备任务时原套餐的 `expected_starts_at`、`expected_expires_at`、`expected_notes`。三项快照必须完整，原记录变化时拒绝，不覆盖新的购买或人工修改。

响应 `data` 包含 `old_subscription_id`、`subscription` 和 `replayed`。首次成功返回新套餐；网络丢响应后的重试返回相同套餐，不重置其已用额度；后继已经撤销也不会重新发放。管理员自定义Key须显式获得 `subscriptions.replace` 权限；用户自助重置开关和剩余31天门槛不影响该管理员商城接口。

撤销、购买校验、创建和持久回执在同一事务中，失败全部回滚。复用迁移253的 `subscription_renewal_activations` 表，不新增数据库迁移。已手动开同组新套餐的客户拒绝重复发放。旧版pending若原套餐已撤销，仍可验证购买来源并安全恢复；不要批量清空pending或重开已处理客户。

## PHP配套补丁

`atomic-replacement.patch` 对应既有商城的三个文件，基线和结果SHA见 `php-change-manifest.json`。应用前必须核对线上文件与基线一致，保留文件备份，使用PHP7.4或更高版本做语法检查。此补丁不包含站点配置、账号或密钥，不更改PHP数据库表。

PHP持久化原套餐快照，改为一次调用上述原子接口。只有核对成功回执后才移除pending；404或错误绝不降级到先撤销再分配。原有订单和退款核对继续生效；部分任务失败时入口返回HTTP503及`partial_failure`，保留成功统计和待重试记录。

必须先部署支持原子接口的Sub2API，验证后再替换PHP文件。回滚PHP到旧版本会重新引入两请求风险，不应作为常规恢复手段；可先暂停该循环任务，而不是修改用户订阅。

## 验证

新增Go单元与真实PostgreSQL事务回归，覆盖合法旧多兑换行、歧义拒绝、手动订阅保护、原子回滚、重复请求、旧pending恢复、退款/原生重置/管理员并发及晚到计费。镜像工作流执行这些测试。

PHP候选通过PHP7.4语法、13组业务及7组入口状态回归；部署前还应在154隔离测试环境做真实PHP→Sub2API联合验收。不能以单元测试代替线上版本验收。
