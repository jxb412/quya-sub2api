# 商城自助重置配套（2026-11-24）

此目录是 Quya 用户订阅页所需的 PHP 商城配套，不是独立商城。生产站点 PHP 未包含在当前 Go/Vue 仓库内，因此保留最小补丁与新文件，避免把商城环境配置、数据库或其他业务文件复制进仓库。

## 内容

- `php-existing-files.patch`：修改现有 `bootstrap.php`、`src/App.php`、`src/Sub2APIClient.php`、`src/SubscriptionRefundService.php`、`src/SubscriptionReplacementService.php`。
- `new-files/`：新增来源订单核验服务和客户确认页面。
- `php-change-manifest.json`：实际商城基线与候选文件 SHA-256；不含凭据。
- `tests/`：使用模拟订单与上游的独立业务、HTTP验收，不连接生产数据。

## 行为

入口 `/subscription-reset?subscription_id=123`。沿用商城已有登录验证，核对已付款、成功履约、同用户、同商品与同订阅分组的购买记录；只支持当前和待启用订单均为30/31天且订阅剩余严格大于31天。记录不完整、已退款或来源存在歧义时交由管理员核对。

页面展示当前订单、下一份订单、实际周期、排队份数、重置前后到期时间及将放弃的剩余额度。只有客户明确确认才提交。签名预览绑定原订阅和操作，CSRF防护与重新核验在服务端执行；超时重试不会换成下一笔续购。

后端在 PostgreSQL 一个事务内完成替换和回执。退款及旧自动重开增加回执检查，避免把另一个流程已完成的重置当成自己的撤销或再次发放。

## 发布顺序

1. 先在隔离环境验证迁移，再于发布维护窗口对共享数据库执行 `253_quya_subscription_renewal_activations.sql`，由现有迁移流程登记 `schema_migrations.filename` 和该发布文件的 SHA-256。核对表结构及登记值后，才让新版 Go 接收业务流量；新版普通订阅计费、撤销也依赖该表，不能只建表而不登记，或只在测试库执行。共享数据库只迁移一次，完成后恢复 `DATABASE_INITIALIZATION_ENABLED=false`；具体一次性迁移流程见 [部署说明](../../deploy/README.md#包含数据库迁移的更新)。随后升级两台 Go，确认两台运行同一新版，并排空所有旧版请求和计费 worker/队列，再进入 PHP 发布步骤。保留 `subscription_maintenance.auto_reset_enabled=false`；启用自动额度周期的实例会拒绝新的自助重置。
2. 复制当前 PHP 网站到独立候选目录，逐文件核对 manifest 的基线 SHA。线上文件若已变化，应人工合并局部补丁，不能覆盖其他更新。
3. 在候选目录执行 `git -c core.autocrlf=false apply --check /path/php-existing-files.patch`；检查通过后去掉 `--check` 应用补丁，再将 `new-files/` 中两个文件按相对路径复制进去，并核对 manifest 的候选 SHA。显式关闭换行转换，确保 Windows 和 Linux 都保持原始文件字节。未通过不得强制覆盖。
4. 用 PHP7.4或当前站点版本执行语法检查、下述测试；备份实际将替换的 PHP 文件。确认第1步两台 Go 升级及旧 worker 排空均完成后，再按现有发布流程安装 PHP 新路由与退款/旧任务恢复补丁。
5. 以管理员设置 `subscription_reset_url=https://www.yunqiao.me/subscription-reset` 开放入口。留空时用户页不显示按钮。

`subscription_reset_url` 留空只隐藏用户页按钮，不是服务端停用开关；已上线的 PHP 页面及 API 仍可被直接访问。PHP 重置路由不能在两台 Go 和旧 worker 完成切换前开放，否则请求可能在新版完成重置后，仍由旧版按用户/分组把旧期费用加入新期缓存。需要临时停用时，应同时在服务器阻断该页面及 `/api/user/subscription-reset/preview`、`/api/user/subscription-reset/execute`（包括 `/pay` 挂载路径）。

不能先装PHP补丁再运行旧Go：新回执查询接口缺失时，退款和自动任务会拒绝继续，以免重复发放。回滚先停用服务端重置路由并清空入口设置；若已经完成过重置，不能直接恢复缺少回执保护的旧 Go/PHP 退款、自动重开或计费代码，应保留这些兼容修复。已完成重置的回执和订阅来源链应保留，不删除回执表。

## 本地验收

```sh
php tests/php_subscription_reset_test.php /path/to/php-candidate
python tests/php_reset_http_test.py /path/to/php /path/to/php-candidate
```

业务测试直接复用真实 `SubscriptionPurchaseService`，订单与网关为内存测试对象；HTTP测试启动临时本地服务。覆盖31天边界、多份30/31天、错误商品/用户/订单状态、重复点击、结果丢失后恢复、登录跳转保留参数、CSRF以及退款/旧任务回执保护。

完整候选部署前仍应验证真实商城登录、跨站嵌入和订单预览；不要使用客户订单做破坏性的重置验收。
