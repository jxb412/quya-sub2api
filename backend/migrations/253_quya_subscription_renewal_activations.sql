-- 2026-11-24 Quya: persist one-shot paid renewal activations. The PHP store
-- verifies payment; this receipt serializes subscription replacement across
-- both application hosts and keeps late billing attached to the old period.
CREATE TABLE IF NOT EXISTS subscription_renewal_activations (
    old_subscription_id BIGINT PRIMARY KEY REFERENCES user_subscriptions(id),
    new_subscription_id BIGINT NOT NULL UNIQUE REFERENCES user_subscriptions(id),
    user_id BIGINT NOT NULL REFERENCES users(id),
    group_id BIGINT NOT NULL REFERENCES groups(id),
    next_code VARCHAR(32) NOT NULL UNIQUE,
    idempotency_key VARCHAR(160) NOT NULL UNIQUE,
    request_hash VARCHAR(64) NOT NULL,
    receipt JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
