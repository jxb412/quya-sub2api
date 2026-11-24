<?php

declare(strict_types=1);

class SubscriptionResetPendingException extends RuntimeException {}

/** 2026-11-24: consume a verified prepaid renewal without losing queued purchases. */
class SubscriptionResetService
{
    private $orders;
    private $refunds;
    private $client;
    private $secret;
    private $stateFile;
    private $lockPath;

    public function __construct(OrderRepository $orders, SubscriptionRefundRepository $refunds, Sub2APIClient $client, string $secret, string $stateFile, ?string $lockPath = null)
    {
        $this->orders = $orders;
        $this->refunds = $refunds;
        $this->client = $client;
        $this->secret = $secret;
        $this->stateFile = $stateFile;
        $this->lockPath = $lockPath;
    }

    public function csrf(int $userId): string
    {
        return Security::createSessionToken(['purpose' => 'subscription-reset-csrf', 'user_id' => $userId, 'expires_at' => time() + 7200], $this->secret);
    }

    public function verifyCsrf(string $token, int $userId): void
    {
        $data = Security::parseSessionToken($token, $this->secret);
        if ($data === null || ($data['purpose'] ?? '') !== 'subscription-reset-csrf' || (int) ($data['user_id'] ?? 0) !== $userId) {
            throw new RuntimeException('页面验证已过期，请刷新页面后重试');
        }
    }

    public function preview(array $settings, int $userId, int $subscriptionId): array
    {
        $lock = SubscriptionMutationLock::acquire($this->lockPath);
        try {
            $prepared = $this->prepare($settings, $userId, $subscriptionId);
            $request = $prepared['request'];
            $quote = $prepared['quote'];
            $quote['quote_token'] = Security::createSessionToken([
                'purpose' => 'subscription-reset-preview', 'user_id' => $userId,
                'subscription_id' => $subscriptionId, 'next_code' => $request['next_code'],
                'idempotency_key' => $request['idempotency_key'], 'request_hash' => $this->hashRequest($request),
                'expires_at' => time() + 600,
            ], $this->secret);
            $quote['csrf_token'] = $this->csrf($userId);
            return $quote;
        } finally { SubscriptionMutationLock::release($lock); }
    }

    public function execute(array $settings, int $userId, string $token): array
    {
        $data = Security::parseSessionToken($token, $this->secret);
        $expired = $data === null;
        if ($expired) { $data = $this->expiredReceiptToken($token); }
        if ($data === null || ($data['purpose'] ?? '') !== 'subscription-reset-preview' ||
            (int) ($data['user_id'] ?? 0) !== $userId || (int) ($data['subscription_id'] ?? 0) <= 0 ||
            !is_string($data['idempotency_key'] ?? null) || !is_string($data['next_code'] ?? null) ||
            !is_string($data['request_hash'] ?? null)) {
            throw new RuntimeException('确认信息已过期或不属于当前账号，请重新查看重置预览');
        }
        $id = (int) $data['subscription_id'];
        $lock = SubscriptionMutationLock::acquire($this->lockPath);
        try {
            // A lost response must recover this exact operation, never consume the next queued order.
            $existing = $this->client->findSubscriptionRenewalActivation($settings, $id);
            if ($existing !== null) { return $this->receiptForUser($existing, $data); }
            if ($expired) { throw new RuntimeException('确认信息已过期，且未查到本次重置成功记录，请重新查看重置预览'); }
            $prepared = $this->prepare($settings, $userId, $id);
            if (!hash_equals($data['request_hash'], $this->hashRequest($prepared['request']))) {
                throw new RuntimeException('套餐或续购订单发生变化，请重新查看重置预览');
            }
            try {
                $receipt = $this->client->activateSubscriptionRenewal($settings, $id, $prepared['request']);
            } catch (Throwable $error) {
                // Neither an HTTP error nor a timeout proves that the atomic transaction failed.
                try {
                    $receipt = $this->client->findSubscriptionRenewalActivation($settings, $id);
                    if ($receipt !== null) { return $this->receiptForUser($receipt, $data); }
                } catch (Throwable $lookupError) { /* Keep the signed operation available for retry. */ }
                throw new SubscriptionResetPendingException('本次重置结果尚未确认，请使用“核对并重试”继续查询同一次操作，勿重复购买', 0, $error);
            }
            return $this->receiptForUser($receipt, $data);
        } finally { SubscriptionMutationLock::release($lock); }
    }

