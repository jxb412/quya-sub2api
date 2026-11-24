<template>
  <AppLayout>
    <div class="space-y-6">
      <!-- Loading State -->
      <div v-if="loading" class="flex justify-center py-12">
        <div
          class="h-8 w-8 animate-spin rounded-full border-2 border-primary-500 border-t-transparent"
        ></div>
      </div>

      <!-- Empty State -->
      <div v-else-if="subscriptions.length === 0" class="card p-12 text-center">
        <div
          class="mx-auto mb-4 flex h-16 w-16 items-center justify-center rounded-full bg-gray-100 dark:bg-dark-700"
        >
          <Icon name="creditCard" size="xl" class="text-gray-400" />
        </div>
        <h3 class="mb-2 text-lg font-semibold text-gray-900 dark:text-white">
          {{ t('userSubscriptions.noActiveSubscriptions') }}
        </h3>
        <p class="text-gray-500 dark:text-dark-400">
          {{ t('userSubscriptions.noActiveSubscriptionsDesc') }}
        </p>
      </div>

      <!-- Subscriptions Grid -->
      <div v-else class="grid gap-6 lg:grid-cols-2">
        <div
          v-for="subscription in subscriptions"
          :key="subscription.id"
          class="overflow-hidden rounded-2xl border bg-white dark:bg-dark-800"
          :class="platformBorderClass(subscription.group?.platform || '')"
        >
          <!-- Header -->
          <div
            class="flex items-center justify-between border-b border-gray-100 p-4 dark:border-dark-700"
          >
            <div class="flex items-center gap-3">
              <div :class="['h-1.5 w-1.5 shrink-0 rounded-full', platformAccentDotClass(subscription.group?.platform || '')]" />
              <div>
                <div class="flex items-center gap-2">
                  <h3 class="font-semibold text-gray-900 dark:text-white">
                    {{ subscription.group?.name || `Group #${subscription.group_id}` }}
                  </h3>
                  <span :class="['rounded-md border px-2 py-0.5 text-[11px] font-medium', platformBadgeClass(subscription.group?.platform || '')]">
                    {{ platformLabel(subscription.group?.platform || '') }}
                  </span>
                </div>
                <p v-if="subscription.group?.description" class="mt-0.5 text-xs text-gray-500 dark:text-dark-400">
                  {{ subscription.group.description }}
                </p>
                <div class="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-[11px] text-gray-400 dark:text-gray-500">
                  <span>{{ t('payment.planCard.rate') }}: ×{{ subscription.group?.rate_multiplier ?? 1 }}</span>
                  <span v-if="subscriptionHasPeakRate(subscription)" class="text-amber-700 dark:text-amber-300">
                    {{ t('payment.planCard.peakRate') }}: {{ subscriptionPeakRateLabel(subscription) }}
                  </span>
                </div>
              </div>
            </div>
            <div class="flex flex-wrap items-center gap-2">
              <span
                :class="[
                  'rounded-full px-2 py-0.5 text-xs font-medium',
                  subscription.status === 'active'
                    ? 'bg-emerald-100 text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300'
                    : subscription.status === 'expired'
                      ? 'bg-gray-100 text-gray-600 dark:bg-dark-700 dark:text-gray-400'
                      : 'bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300'
                ]"
              >
                {{ t(`userSubscriptions.status.${subscription.status}`) }}
              </span>
              <button
                v-if="resetEnabled && canOpenSubscriptionReset(subscription, currentTime)"
                type="button"
                class="btn btn-secondary px-3 py-1.5 text-xs"
                :data-reset-subscription-id="subscription.id"
                :disabled="resetSubmitting"
                @click="openSubscriptionReset(subscription)"
              >
                {{ t('userSubscriptions.resetSubscription') }}
              </button>
              <button
                v-if="subscription.status === 'active'"
                :class="['rounded-lg px-3 py-1.5 text-xs font-semibold text-white transition-colors', platformButtonClass(subscription.group?.platform || '')]"
                @click="router.push({ path: '/purchase', query: { tab: 'subscription', group: String(subscription.group_id) } })"
              >
                {{ t('payment.renewNow') }}
              </button>
            </div>
          </div>

          <!-- Usage Progress -->
          <div class="space-y-4 p-4">
            <!-- Expiration Info -->
            <div v-if="subscription.expires_at" class="flex items-center justify-between text-sm">
              <span class="text-gray-500 dark:text-dark-400">{{
                t('userSubscriptions.expires')
              }}</span>
              <span :class="getExpirationClass(subscription.expires_at)">
                {{ formatExpirationDate(subscription.expires_at) }}
              </span>
            </div>
            <div v-else class="flex items-center justify-between text-sm">
              <span class="text-gray-500 dark:text-dark-400">{{
                t('userSubscriptions.expires')
              }}</span>
              <span class="text-gray-700 dark:text-gray-300">{{
                t('userSubscriptions.noExpiration')
              }}</span>
            </div>

            <!-- Daily Usage -->
            <div v-if="subscription.group?.daily_limit_usd" class="space-y-2">
              <div class="flex items-center justify-between">
                <span class="text-sm font-medium text-gray-700 dark:text-gray-300">
                  {{ t('userSubscriptions.daily') }}
                </span>
                <span class="text-sm text-gray-500 dark:text-dark-400">
                  ${{ (subscription.daily_usage_usd || 0).toFixed(2) }} / ${{
                    subscription.group.daily_limit_usd.toFixed(2)
                  }}
                </span>
              </div>
              <div class="relative h-2 overflow-hidden rounded-full bg-gray-200 dark:bg-dark-600">
                <div
                  class="absolute inset-y-0 left-0 rounded-full transition-all duration-300"
                  :class="
                    getProgressBarClass(
                      subscription.daily_usage_usd,
                      subscription.group.daily_limit_usd
                    )
                  "
                  :style="{
                    width: getProgressWidth(
                      subscription.daily_usage_usd,
                      subscription.group.daily_limit_usd
                    )
                  }"
                ></div>
              </div>
              <p
                v-if="subscription.daily_window_start"
                class="text-xs text-gray-500 dark:text-dark-400"
              >
                {{ formatDailyUsageWindow(subscription) }}
              </p>
            </div>

            <!-- Weekly Usage -->
            <div v-if="subscription.group?.weekly_limit_usd" class="space-y-2">
              <div class="flex items-center justify-between">
                <span class="text-sm font-medium text-gray-700 dark:text-gray-300">
                  {{ t('userSubscriptions.weekly') }}
                </span>
                <span class="text-sm text-gray-500 dark:text-dark-400">
                  ${{ (subscription.weekly_usage_usd || 0).toFixed(2) }} / ${{
                    subscription.group.weekly_limit_usd.toFixed(2)
                  }}
                </span>
              </div>
              <div class="relative h-2 overflow-hidden rounded-full bg-gray-200 dark:bg-dark-600">
                <div
                  class="absolute inset-y-0 left-0 rounded-full transition-all duration-300"
                  :class="
                    getProgressBarClass(
                      subscription.weekly_usage_usd,
                      subscription.group.weekly_limit_usd
                    )
                  "
                  :style="{
                    width: getProgressWidth(
                      subscription.weekly_usage_usd,
                      subscription.group.weekly_limit_usd
                    )
                  }"
                ></div>
              </div>
              <p
                v-if="subscription.weekly_window_start"
                class="text-xs text-gray-500 dark:text-dark-400"
              >
                {{
                  t('userSubscriptions.resetIn', {
                    time: formatResetTime(subscription.weekly_window_start, 168)
                  })
                }}
              </p>
            </div>

            <!-- Monthly Usage -->
            <div v-if="subscription.group?.monthly_limit_usd" class="space-y-2">
              <div class="flex items-center justify-between">
                <span class="text-sm font-medium text-gray-700 dark:text-gray-300">
                  {{ t('userSubscriptions.monthly') }}
                </span>
                <span class="text-sm text-gray-500 dark:text-dark-400">
                  ${{ (subscription.monthly_usage_usd || 0).toFixed(2) }} / ${{
                    subscription.group.monthly_limit_usd.toFixed(2)
                  }}
                </span>
              </div>
              <div class="relative h-2 overflow-hidden rounded-full bg-gray-200 dark:bg-dark-600">
                <div
                  class="absolute inset-y-0 left-0 rounded-full transition-all duration-300"
                  :class="
                    getProgressBarClass(
                      subscription.monthly_usage_usd,
                      subscription.group.monthly_limit_usd
                    )
                  "
                  :style="{
                    width: getProgressWidth(
                      subscription.monthly_usage_usd,
                      subscription.group.monthly_limit_usd
                    )
                  }"
                ></div>
              </div>
              <p
                v-if="subscription.monthly_window_start"
                class="text-xs text-gray-500 dark:text-dark-400"
              >
                {{
                  t('userSubscriptions.resetIn', {
                    time: formatResetTime(subscription.monthly_window_start, 720)
                  })
                }}
              </p>
            </div>

            <!-- No limits configured - Unlimited badge -->
            <div
              v-if="
                !subscription.group?.daily_limit_usd &&
                !subscription.group?.weekly_limit_usd &&
                !subscription.group?.monthly_limit_usd
              "
              class="flex items-center justify-center rounded-xl bg-gradient-to-r from-emerald-50 to-teal-50 py-6 dark:from-emerald-900/20 dark:to-teal-900/20"
            >
              <div class="flex items-center gap-3">
                <span class="text-4xl text-emerald-600 dark:text-emerald-400">∞</span>
                <div>
                  <p class="text-sm font-medium text-emerald-700 dark:text-emerald-300">
                    {{ t('userSubscriptions.unlimited') }}
                  </p>
                  <p class="text-xs text-emerald-600/70 dark:text-emerald-400/70">
                    {{ t('userSubscriptions.unlimitedDesc') }}
                  </p>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
    <BaseDialog
      :show="!!resetTarget"
      :title="t('userSubscriptions.resetSubscription')"
      width="normal"
      :close-on-escape="!resetSubmitting"
      :show-close-button="!resetSubmitting"
      @close="closeSubscriptionReset"
    >
      <div class="space-y-4" aria-live="polite" :aria-busy="resetLoading || resetSubmitting">
        <p v-if="resetLoading" class="py-6 text-center text-sm text-gray-500 dark:text-gray-400">
          {{ t('userSubscriptions.resetLoading') }}
        </p>
        <template v-else-if="resetPreview?.eligible">
          <p class="text-sm text-gray-600 dark:text-gray-300">
            {{ t('userSubscriptions.resetDescription') }}
          </p>
          <dl class="space-y-3 rounded-xl bg-gray-50 p-4 text-sm dark:bg-dark-900">
            <div class="flex justify-between gap-4">
              <dt class="text-gray-500 dark:text-gray-400">{{ t('userSubscriptions.resetPlan') }}</dt>
              <dd class="text-right font-medium">{{ resetPreview.group_name }}</dd>
            </div>
            <div class="flex justify-between gap-4">
              <dt class="shrink-0 text-gray-500 dark:text-gray-400">{{ t('userSubscriptions.resetCurrentOrder') }}</dt>
              <dd class="break-all text-right" data-reset-current-order>{{ resetPreview.current_order_no }}</dd>
            </div>
            <div class="flex justify-between gap-4">
              <dt class="shrink-0 text-gray-500 dark:text-gray-400">{{ t('userSubscriptions.resetNextOrder') }}</dt>
              <dd class="break-all text-right font-medium" data-reset-next-order>{{ resetPreview.next_order_no }}</dd>
            </div>
            <div class="flex justify-between gap-4">
              <dt class="text-gray-500 dark:text-gray-400">{{ t('userSubscriptions.resetNewPeriod') }}</dt>
              <dd class="text-right font-medium">{{ t('userSubscriptions.resetFullDays', { days: resetPreview.next_validity_days }) }}</dd>
            </div>
            <div class="flex justify-between gap-4">
              <dt class="text-gray-500 dark:text-gray-400">{{ t('userSubscriptions.resetQueuedOrders') }}</dt>
              <dd class="text-right">{{ t('userSubscriptions.resetQueuedCount', { count: resetPreview.remaining_pending_count }) }}</dd>
            </div>
            <div class="flex justify-between gap-4">
              <dt class="text-gray-500 dark:text-gray-400">{{ t('userSubscriptions.resetOldExpiry') }}</dt>
              <dd class="text-right">{{ formatResetDate(resetPreview.old_expires_at) }}</dd>
            </div>
            <div class="flex justify-between gap-4">
              <dt class="text-gray-500 dark:text-gray-400">{{ t('userSubscriptions.resetNewExpiry') }}</dt>
              <dd class="text-right font-medium">{{ formatResetDate(resetPreview.new_expires_at) }}</dd>
            </div>
          </dl>
          <p class="text-xs text-gray-500 dark:text-gray-400">
            {{ t('userSubscriptions.resetExpiryHint', { days: resetPreview.total_validity_days }) }}
          </p>
          <div class="space-y-2 rounded-xl border border-amber-200 bg-amber-50 p-3 text-sm text-amber-900 dark:border-amber-800 dark:bg-amber-900/20 dark:text-amber-200">
            <p>{{ t('userSubscriptions.resetWarning') }}</p>
            <p>{{ t('userSubscriptions.resetRefundNotice') }}</p>
          </div>
          <label class="flex cursor-pointer items-start gap-2 text-sm text-gray-700 dark:text-gray-300">
            <input v-model="resetAccepted" type="checkbox" class="mt-1" :disabled="resetSubmitting" data-reset-accept />
            <span>{{ t('userSubscriptions.resetAccept') }}</span>
          </label>
        </template>
        <p v-else-if="resetPreview" class="rounded-xl bg-amber-50 p-3 text-sm text-amber-900 dark:bg-amber-900/20 dark:text-amber-200" data-reset-unavailable>
          {{ resetPreview.reason || t('userSubscriptions.resetUnavailable') }}
        </p>
        <p v-if="resetError" role="alert" class="text-sm text-red-600 dark:text-red-400" data-reset-error>{{ resetError }}</p>
        <p v-if="resetAttempted && resetError" class="text-xs text-gray-500 dark:text-gray-400">
          {{ t('userSubscriptions.resetRetryHint') }}
        </p>
      </div>
      <template #footer>
        <button type="button" class="btn btn-secondary" :disabled="resetSubmitting" @click="closeSubscriptionReset" data-reset-cancel>
          {{ t('common.cancel') }}
        </button>
        <button
          v-if="!resetLoading && !resetPreview?.eligible"
          type="button"
          class="btn btn-secondary"
          @click="loadResetPreview"
          data-reset-reload
        >
          {{ t('userSubscriptions.resetCheckAgain') }}
        </button>
        <button
          v-if="resetPreview?.eligible"
          type="button"
          class="btn btn-primary"
          :disabled="!resetAccepted || resetSubmitting || !resetPreview.preview_token"
          @click="confirmSubscriptionReset"
          data-reset-confirm
        >
          {{ t(resetSubmitting ? 'userSubscriptions.resetSubmitting' : resetAttempted ? 'userSubscriptions.resetRetry' : 'userSubscriptions.resetConfirm') }}
        </button>
      </template>
    </BaseDialog>
  </AppLayout>
