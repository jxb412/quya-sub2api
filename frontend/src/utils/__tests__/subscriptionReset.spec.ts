import { describe, expect, it } from 'vitest'
import { canOpenSubscriptionReset, isSubscriptionResetCompletion, SUBSCRIPTION_RESET_MIN_REMAINING_MS, trustedSubscriptionResetUrl } from '../subscriptionReset'

describe('subscription reset eligibility', () => {
  const now = Date.parse('2026-11-24T12:00:00Z')
  it.each([-1, 0, 1])('uses a strict 31-day boundary (%i ms)', offset => {
    const expires_at = new Date(now + SUBSCRIPTION_RESET_MIN_REMAINING_MS + offset).toISOString()
    expect(canOpenSubscriptionReset({ status: 'active', expires_at }, now)).toBe(offset > 0)
  })
  it('rejects inactive, expired and malformed subscriptions', () => {
    expect(canOpenSubscriptionReset({ status: 'revoked', expires_at: '2030-01-01T00:00:00Z' }, now)).toBe(false)
    expect(canOpenSubscriptionReset({ status: 'active', expires_at: 'invalid' }, now)).toBe(false)
    expect(canOpenSubscriptionReset({ status: 'active', expires_at: '' }, now)).toBe(false)
  })
})

describe('subscription reset trusted destination', () => {
  it.each(['', 'http://store.example/reset', 'javascript:alert(1)', '//store.example/reset', '/reset', 'https://user:pass@store.example/reset', 'https://@store.example/reset', 'https://store.example/reset#redirect', 'https://store.example\\@evil.example/reset', 'https://store.example/re\nset'])('rejects %s', raw => {
    expect(trustedSubscriptionResetUrl(raw)).toBeNull()
  })
  it('accepts an administrator-configured HTTPS page', () => {
    expect(trustedSubscriptionResetUrl(' https://www.yunqiao.me/subscription-reset ')?.origin).toBe('https://www.yunqiao.me')
  })
})

describe('subscription reset completion messages', () => {
  const frame = {} as Window
  const data = { type: 'quya:subscription-reset-completed', old_subscription_id: 17, new_subscription_id: 18 }
  const message = (overrides: Partial<MessageEvent> = {}) => ({ source: frame, origin: 'https://www.yunqiao.me', data, ...overrides }) as MessageEvent
  it('requires the selected iframe, origin and subscription', () => {
    expect(isSubscriptionResetCompletion(message(), 'https://www.yunqiao.me', frame, 17)).toBe(true)
    for (const event of [message({ source: {} as Window }), message({ origin: 'https://evil.example' }), message({ data: { ...data, old_subscription_id: 19 } }), message({ data: { ...data, new_subscription_id: '18' } }), message({ data: { ...data, new_subscription_id: 17 } }), message({ data: { ...data, new_subscription_id: 0 } })]) {
      expect(isSubscriptionResetCompletion(event, 'https://www.yunqiao.me', frame, 17)).toBe(false)
    }
    expect(isSubscriptionResetCompletion(message({ source: null }), 'https://www.yunqiao.me', null, 17)).toBe(false)
  })
})
