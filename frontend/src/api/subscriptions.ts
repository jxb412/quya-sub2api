/**
 * User Subscription API
 * API for regular users to view their own subscriptions and progress
 */

import { apiClient } from './client'
import type { UserSubscription, SubscriptionProgress } from '@/types'

// 2026-11-24: the native flow uses issued subscription records; no storefront
// credentials or raw redemption codes are sent to the browser.
interface SubscriptionResetPreviewBase {
  subscription_id: number
  group_id: number
  group_name: string
  old_expires_at: string
  reason_code?: string
  reason?: string
}

export type SubscriptionResetPreview = SubscriptionResetPreviewBase & (
  { eligible: false } | {
    eligible: true
    current_order_no: string
    next_order_no: string
    current_validity_days: number
    next_validity_days: number
    pending_count: number
    remaining_pending_count: number
    total_validity_days: number
    new_expires_at: string
    discarded_remaining_seconds: number
    preview_token: string
  }
)

export interface SubscriptionResetResult {
  old_subscription_id: number
  new_subscription_id: number
  starts_at: string
  expires_at: string
  current_validity_days: number
  total_validity_days: number
  pending_count: number
  replayed: boolean
}

export async function getSubscriptionResetPreview(subscriptionId: number): Promise<SubscriptionResetPreview> {
  const { data } = await apiClient.get<SubscriptionResetPreview>(`/subscriptions/${subscriptionId}/reset-preview`)
  return data
}

export async function resetSubscription(subscriptionId: number, previewToken: string): Promise<SubscriptionResetResult> {
  const { data } = await apiClient.post<SubscriptionResetResult>(`/subscriptions/${subscriptionId}/reset`, {
    preview_token: previewToken,
    accepted_rules: true
  })
  return data
}

/**
 * Subscription summary for user dashboard
 */
export interface SubscriptionSummary {
  active_count: number
  subscriptions: Array<{
    id: number
    group_name: string
    status: string
    daily_progress: number | null
    weekly_progress: number | null
    monthly_progress: number | null
    expires_at: string | null
    days_remaining: number | null
  }>
}

/**
 * Get list of current user's subscriptions
 */
export async function getMySubscriptions(): Promise<UserSubscription[]> {
  const response = await apiClient.get<UserSubscription[]>('/subscriptions')
  return response.data
}

/**
 * Get current user's active subscriptions
 */
export async function getActiveSubscriptions(): Promise<UserSubscription[]> {
  const response = await apiClient.get<UserSubscription[]>('/subscriptions/active')
  return response.data
}

/**
 * Get progress for all user's active subscriptions
 */
export async function getSubscriptionsProgress(): Promise<SubscriptionProgress[]> {
  const response = await apiClient.get<SubscriptionProgress[]>('/subscriptions/progress')
  return response.data
}

/**
 * Get subscription summary for dashboard display
 */
export async function getSubscriptionSummary(): Promise<SubscriptionSummary> {
  const response = await apiClient.get<SubscriptionSummary>('/subscriptions/summary')
  return response.data
}

/**
 * Get progress for a specific subscription
 */
export async function getSubscriptionProgress(
  subscriptionId: number
): Promise<SubscriptionProgress> {
  const response = await apiClient.get<SubscriptionProgress>(
    `/subscriptions/${subscriptionId}/progress`
  )
  return response.data
}

export default {
  getMySubscriptions,
  getActiveSubscriptions,
  getSubscriptionsProgress,
  getSubscriptionSummary,
  getSubscriptionProgress,
  getSubscriptionResetPreview,
  resetSubscription
}
