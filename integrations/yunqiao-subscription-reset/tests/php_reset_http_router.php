<?php
declare(strict_types=1);
$root = getenv('QUYA_RESET_TEST_ROOT');
require $root . '/src/Security.php';
require $root . '/src/App.php';
class Config {
    public static function sessionSecret(): string { return 'test-http-secret'; }
    public static function defaults(): array { return ['sub2api_base_url' => 'http://isolated.invalid']; }
    public static function merge(array $a, array $b): array { return array_merge($a, $b); }
}
class SubscriptionResetPendingException extends RuntimeException {}
class SubscriptionMutationBusyException extends RuntimeException {}
class ResetHttpRepo { public function getSettings(): array { return []; } }
class ResetHttpClient {
    public function verifyUserToken(string $base, string $token, int $userId): array {
        if ($token !== 'test-login-token' || $userId !== 42) throw new RuntimeException('invalid test token');
        return ['id' => 42, 'email' => 'test@example.invalid'];
    }
}
class ResetHttpService {
    public function verifyCsrf(string $token, int $userId): void { if ($token !== 'csrf-42' || $userId !== 42) throw new RuntimeException('invalid csrf'); }
    public function preview(array $settings, int $userId, int $id): array { return ['actor_id' => $userId, 'subscription_id' => $id]; }
    public function execute(array $settings, int $userId, string $token): array {
        if ($token === 'pending') throw new SubscriptionResetPendingException('pending');
        if ($token === 'busy') throw new SubscriptionMutationBusyException('busy');
        return ['actor_id' => $userId, 'quote_token' => $token, 'new_subscription_id' => 200];
    }
}
$app = (new ReflectionClass(App::class))->newInstanceWithoutConstructor();
foreach (['repo' => new ResetHttpRepo(), 'sub2api' => new ResetHttpClient(), 'subscriptionReset' => new ResetHttpService()] as $name => $value) {
    $p = new ReflectionProperty($app, $name); $p->setAccessible(true); $p->setValue($app, $value);
}
$path = parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH);
if (strpos($path, '/pay') === 0) $path = substr($path, 4);
$m = new ReflectionMethod($app, 'subscriptionResetHttp'); $m->setAccessible(true);
if (!$m->invoke($app, $_SERVER['REQUEST_METHOD'], $path)) { http_response_code(404); echo 'not found'; }