</template>

<script setup lang="ts">
import { computed, ref, onMounted, onUnmounted } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRouter } from 'vue-router'
import { useAppStore } from '@/stores/app'
import subscriptionsAPI, { type SubscriptionResetPreview } from '@/api/subscriptions'
import type { UserSubscription } from '@/types'
import AppLayout from '@/components/layout/AppLayout.vue'
import Icon from '@/components/icons/Icon.vue'
import BaseDialog from '@/components/common/BaseDialog.vue'
import { canOpenSubscriptionReset } from '@/utils/subscriptionReset'
import { formatDateTimeToMinute } from '@/utils/format'
import { hasPeakRate, formatPeakRateWindow, serverTimezoneLabel } from '@/utils/peak-rate'
import { platformBorderClass, platformBadgeClass, platformButtonClass, platformLabel } from '@/utils/platformColors'
import {
  getExpirationDateRelation,
  getRemainingDurationParts,
  isOneTimeDailyQuota,
  type RemainingDurationParts
} from '@/utils/subscriptionQuota'

function platformAccentDotClass(p: string): string {
  switch (p) {
    case 'anthropic': return 'bg-orange-500'
    case 'openai': return 'bg-emerald-500'
    case 'antigravity': return 'bg-purple-500'
    case 'gemini': return 'bg-blue-500'
    default: return 'bg-gray-400'
  }
}

