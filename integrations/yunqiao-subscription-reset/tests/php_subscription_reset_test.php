<?php
declare(strict_types=1);

// Isolated contract tests use the real purchase resolver; all remote calls and orders are in memory.
class OrderRepository
{
    public $rows = [];
    public function findOrderByRedeemCode(string $code): ?array { return $this->rows[$code] ?? null; }
    public function findSubscriptionBalanceRefundIntent(int $id, string $orderNo): ?array { return null; }
}
class SubscriptionRefundRepository
{
    public $bySub = [];
    public $byOrder = [];
    public function find(int $id): ?array { return $this->bySub[$id] ?? null; }
    public function findByOrderNo(string $orderNo): ?array { return $this->byOrder[$orderNo] ?? null; }
}
class Sub2APIClient
{
    public $sub;
    public $codes = [];
    public $credits = [];
    public $activation = null;
    public $calls = 0;
    public $lastRequest;
    public $loseResponse = false;
    public $failBeforeCommit = false;
    public $resetDuringRevoke = false;
    public $assignCalls = 0;
    public $history = null;
    public $historyPageSize = null;
    public $incompleteHistory = false;
    public function getAdminSubscription(array $settings, int $id): array { return $this->sub; }
    public function findRedeemCodeByCode(array $settings, string $code): ?array { return $this->codes[$code] ?? null; }
    public function findSubscriptionRefundCredit(array $settings, int $userId, string $code): ?array { return $this->credits[$code] ?? null; }
    public function findSubscriptionRenewalActivation(array $settings, int $id): ?array { return $this->activation; }
    public function findSubscriptionForRefundRecovery(array $settings, int $id, int $userId, int $groupId): ?array { return null; }
    public function listAdminSubscriptions(array $settings, int $page, int $pageSize, array $filters): array {
        if (($filters['status'] ?? '') === '' && $this->historyPageSize !== null) $pageSize = $this->historyPageSize;
        $rows = $this->history ?? [$this->sub];
        if (($filters['status'] ?? '') === 'active') { $rows = array_values(array_filter($rows, fn($row) => $row['status'] === 'active')); }
        return ['items' => $this->incompleteHistory && $page > 1 ? [] : array_slice($rows, ($page-1)*$pageSize, $pageSize), 'total' => count($rows), 'page' => $page, 'page_size' => $pageSize];
    }
    public function listUserSubscriptions(array $settings, int $userId): array { return array_values(array_filter($this->history ?? [$this->sub], fn($row) => $row['status'] === 'active')); }
    public function revokeSubscription(array $settings, int $id): array { if ($this->resetDuringRevoke) { $this->activation = ['old_subscription_id' => $id, 'new_subscription_id' => 200, 'current_code' => 'store_purchase_1']; } return []; }
    public function assignSubscription(array $settings, int $userId, int $groupId, int $days, string $notes): array { $this->assignCalls++; return []; }
    public function activateSubscriptionRenewal(array $settings, int $id, array $request): array
    {
        $this->calls++; $this->lastRequest = $request;
        if ($this->failBeforeCommit) { throw new RuntimeException('simulated transport failure'); }
        $days = (int) $this->codes[$request['next_code']]['validity_days'];
        $total = $days;
        foreach ($request['pending_codes'] as $code) { $total += (int) $this->codes[$code]['validity_days']; }
        $this->activation = ['old_subscription_id' => $id, 'new_subscription_id' => 200, 'current_code' => $request['next_code'],
            'pending_codes' => $request['pending_codes'], 'current_validity_days' => $days, 'total_validity_days' => $total,
            'starts_at' => gmdate('c'), 'expires_at' => gmdate('c', time() + $total * 86400), 'idempotency_key' => $request['idempotency_key'], 'replayed' => false];
        if ($this->loseResponse) { throw new RuntimeException('simulated lost response'); }
        return $this->activation;
    }
}
$root = $argv[1] ?? (dirname(__DIR__) . '/php-candidate');
require $root . '/src/Security.php';
require $root . '/src/SubscriptionMutationLock.php';
require $root . '/src/SubscriptionPurchaseService.php';
require $root . '/src/SubscriptionReplacementService.php';
require $root . '/src/SubscriptionResetService.php';
require $root . '/src/SubscriptionRefundService.php';
$testRoot = sys_get_temp_dir() . '/quya-reset-' . bin2hex(random_bytes(6));
mkdir($testRoot);
$passed = [];
function check(bool $condition, string $message): void { if (!$condition) { throw new RuntimeException($message); } }
function rejected(callable $fn, string $contains = ''): void
{
    try { $fn(); } catch (Throwable $e) { check($contains === '' || strpos($e->getMessage(), $contains) !== false, 'unexpected error: ' . $e->getMessage()); return; }
    throw new RuntimeException('expected rejection');
}
function fixture(array $days = [30, 30, 31], int $ageDays = 10): array
{
    global $testRoot;
    $orders = new OrderRepository(); $refunds = new SubscriptionRefundRepository(); $client = new Sub2APIClient();
    $start = time() - $ageDays * 86400;
    $codes = [];
    foreach ($days as $i => $day) {
        $code = 'store_purchase_' . $i; $codes[] = $code; $used = $start + $i * 3600;
        $orders->rows[$code] = ['order_no' => 'P_ORDER_' . $i, 'user_id' => 42, 'status' => 'paid', 'recharge_status' => 'success',
            'redeem_code' => $code, 'recharged_at' => gmdate('c', $used),
            'provider_payload' => json_encode(['product' => ['id' => 'monthly-pro', 'type' => 'subscription', 'group_id' => 7, 'validity_days' => $day]])];
        $client->codes[$code] = ['code' => $code, 'type' => 'subscription', 'status' => 'used', 'used_by' => 42, 'group_id' => 7, 'validity_days' => $day, 'used_at' => gmdate('c', $used)];
    }
    $client->sub = ['id' => 100, 'user_id' => 42, 'group_id' => 7, 'status' => 'active', 'starts_at' => gmdate('c', $start),
        'expires_at' => gmdate('c', $start + array_sum($days) * 86400), 'notes' => implode("\n", $codes),
        'monthly_usage_usd' => 98, 'group' => ['name' => '测试套餐', 'monthly_limit_usd' => 100]];
    $state = $testRoot . '/state-' . bin2hex(random_bytes(3)) . '.json';
    $service = new SubscriptionResetService($orders, $refunds, $client, 'isolated-test-secret', $state, $state . '.lock');
    return [$service, $orders, $refunds, $client, $state];
}
function test(string $name, callable $fn): void { global $passed; $fn(); $passed[] = $name; }

