<template>
  <section class="card p-5 sm:p-6">
    <div class="mb-5 flex flex-wrap items-start justify-between gap-3">
      <div>
        <h2 class="text-base font-semibold text-gray-900 dark:text-white">{{ t('usage.breakdownTitle') }}</h2>
        <p class="mt-1 text-xs text-gray-500 dark:text-gray-400">{{ t('usage.breakdownHint') }}</p>
      </div>
      <div class="rounded-lg bg-green-50 px-3 py-2 text-right dark:bg-green-900/20">
        <div class="text-[11px] font-medium text-green-700 dark:text-green-300">{{ t('usage.actualSpend') }}</div>
        <div class="mt-0.5 text-lg font-bold tabular-nums text-green-700 dark:text-green-300">${{ formatCost(totalActualCost) }}</div>
      </div>
    </div>

    <div class="grid grid-cols-1 gap-6 2xl:grid-cols-2">
      <div class="min-w-0">
        <div class="mb-2 flex items-center justify-between gap-3">
          <h3 class="text-sm font-semibold text-gray-800 dark:text-gray-200">{{ t('usage.byReasoningEffort') }}</h3>
          <span class="text-xs text-gray-400">{{ t('usage.itemsCount', { count: efforts.length }) }}</span>
        </div>
        <div class="overflow-x-auto rounded-lg border border-gray-200 dark:border-dark-700">
          <table class="w-full min-w-[520px] text-left text-sm">
            <thead class="bg-gray-50 text-xs text-gray-500 dark:bg-dark-800 dark:text-gray-400">
              <tr>
                <th class="px-3 py-2.5 font-medium">{{ t('usage.reasoningEffort') }}</th>
                <th class="px-3 py-2.5 text-right font-medium">{{ t('usage.totalRequests') }}</th>
                <th class="px-3 py-2.5 text-right font-medium">{{ t('usage.totalTokens') }}</th>
                <th class="px-3 py-2.5 text-right font-medium">{{ t('usage.actualSpend') }}</th>
                <th class="px-3 py-2.5 text-right font-medium">{{ t('usage.share') }}</th>
              </tr>
            </thead>
            <tbody class="divide-y divide-gray-100 dark:divide-dark-700">
              <tr v-for="row in efforts" :key="row.reasoning_effort || '__unset__'">
                <td class="px-3 py-3">
                  <span class="inline-flex max-w-[150px] items-center rounded-md bg-slate-100 px-2 py-1 text-xs font-semibold text-slate-700 dark:bg-dark-700 dark:text-gray-200">
                    {{ effortLabel(row.reasoning_effort) }}
                  </span>
                </td>
                <td class="px-3 py-3 text-right tabular-nums text-gray-600 dark:text-gray-300">{{ row.requests.toLocaleString() }}</td>
                <td class="px-3 py-3 text-right tabular-nums text-gray-600 dark:text-gray-300">{{ formatTokens(row.total_tokens) }}</td>
                <td class="px-3 py-3 text-right font-semibold tabular-nums text-green-600 dark:text-green-400">${{ formatCost(row.actual_cost) }}</td>
                <td class="px-3 py-3 text-right tabular-nums text-gray-500">{{ percent(row.actual_cost, totalActualCost) }}</td>
              </tr>
              <tr v-if="efforts.length === 0">
                <td colspan="5" class="px-3 py-8 text-center text-xs text-gray-400">{{ t('usage.noBreakdownData') }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>

      <div class="min-w-0">
        <div class="mb-2 flex items-center justify-between gap-3">
          <h3 class="text-sm font-semibold text-gray-800 dark:text-gray-200">{{ t('usage.byAPIKey') }}</h3>
          <span class="text-xs text-gray-400">{{ t('usage.itemsCount', { count: keys.length }) }}</span>
        </div>
        <div class="overflow-x-auto rounded-lg border border-gray-200 dark:border-dark-700">
          <table class="w-full min-w-[560px] text-left text-sm">
            <thead class="bg-gray-50 text-xs text-gray-500 dark:bg-dark-800 dark:text-gray-400">
              <tr>
                <th class="px-3 py-2.5 font-medium">{{ t('usage.apiKeyFilter') }}</th>
                <th class="px-3 py-2.5 text-right font-medium">{{ t('usage.totalRequests') }}</th>
                <th class="px-3 py-2.5 text-right font-medium">{{ t('usage.totalTokens') }}</th>
                <th class="px-3 py-2.5 text-right font-medium">{{ t('usage.actualSpend') }}</th>
                <th class="px-3 py-2.5 text-right font-medium">{{ t('usage.share') }}</th>
              </tr>
            </thead>
            <tbody class="divide-y divide-gray-100 dark:divide-dark-700">
              <tr v-for="row in keys" :key="`${row.api_key_id}:${row.group_id}`">
                <td class="max-w-[210px] px-3 py-3">
                  <div class="truncate font-medium text-gray-800 dark:text-gray-200" :title="row.api_key_name || `#${row.api_key_id}`">
                    {{ row.api_key_name || `#${row.api_key_id}` }}
                  </div>
                  <div class="mt-0.5 truncate text-xs text-gray-400" :title="row.group_name || t('usage.ungroupedKey')">
                    {{ row.group_name || t('usage.ungroupedKey') }} · #{{ row.api_key_id }}
                  </div>
                </td>
                <td class="px-3 py-3 text-right tabular-nums text-gray-600 dark:text-gray-300">{{ row.requests.toLocaleString() }}</td>
                <td class="px-3 py-3 text-right tabular-nums text-gray-600 dark:text-gray-300">{{ formatTokens(row.total_tokens) }}</td>
                <td class="px-3 py-3 text-right font-semibold tabular-nums text-green-600 dark:text-green-400">${{ formatCost(row.actual_cost) }}</td>
                <td class="px-3 py-3 text-right tabular-nums text-gray-500">{{ percent(row.actual_cost, totalActualCost) }}</td>
              </tr>
              <tr v-if="keys.length === 0">
                <td colspan="5" class="px-3 py-8 text-center text-xs text-gray-400">{{ t('usage.noBreakdownData') }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </div>
  </section>
</template>

<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import type { APIKeyUsageStat, ReasoningEffortUsageStat } from '@/types'
import { formatReasoningEffort } from '@/utils/format'

defineProps<{
  keys: APIKeyUsageStat[]
  efforts: ReasoningEffortUsageStat[]
  totalActualCost: number
}>()

const { t } = useI18n()

const formatCost = (value: number) => {
  const amount = Number(value || 0)
  if (amount === 0) return '0.00'
  if (Math.abs(amount) < 0.0001) return amount.toFixed(6)
  if (Math.abs(amount) < 1) return amount.toFixed(4)
  return amount.toFixed(2)
}

const formatTokens = (value: number) => {
  const amount = Number(value || 0)
  if (amount >= 1e9) return (amount / 1e9).toFixed(2) + 'B'
  if (amount >= 1e6) return (amount / 1e6).toFixed(2) + 'M'
  if (amount >= 1e3) return (amount / 1e3).toFixed(2) + 'K'
  return amount.toLocaleString()
}

const percent = (value: number, total: number) => {
  if (!total || total <= 0) return '0%'
  return `${((Number(value || 0) / total) * 100).toFixed(1)}%`
}

const effortLabel = (effort: string) => {
  const raw = (effort || '').trim()
  if (!raw) return t('usage.unspecifiedEffort')
  const formatted = formatReasoningEffort(raw)
  return formatted === '-' ? raw.toUpperCase() : formatted
}
</script>