    private function prepare(array $settings, int $userId, int $id): array
    {
        if ($userId <= 0 || $id <= 0) { throw new RuntimeException('请选择有效的订阅'); }
        $sub = $this->client->getAdminSubscription($settings, $id);
        if ((int) ($sub['id'] ?? 0) !== $id || (int) ($sub['user_id'] ?? 0) !== $userId) {
            throw new RuntimeException('未找到当前账号的订阅');
        }
        $expires = strtotime((string) ($sub['expires_at'] ?? ''));
        if (($sub['status'] ?? '') !== 'active' || !empty($sub['revoked_at']) || $expires === false || $expires <= time() + 31 * 86400) {
            throw new RuntimeException('仅剩余有效期大于31天的有效订阅可申请重置');
        }
        $groupId = (int) ($sub['group_id'] ?? 0);
        if (SubscriptionReplacementService::hasPendingReplacement($userId, $groupId, $this->stateFile)) {
            throw new SubscriptionMutationBusyException('该套餐正在处理续购，请稍后重新查看');
        }
        // PurchaseService validates paid + fulfilled orders, used redemption codes and legacy lineage.
        $sources = (new SubscriptionPurchaseService($this->orders, $this->client))->resolve($settings, $sub, true);
        if (!$sources['pending']) { throw new RuntimeException('没有可启用的同套餐续购订单'); }
        $all = array_merge([$sources['current']], $sources['pending']);
        $productId = $this->productId($sources['current']);
        foreach (array_merge([$id], $sources['lineage_subscription_ids']) as $sourceId) {
            if ($this->refunds->find((int) $sourceId) !== null) { throw new RuntimeException('该套餐存在余额转换记录，暂不可重置'); }
        }
        foreach ($all as $purchase) {
            if ($this->productId($purchase) !== $productId) { throw new RuntimeException('续购订单与当前套餐商品不一致，请联系客服核对'); }
            if (!in_array((int) $purchase['validity_days'], [30, 31], true)) { throw new RuntimeException('当前自助重置支持30天或31天套餐'); }
            $orderNo = (string) ($purchase['order']['order_no'] ?? '');
            if ($orderNo === '' || $this->refunds->findByOrderNo($orderNo) !== null ||
                $this->client->findSubscriptionRefundCredit($settings, $userId, $purchase['code']) !== null) {
                throw new RuntimeException('购买订单存在余额转换记录或来源不完整，暂不可重置');
            }
        }
        $pending = $sources['pending'];
        $next = $pending[0];
        $days = array_sum(array_column($pending, 'validity_days'));
        if ($days < 30 || $days > 36500) { throw new RuntimeException('续购有效期异常，请联系客服核对'); }
        $request = [
            'user_id' => $userId, 'group_id' => $groupId,
            'current_code' => $sources['current']['code'], 'next_code' => $next['code'],
            'pending_codes' => array_column(array_slice($pending, 1), 'code'),
            'expected_starts_at' => (string) $sub['starts_at'], 'expected_expires_at' => (string) $sub['expires_at'],
            'expected_notes' => (string) ($sub['notes'] ?? ''),
            'idempotency_key' => 'user-renewal-' . hash('sha256', $userId . ':' . $id . ':' . $next['code']),
        ];
        $currentEnd = min($expires, (int) strtotime((string) $sub['starts_at']) + (int) $sources['current']['validity_days'] * 86400);
        $group = is_array($sub['group'] ?? null) ? $sub['group'] : [];
        $quotas = [];
        foreach (['daily' => '日额度', 'weekly' => '周额度', 'monthly' => '套餐额度'] as $window => $label) {
            $limit = $group[$window . '_limit_usd'] ?? null;
            if (!is_numeric($limit) || (float) $limit <= 0) { continue; }
            $used = max(0, (float) ($sub[$window . '_usage_usd'] ?? 0));
            $quotas[] = ['label' => $label, 'limit' => (float) $limit, 'used' => $used, 'remaining' => max(0, (float) $limit - $used)];
        }
        return ['request' => $request, 'quote' => [
            'subscription_id' => $id, 'group_name' => (string) ($group['name'] ?? '订阅套餐'),
            'current_order_no' => (string) $sources['current']['order']['order_no'],
            'next_order_no' => (string) $next['order']['order_no'],
            'old_expires_at' => $sub['expires_at'], 'current_remaining_seconds' => max(0, $currentEnd - time()),
            'current_validity_days' => (int) $next['validity_days'], 'total_validity_days' => $days,
            'new_expires_at_estimate' => gmdate('c', time() + $days * 86400),
            'queued_count' => count($pending) - 1,
            'queued_orders' => array_map(static function (array $row): array { return ['order_no' => (string) $row['order']['order_no'], 'validity_days' => (int) $row['validity_days']]; }, array_slice($pending, 1)),
            'quotas' => $quotas,
        ]];
    }

    private function productId(array $purchase): string
    {
        $payload = json_decode((string) ($purchase['order']['provider_payload'] ?? ''), true);
        $productId = $payload['product']['id'] ?? null;
        if ((!is_string($productId) && !is_int($productId)) || trim((string) $productId) === '') {
            throw new RuntimeException('订单缺少可核对的套餐商品信息，请联系客服处理');
        }
        return (string) $productId;
    }

    private function hashRequest(array $request): string
    {
        return hash('sha256', json_encode($request, JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE | JSON_THROW_ON_ERROR));
    }

    // 2026-11-24: expired confirmations may recover an already-committed receipt only.
    // This deliberately never renews a preview's authority to perform a new mutation.
    private function expiredReceiptToken(string $token): ?array
    {
        if (strlen($token) > 12000) { return null; }
        $parts = explode('.', $token, 2);
        if (count($parts) !== 2 || !hash_equals(hash_hmac('sha256', $parts[0], $this->secret), $parts[1])) { return null; }
        $json = base64_decode(strtr($parts[0], '-_', '+/'), true);
        $data = $json === false ? null : json_decode($json, true);
        if (!is_array($data) || !is_int($data['expires_at'] ?? null) || $data['expires_at'] >= time()) { return null; }
        return $data;
    }

    private function receiptForUser(array $receipt, array $data): array
    {
        if ((int) ($receipt['old_subscription_id'] ?? 0) !== (int) $data['subscription_id'] ||
            (int) ($receipt['new_subscription_id'] ?? 0) <= 0 ||
            ($receipt['current_code'] ?? '') !== $data['next_code'] ||
            !hash_equals($data['idempotency_key'], (string) ($receipt['idempotency_key'] ?? '')) ||
            !in_array((int) ($receipt['current_validity_days'] ?? 0), [30, 31], true) ||
            strtotime((string) ($receipt['expires_at'] ?? '')) === false) {
            throw new RuntimeException('该订阅已被其他操作处理，请刷新订阅页面核对');
        }
        return array_intersect_key($receipt, array_flip(['old_subscription_id', 'new_subscription_id', 'current_validity_days', 'total_validity_days', 'starts_at', 'expires_at', 'replayed']));
    }
}
