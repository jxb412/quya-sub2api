import type { UserSubscription } from '@/types'

// 2026-11-24: this is only an entry hint. The server validates the member's
// issued renewals and their unused status again before atomic activation.
export const SUBSCRIPTION_RESET_MIN_REMAINING_MS = 31 * 24 * 60 * 60 * 1000

export function canOpenSubscriptionReset(subscription: Pick<UserSubscription, 'status' | 'expires_at'>, now = Date.now()): boolean {
  if (subscription.status !== 'active' || !subscription.expires_at) return false
  const expires = Date.parse(subscription.expires_at)
  return Number.isFinite(expires) && expires - now > SUBSCRIPTION_RESET_MIN_REMAINING_MS
}