const { t } = useI18n()
const router = useRouter()
const appStore = useAppStore()

const subscriptions = ref<UserSubscription[]>([])
const loading = ref(true)
const currentTime = ref(Date.now())
const resetEnabled = computed(() => appStore.cachedPublicSettings?.subscription_reset_enabled === true)
const resetTarget = ref<number | null>(null)
const resetPreview = ref<SubscriptionResetPreview | null>(null)
const resetLoading = ref(false)
const resetSubmitting = ref(false)
const resetAccepted = ref(false)
const resetAttempted = ref(false)
const resetError = ref('')
let resetRequestVersion = 0
let resetClock: ReturnType<typeof setInterval> | undefined

// 2026-11-24: keep confirmation in Sub2API. Every retry uses the original
// preview token so a lost response cannot accidentally consume another renewal.
function openSubscriptionReset(subscription: UserSubscription) {
  if (!resetEnabled.value || resetSubmitting.value || !canOpenSubscriptionReset(subscription)) return
  resetTarget.value = subscription.id
  resetAttempted.value = false
  void loadResetPreview()
}

async function loadResetPreview() {
  const id = resetTarget.value
  if (!id || resetSubmitting.value || resetAttempted.value) return
  const version = ++resetRequestVersion
  resetPreview.value = null
  resetError.value = ''
  resetAccepted.value = false
  resetLoading.value = true
  try {
    const preview = await subscriptionsAPI.getSubscriptionResetPreview(id)
    if (version === resetRequestVersion && id === resetTarget.value) {
      if (preview.subscription_id !== id) {
        resetError.value = t('userSubscriptions.resetPreviewFailed')
        return
      }
      resetPreview.value = preview
    }
  } catch (error) {
    if (version === resetRequestVersion) resetError.value = resetErrorMessage(error, 'userSubscriptions.resetPreviewFailed')
  } finally {
    if (version === resetRequestVersion) resetLoading.value = false
  }
}

