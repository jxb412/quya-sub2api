import type { UserSubscription } from '@/types'

// 2026-11-24: reset eligibility is only an entry hint. The trusted storefront
// verifies paid renewals and confirms the destructive change before execution.
export const SUBSCRIPTION_RESET_MIN_REMAINING_MS = 31 * 24 * 60 * 60 * 1000

export function canOpenSubscriptionReset(subscription: Pick<UserSubscription, 'status' | 'expires_at'>, now = Date.now()): boolean {
  if (subscription.status !== 'active' || !subscription.expires_at) return false
  const expires = Date.parse(subscription.expires_at)
  return Number.isFinite(expires) && expires - now > SUBSCRIPTION_RESET_MIN_REMAINING_MS
}

// This URL must come from public settings written by an administrator, never
// from a user-controlled route query. Login credentials go only to this origin.
export function trustedSubscriptionResetUrl(raw: string | undefined): URL | null {
  const value = raw?.trim() || ''
  if (!value || value.length > 2048 || !value.startsWith('https://') || value.includes('\\')) return null
  if (Array.from(value).some(char => {
    const code = char.charCodeAt(0)
    return code < 32 || (code >= 127 && code <= 159)
  })) return null
  if (value.slice('https://'.length).split(/[/?#]/, 1)[0]?.includes('@')) return null
  try {
    const url = new URL(value)
    if (url.protocol !== 'https:' || !url.hostname || url.username || url.password || url.hash) return null
    return url
  } catch {
    return null
  }
}

export function isSubscriptionResetCompletion(
  event: MessageEvent,
  expectedOrigin: string,
  expectedWindow: Window | null | undefined,
  oldSubscriptionID: number,
): boolean {
  if (!expectedWindow || event.source !== expectedWindow || event.origin !== expectedOrigin) return false
  const data: unknown = event.data
  if (!data || typeof data !== 'object') return false
  const message = data as Record<string, unknown>
  return message.type === 'quya:subscription-reset-completed'
    && message.old_subscription_id === oldSubscriptionID
    && typeof message.new_subscription_id === 'number'
    && Number.isSafeInteger(message.new_subscription_id)
    && message.new_subscription_id > 0
    && message.new_subscription_id !== oldSubscriptionID
}
