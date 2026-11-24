import { describe, expect, it } from 'vitest'
import { canOpenSubscriptionReset, SUBSCRIPTION_RESET_MIN_REMAINING_MS } from '../subscriptionReset'

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