function resetErrorMessage(error: unknown, fallback: string): string {
  if (error && typeof error === 'object' && 'message' in error && typeof error.message === 'string' && error.message) return error.message
  return t(fallback)
}

function formatResetDate(value: string): string {
  return formatDateTimeToMinute(new Date(value))
}

async function confirmSubscriptionReset() {
  const id = resetTarget.value
  const preview = resetPreview.value
  if (!id || !preview?.eligible || !preview.preview_token || !resetAccepted.value || resetSubmitting.value) return
  resetSubmitting.value = true
  resetAttempted.value = true
  resetError.value = ''
  try {
    await subscriptionsAPI.resetSubscription(id, preview.preview_token)
    resetSubmitting.value = false
    appStore.showSuccess(t('userSubscriptions.resetSuccess'))
    closeSubscriptionReset()
  } catch (error) {
    resetError.value = resetErrorMessage(error, 'userSubscriptions.resetFailed')
  } finally {
    resetSubmitting.value = false
  }
}

function closeSubscriptionReset() {
  if (!resetTarget.value || resetSubmitting.value) return
  const attempted = resetAttempted.value
  resetRequestVersion++
  resetTarget.value = null
  resetPreview.value = null
  resetAccepted.value = false
  resetError.value = ''
  resetAttempted.value = false
  // A failed response can still represent a committed reset; refresh when the
  // member closes after an attempt without changing the operation on retry.
  if (attempted) void loadSubscriptions()
}