test('30-day next purchase retains 31-day queue and reveals no purchase codes', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100);
    check($q['current_validity_days'] === 30 && $q['total_validity_days'] === 61 && $q['queued_count'] === 1, 'wrong term');
    check($q['next_order_no'] === 'P_ORDER_1' && $q['queued_orders'][0]['order_no'] === 'P_ORDER_2', 'wrong FIFO');
    $s->verifyCsrf($q['csrf_token'], 42);
    $out = $s->execute([], 42, $q['quote_token']);
    check($out['new_subscription_id'] === 200 && $out['total_validity_days'] === 61 && !isset($out['current_code']), 'wrong receipt');
    check($c->lastRequest['pending_codes'] === ['store_purchase_2'], 'next included in queue');
});
test('50 remaining days resets to full purchased 30 days', function (): void {
    [$s] = fixture([30, 30], 10); $q = $s->preview([], 42, 100); $out = $s->execute([], 42, $q['quote_token']);
    check($out['total_validity_days'] === 30 && abs(strtotime($out['expires_at']) - time() - 30 * 86400) <= 2, 'fixed subtraction used');
});
test('31-day next order uses actual purchased duration', function (): void {
    [$s] = fixture([30, 31]); $q = $s->preview([], 42, 100); check($q['current_validity_days'] === 31, '31-day term lost');
});
test('exactly 31 days is denied', function (): void { [$s] = fixture([30, 30], 29); rejected(fn() => $s->preview([], 42, 100), '大于31'); });
test('different owner is denied', function (): void { [$s] = fixture(); rejected(fn() => $s->preview([], 41, 100), '当前账号'); });
test('same group different product cannot reset', function (): void {
    [$s, $o] = fixture(); $p = json_decode($o->rows['store_purchase_1']['provider_payload'], true); $p['product']['id'] = 'another-product';
    $o->rows['store_purchase_1']['provider_payload'] = json_encode($p); rejected(fn() => $s->preview([], 42, 100), '商品不一致');
});
test('unpaid or unfulfilled orders fail actual purchase resolver', function (): void {
    [$s, $o] = fixture(); $o->rows['store_purchase_1']['status'] = 'pending'; rejected(fn() => $s->preview([], 42, 100), '购买订单');
    [$s, $o] = fixture(); $o->rows['store_purchase_1']['recharge_status'] = 'pending'; rejected(fn() => $s->preview([], 42, 100), '购买订单');
});
test('wrong redemption owner and days are denied', function (): void {
    [$s, $o, $r, $c] = fixture(); $c->codes['store_purchase_1']['used_by'] = 99; rejected(fn() => $s->preview([], 42, 100), '购买订单');
    [$s, $o, $r, $c] = fixture(); $c->codes['store_purchase_1']['validity_days'] = 31; rejected(fn() => $s->preview([], 42, 100), '购买订单');
});
test('manual expiry extension is not inferred as paid purchase', function (): void {
    [$s, $o, $r, $c] = fixture([30]); $c->sub['expires_at'] = gmdate('c', time() + 60 * 86400); rejected(fn() => $s->preview([], 42, 100), '购买记录');
});
test('refund claim and remote credit both block reset', function (): void {
    [$s, $o, $r] = fixture(); $r->byOrder['P_ORDER_1'] = ['status' => 'prepared']; rejected(fn() => $s->preview([], 42, 100), '余额转换');
    [$s, $o, $r, $c] = fixture(); $c->credits['store_purchase_1'] = ['status' => 'unused']; rejected(fn() => $s->preview([], 42, 100), '余额转换');
});
test('existing cron pending operation blocks user reset', function (): void {
    [$s, $o, $r, $c, $state] = fixture(); file_put_contents($state, json_encode(['pending' => ['100' => ['old_subscription_id' => 100, 'user_id' => 42, 'group_id' => 7, 'validity_days' => 30, 'redeem_code' => 'store_purchase_1', 'notes' => 'pending']]]));
    rejected(fn() => $s->preview([], 42, 100), '正在处理');
});
test('snapshot change after preview is rejected before mutation', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100); $c->sub['notes'] .= "\n管理员备注更新";
    rejected(fn() => $s->execute([], 42, $q['quote_token']), '发生变化'); check($c->calls === 0, 'mutation occurred');
});
test('order state rechecked at execution', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100); $o->rows['store_purchase_1']['status'] = 'refunded';
    rejected(fn() => $s->execute([], 42, $q['quote_token']), '购买订单'); check($c->calls === 0, 'mutation occurred');
});
test('same operation double click returns receipt without second activation', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100); $a = $s->execute([], 42, $q['quote_token']); $b = $s->execute([], 42, $q['quote_token']);
    check($a === $b && $c->calls === 1, 'repeated activation');
});
test('lost response recovers committed receipt', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100); $c->loseResponse = true;
    $out = $s->execute([], 42, $q['quote_token']); check($out['new_subscription_id'] === 200 && $c->calls === 1, 'receipt not recovered');
});
test('unconfirmed response retains original operation for retry', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100); $c->failBeforeCommit = true;
    try { $s->execute([], 42, $q['quote_token']); throw new RuntimeException('missing pending'); } catch (SubscriptionResetPendingException $e) {}
    $key = $c->lastRequest['idempotency_key']; $c->failBeforeCommit = false;
    $s->execute([], 42, $q['quote_token']); check($c->lastRequest['idempotency_key'] === $key, 'retry key changed');
});
test('tampered quote and CSRF for another user rejected', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100);
    rejected(fn() => $s->execute([], 43, $q['quote_token']), '当前账号');
    rejected(fn() => $s->execute([], 42, 'x' . $q['quote_token']), '确认信息');
    rejected(fn() => $s->verifyCsrf($q['csrf_token'], 43), '验证'); check($c->calls === 0, 'unauthorized mutation');
});
test('different operation receipt cannot be claimed', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100); $s->execute([], 42, $q['quote_token']);
    $c->activation['idempotency_key'] = 'another-operation'; rejected(fn() => $s->execute([], 42, $q['quote_token']), '其他操作');
});
test('local mutation lock prevents race with purchase or refund', function (): void {
    [$s, $o, $r, $c, $state] = fixture(); $lock = SubscriptionMutationLock::acquire($state . '.lock');
    try { rejected(fn() => $s->preview([], 42, 100), '正在处理'); } finally { SubscriptionMutationLock::release($lock); }
    check($c->calls === 0, 'mutation ran under lock');
});
test('refund recovery never credits a subscription revoked by reset', function (): void {
    foreach (['prepared', 'revoking', 'revoked', 'crediting'] as $stage) {
        [$s, $o, $r, $c] = fixture(); $c->activation = ['current_code' => 'store_purchase_1'];
        $refund = new SubscriptionRefundService($o, $r, $c, 'isolated-test-secret');
        $method = new ReflectionMethod($refund, 'resumeLocked'); $method->setAccessible(true);
        rejected(fn() => $method->invoke($refund, [], ['status' => $stage, 'subscription_id' => 100]), '不能把该次撤销');
    }
});
test('cron notices concurrent reset after its revoke before assigning', function (): void {
    [$s, $o, $r, $c, $state] = fixture(); $c->resetDuringRevoke = true;
    $cron = new SubscriptionReplacementService($c, [], $state, 3.0, 200, 20, $o, $state . '.lock');
    $report = $cron->run(); check($c->assignCalls === 0 && count($report['errors']) === 1, 'cron assigned after reset');
    check(strpos($report['errors'][0]['message'], '自助重置') !== false, 'unexpected cron error');
});
test('interrupted cron recovers reset receipt without second assignment', function (): void {
    [$s, $o, $r, $c, $state] = fixture();
    file_put_contents($state, json_encode(['pending' => ['100' => ['old_subscription_id' => 100, 'user_id' => 42, 'group_id' => 7, 'validity_days' => 30, 'redeem_code' => 'store_purchase_1', 'notes' => 'pending']]]));
    $c->activation = ['old_subscription_id' => 100, 'new_subscription_id' => 200, 'current_code' => 'store_purchase_1'];
    $cron = new SubscriptionReplacementService($c, [], $state, 3.0, 200, 20, $o, $state . '.lock');
    $report = $cron->run(); check($c->assignCalls === 0 && $report['recovered'] === 1, 'pending cron repeated assignment');
    check(json_decode(file_get_contents($state), true)['pending'] === [], 'stale pending entry remains');
});
function historicalCron(array $alter = []): array
{
    [$s, $o, $r, $c, $state] = fixture();
    $pending = ['old_subscription_id' => 100, 'user_id' => 42, 'group_id' => 7, 'validity_days' => 30, 'assignment_days' => 61,
        'redeem_code' => 'store_purchase_1', 'notes' => SubscriptionPurchaseService::replacementNotes('store_purchase_1', ['store_purchase_2'], 100)];
    file_put_contents($state, json_encode(['pending' => ['100' => $pending]]));
    $old = $c->sub; $old['status'] = 'revoked'; $old['revoked_at'] = gmdate('c');
    $successor = array_replace($old, ['id' => 200, 'notes' => $pending['notes']], $alter);
    $later = array_replace($old, ['id' => 300, 'notes' => SubscriptionPurchaseService::replacementNotes('store_purchase_2', [], 200)]);
    $c->history = [$old, $successor, $later];
    $cron = new SubscriptionReplacementService($c, [], $state, 3.0, 200, 20, $o, $state . '.lock');
    return [$cron, $c, $state];
}
test('stale cron sees revoked successor after another-host reset and never reissues', function (): void {
    [$cron, $c, $state] = historicalCron(); $report = $cron->run();
    check($c->assignCalls === 0 && $report['recovered'] === 1 && !$report['errors'], 'revoked successor was reissued');
    check(json_decode(file_get_contents($state), true)['pending'] === [], 'completed pending not cleared');
});
test('stale cron recognizes legacy successor marker without explicit current line', function (): void {
    [$cron, $c] = historicalCron(['notes' => "通过兑换码 store_purchase_1 兑换\n套餐循环检查自动重开，原订阅ID 100"]);
    $report = $cron->run(); check($c->assignCalls === 0 && $report['recovered'] === 1, 'legacy successor not recognized');
});
test('purchase mentioned only in successor pending list is not a consumed current order', function (): void {
    [$cron, $c, $state] = historicalCron(['notes' => SubscriptionPurchaseService::replacementNotes('store_purchase_2', ['store_purchase_1'], 100)]);
    $report = $cron->run(); check($c->assignCalls === 0 && $report['recovered'] === 0 && count($report['errors']) === 1, 'queued code treated as current');
    check(isset(json_decode(file_get_contents($state), true)['pending']['100']), 'ambiguous pending removed');
});
test('restored old subscription plus historical successor is a conflict', function (): void {
    [$cron, $c] = historicalCron(); $c->history[0]['status'] = 'active'; unset($c->history[0]['revoked_at']);
    $report = $cron->run(); check($c->assignCalls === 0 && $report['recovered'] === 0 && count($report['errors']) >= 1, 'active parent conflict missed');
});
test('multiple successors with same parent and current purchase reject recovery', function (): void {
    [$cron, $c] = historicalCron(); $c->history[] = array_replace($c->history[1], ['id' => 201]);
    $report = $cron->run(); check($c->assignCalls === 0 && $report['recovered'] === 0 && count($report['errors']) === 1, 'duplicate successors accepted');
});
test('historical successor on later page is found', function (): void {
    [$cron, $c] = historicalCron(); $c->historyPageSize = 1;
    $report = $cron->run(); check($c->assignCalls === 0 && $report['recovered'] === 1 && !$report['errors'], 'later-page successor missed');
});
test('incomplete historical pagination fails closed', function (): void {
    [$cron, $c, $state] = historicalCron(); $c->historyPageSize = 1; $c->incompleteHistory = true;
    $report = $cron->run(); check($c->assignCalls === 0 && $report['recovered'] === 0 && count($report['errors']) === 1, 'incomplete history allowed assignment');
    check(isset(json_decode(file_get_contents($state), true)['pending']['100']), 'incomplete history removed pending');
});
test('duplicate subscription IDs across history pages fail closed', function (): void {
    [$cron, $c] = historicalCron(); $c->historyPageSize = 1; $c->history[2] = $c->history[1];
    $report = $cron->run(); check($c->assignCalls === 0 && $report['recovered'] === 0 && count($report['errors']) === 1, 'duplicate history accepted');
});
function expireQuote(string $token): string {
    $data = Security::parseSessionToken($token, 'isolated-test-secret'); $data['expires_at'] = time()-1;
    return Security::createSessionToken($data, 'isolated-test-secret');
}
test('expired confirmed operation can recover exact committed receipt', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100); $first = $s->execute([], 42, $q['quote_token']);
    $again = $s->execute([], 42, expireQuote($q['quote_token'])); check($again === $first && $c->calls === 1, 'expired receipt lookup repeated mutation');
});
test('expired confirmation without committed receipt cannot mutate', function (): void {
    [$s, $o, $r, $c] = fixture(); $q = $s->preview([], 42, 100);
    rejected(fn() => $s->execute([], 42, expireQuote($q['quote_token'])), '已过期'); check($c->calls === 0, 'expired confirmation mutated');
});
foreach (glob($testRoot . '/*') ?: [] as $file) { unlink($file); }
rmdir($testRoot);
echo json_encode(['passed' => count($passed), 'tests' => $passed], JSON_PRETTY_PRINT | JSON_UNESCAPED_UNICODE) . "\n";
