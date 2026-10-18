<template>
  <div ref="containerRef" class="relative min-w-0">
    <button
      ref="triggerRef"
      type="button"
      class="select-trigger w-full"
      :class="{ 'select-trigger-open': isOpen }"
      :aria-expanded="isOpen"
      aria-haspopup="listbox"
      @click="toggleOpen"
    >
      <span class="select-value min-w-0 text-left">
        <span class="block truncate">{{ triggerLabel }}</span>
        <span v-if="modelValue.length > 1" class="mt-0.5 block truncate text-xs font-normal text-gray-400">
          {{ selectedGroupSummary }}
        </span>
      </span>
      <span class="ml-auto flex shrink-0 items-center gap-1.5">
        <span v-if="modelValue.length" class="rounded-full bg-primary-50 px-2 py-0.5 text-xs font-semibold text-primary-600 dark:bg-primary-900/30 dark:text-primary-300">
          {{ modelValue.length }}
        </span>
        <Icon name="chevronDown" size="sm" :class="['transition-transform', isOpen && 'rotate-180']" />
      </span>
    </button>

    <Teleport to="body">
      <Transition name="select-dropdown">
        <div
          v-if="isOpen"
          ref="dropdownRef"
          class="select-dropdown-portal"
          :class="instanceId"
          :style="dropdownStyle"
          role="listbox"
          aria-multiselectable="true"
          @click.stop
          @mousedown.stop
        >
          <div class="select-search">
            <Icon name="search" size="sm" class="text-gray-400" />
            <input
              ref="searchInputRef"
              v-model="searchQuery"
              type="text"
              class="select-search-input"
              :placeholder="searchPlaceholder || t('usage.searchKeys')"
              @click.stop
            />
          </div>

          <div class="flex items-center justify-between border-b border-gray-100 px-4 py-2 dark:border-dark-700">
            <button type="button" class="text-xs font-semibold text-primary-600 hover:text-primary-700" @click="selectAllVisible">
              {{ t('usage.selectAllVisible') }}
            </button>
            <button type="button" class="text-xs text-gray-500 hover:text-gray-700 dark:text-gray-400" @click="clearSelection">
              {{ t('usage.clearSelection') }}
            </button>
          </div>

          <div class="select-options max-h-72">
            <button
              v-for="key in filteredKeys"
              :key="key.id"
              type="button"
              role="option"
              class="select-option w-full text-left"
              :class="modelValue.includes(key.id) && 'select-option-selected'"
              :aria-selected="modelValue.includes(key.id)"
              @click="toggleKey(key.id)"
            >
              <span class="flex min-w-0 flex-1 items-start gap-2.5">
                <span
                  class="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded border border-gray-300 dark:border-dark-600"
                  :class="modelValue.includes(key.id) && 'border-primary-500 bg-primary-50 dark:bg-primary-900/30'"
                >
                  <Icon v-if="modelValue.includes(key.id)" name="check" size="sm" class="text-primary-500" />
                </span>
                <span class="min-w-0 flex-1">
                  <span class="block truncate font-medium">{{ key.name }}</span>
                  <span class="mt-0.5 block truncate text-xs font-normal text-gray-400">
                    {{ key.group?.name || t('usage.ungroupedKey') }} · #{{ key.id }}
                  </span>
                </span>
              </span>
            </button>
            <p v-if="filteredKeys.length === 0" class="px-4 py-4 text-center text-xs text-gray-400">
              {{ t('usage.noMatchingKeys') }}
            </p>
          </div>

          <div class="flex items-center justify-between border-t border-gray-100 px-4 py-2.5 dark:border-dark-700">
            <span class="text-xs text-gray-400">{{ t('usage.selectedKeyCount', { count: modelValue.length }) }}</span>
            <button type="button" class="btn btn-primary btn-sm" @click="close()">{{ t('common.confirm') }}</button>
          </div>
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<script setup lang="ts">
import type { CSSProperties } from 'vue'
import { computed, nextTick, onBeforeUnmount, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import type { ApiKey } from '@/types'
import Icon from '@/components/icons/Icon.vue'

const props = withDefaults(defineProps<{
  modelValue: number[]
  apiKeys: ApiKey[]
  allLabel: string
  searchPlaceholder?: string
}>(), {
  searchPlaceholder: '',
})

const emit = defineEmits<{
  'update:modelValue': [value: number[]]
  change: [value: number[]]
}>()

const { t } = useI18n()
const containerRef = ref<HTMLElement | null>(null)
const triggerRef = ref<HTMLButtonElement | null>(null)
const dropdownRef = ref<HTMLElement | null>(null)
const searchInputRef = ref<HTMLInputElement | null>(null)
const isOpen = ref(false)
const searchQuery = ref('')
const dirty = ref(false)
const instanceId = `usage-key-select-${Math.random().toString(36).slice(2, 9)}`

const selectedKeys = computed(() => props.apiKeys.filter((key) => props.modelValue.includes(key.id)))
const triggerLabel = computed(() => {
  if (props.modelValue.length === 0) return props.allLabel
  if (props.modelValue.length === 1) return selectedKeys.value[0]?.name || t('usage.selectedKeyCount', { count: 1 })
  return t('usage.selectedKeyCount', { count: props.modelValue.length })
})
const selectedGroupSummary = computed(() => {
  const groups = Array.from(new Set(selectedKeys.value.map((key) => key.group?.name || t('usage.ungroupedKey'))))
  return groups.length <= 2 ? groups.join(' · ') : t('usage.groupCountSummary', { count: groups.length })
})
const filteredKeys = computed(() => {
  const needle = searchQuery.value.trim().toLowerCase()
  if (!needle) return props.apiKeys
  return props.apiKeys.filter((key) =>
    key.name.toLowerCase().includes(needle)
    || String(key.id).includes(needle)
    || (key.group?.name || '').toLowerCase().includes(needle)
  )
})

const dropdownStyle = computed<CSSProperties>(() => {
  const trigger = triggerRef.value
  if (!trigger) return {}
  const rect = trigger.getBoundingClientRect()
  const padding = 8
  const viewportRight = Math.max(padding, window.innerWidth - padding)
  const left = Math.min(Math.max(padding, rect.left), viewportRight)
  const availableWidth = Math.max(0, viewportRight - left)
  const minWidth = Math.min(Math.max(280, rect.width), availableWidth)
  const estimatedHeight = 360
  const top = rect.bottom + 4 + estimatedHeight > window.innerHeight - padding
    ? Math.max(padding, rect.top - estimatedHeight - 4)
    : rect.bottom + 4
  return {
    position: 'fixed',
    left: `${left}px`,
    top: `${top}px`,
    minWidth: `${minWidth}px`,
    maxWidth: `${availableWidth}px`,
    zIndex: '100000020',
  }
})

function updateValue(value: number[]) {
  const next = Array.from(new Set(value)).slice(0, 100)
  emit('update:modelValue', next)
  dirty.value = true
}

function toggleKey(id: number) {
  const next = new Set(props.modelValue)
  if (next.has(id)) next.delete(id)
  else next.add(id)
  updateValue([...next])
}

function selectAllVisible() {
  updateValue([...new Set([...props.modelValue, ...filteredKeys.value.map((key) => key.id)])])
}

function clearSelection() {
  updateValue([])
}

function toggleOpen() {
  if (isOpen.value) close()
  else open()
}

function open() {
  isOpen.value = true
  searchQuery.value = ''
  void nextTick(() => {
    positionDropdown()
    searchInputRef.value?.focus()
    document.addEventListener('mousedown', onDocumentMouseDown)
    document.addEventListener('keydown', onDocumentKeyDown)
    window.addEventListener('resize', positionDropdown)
    window.addEventListener('scroll', positionDropdown, true)
  })
}

function close(notify = true) {
  const changed = dirty.value
  isOpen.value = false
  dirty.value = false
  document.removeEventListener('mousedown', onDocumentMouseDown)
  document.removeEventListener('keydown', onDocumentKeyDown)
  window.removeEventListener('resize', positionDropdown)
  window.removeEventListener('scroll', positionDropdown, true)
  if (notify && changed) emit('change', props.modelValue)
}

function positionDropdown() {
  const trigger = triggerRef.value
  const dropdown = dropdownRef.value
  if (!trigger || !dropdown) return
  const rect = trigger.getBoundingClientRect()
  const padding = 8
  const viewportRight = Math.max(padding, window.innerWidth - padding)
  const left = Math.min(Math.max(padding, rect.left), viewportRight)
  const availableWidth = Math.max(0, viewportRight - left)
  const estimatedHeight = 360
  const top = rect.bottom + 4 + estimatedHeight > window.innerHeight - padding
    ? Math.max(padding, rect.top - estimatedHeight - 4)
    : rect.bottom + 4
  dropdown.style.left = `${left}px`
  dropdown.style.top = `${top}px`
  dropdown.style.minWidth = `${Math.min(Math.max(280, rect.width), availableWidth)}px`
  dropdown.style.maxWidth = `${availableWidth}px`
}

function onDocumentMouseDown(event: MouseEvent) {
  const target = event.target as Node | null
  if (!target) return
  if (containerRef.value?.contains(target) || dropdownRef.value?.contains(target)) return
  close()
}

function onDocumentKeyDown(event: KeyboardEvent) {
  if (event.key === 'Escape') close()
}

onBeforeUnmount(() => close(false))
</script>

<style scoped>
.select-trigger {
  @apply flex w-full items-center justify-between gap-2;
  @apply rounded-xl px-4 py-2.5 text-sm;
  @apply border border-gray-200 bg-white text-gray-900;
  @apply transition-all duration-200 hover:border-gray-300;
  @apply focus:border-primary-500 focus:outline-none focus:ring-2 focus:ring-primary-500/30;
  @apply dark:border-dark-600 dark:bg-dark-800 dark:text-gray-100 dark:hover:border-dark-500;
  cursor: pointer;
}

.select-value {
  @apply min-w-0 flex-1 overflow-hidden;
}

.select-trigger-open {
  @apply border-primary-500 ring-2 ring-primary-500/30;
}
</style>