function subscriptionHasPeakRate(subscription: UserSubscription): boolean {
  return hasPeakRate(subscription.group)
}

function subscriptionPeakRateLabel(subscription: UserSubscription): string {
  return formatPeakRateWindow(subscription.group, serverTimezoneLabel(appStore.cachedPublicSettings?.server_utc_offset))
}

async function loadSubscriptions() {
  try {
    loading.value = true
    subscriptions.value = await subscriptionsAPI.getMySubscriptions()
  } catch (error) {
    console.error('Failed to load subscriptions:', error)
    appStore.showError(t('userSubscriptions.failedToLoad'))
  } finally {
    loading.value = false
  }
}

function getProgressWidth(used: number | undefined, limit: number | null | undefined): string {
  if (!limit || limit === 0) return '0%'
  const percentage = Math.min(((used || 0) / limit) * 100, 100)
  return `${percentage}%`
}

function getProgressBarClass(used: number | undefined, limit: number | null | undefined): string {
  if (!limit || limit === 0) return 'bg-gray-400'
  const percentage = ((used || 0) / limit) * 100
  if (percentage >= 90) return 'bg-red-500'
  if (percentage >= 70) return 'bg-orange-500'
  return 'bg-green-500'
}

function formatExpirationDate(expiresAt: string): string {
  const now = new Date()
  const expires = new Date(expiresAt)
  const diff = expires.getTime() - now.getTime()
  const days = Math.ceil(diff / (1000 * 60 * 60 * 24))
  const relation = getExpirationDateRelation(expires, now)

  if (relation === null) return ''

  if (relation === 'expired') {
    return t('userSubscriptions.status.expired')
  }

  const dateStr = formatDateTimeToMinute(expires)

  if (relation === 'today') {
    return `${dateStr} (${t('common.today')})`
  }
  if (relation === 'tomorrow') {
    return `${dateStr} (${t('common.tomorrow')})`
  }

  return t('userSubscriptions.daysRemaining', { days }) + ` (${dateStr})`
}

function getExpirationClass(expiresAt: string): string {
  const now = new Date()
  const expires = new Date(expiresAt)
  const diff = expires.getTime() - now.getTime()
  const days = Math.ceil(diff / (1000 * 60 * 60 * 24))

  if (diff <= 0) return 'text-red-600 dark:text-red-400 font-medium'
  if (days <= 3) return 'text-red-600 dark:text-red-400'
  if (days <= 7) return 'text-orange-600 dark:text-orange-400'
  return 'text-gray-700 dark:text-gray-300'
}

function formatDurationParts(parts: RemainingDurationParts): string {
  if (parts.days > 0) {
    return `${parts.days}d ${parts.hours}h`
  }

  if (parts.hours > 0) {
    return `${parts.hours}h ${parts.minutes}m`
  }

  return `${parts.minutes}m`
}

function formatDailyUsageWindow(subscription: UserSubscription): string {
  if (isOneTimeDailyQuota(subscription) && subscription.expires_at) {
    const parts = getRemainingDurationParts(subscription.expires_at)
    if (!parts) return t('userSubscriptions.windowNotActive')
    return t('userSubscriptions.quotaEndsIn', { time: formatDurationParts(parts) })
  }

  return t('userSubscriptions.resetIn', {
    time: formatResetTime(subscription.daily_window_start, 24)
  })
}

function formatResetTime(windowStart: string | null, windowHours: number): string {
  if (!windowStart) return t('userSubscriptions.windowNotActive')

  const start = new Date(windowStart)
  const end = new Date(start.getTime() + windowHours * 60 * 60 * 1000)
  const parts = getRemainingDurationParts(end)

  return parts ? formatDurationParts(parts) : t('userSubscriptions.windowNotActive')
}

onMounted(() => {
  void loadSubscriptions()
  void appStore.fetchPublicSettings()
  resetClock = setInterval(() => { currentTime.value = Date.now() }, 1000)
})

onUnmounted(() => {
  resetRequestVersion++
  if (resetClock) clearInterval(resetClock)
})
</script>
