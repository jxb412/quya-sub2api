import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { enableAutoUnmount, flushPromises, mount } from '@vue/test-utils'
import { reactive } from 'vue'
import SubscriptionsView from '../SubscriptionsView.vue'

const mocks = vi.hoisted(() => ({ list: vi.fn(), preview: vi.fn(), reset: vi.fn(), fetchSettings: vi.fn(), showError: vi.fn(), showSuccess: vi.fn(), push: vi.fn() }))
const app = reactive({ cachedPublicSettings: { subscription_reset_enabled: true as boolean | undefined }, fetchPublicSettings: mocks.fetchSettings, showError: mocks.showError, showSuccess: mocks.showSuccess })
vi.mock('vue-i18n', async importOriginal => ({
  ...await importOriginal<typeof import('vue-i18n')>(),
  useI18n: () => ({ t: (key: string, values?: Record<string, unknown>) => values ? `${key}:${JSON.stringify(values)}` : key }),
}))
vi.mock('vue-router', () => ({ useRouter: () => ({ push: mocks.push }) }))
vi.mock('@/stores/app', () => ({ useAppStore: () => app }))
vi.mock('@/api/subscriptions', () => ({ default: { getMySubscriptions: mocks.list, getSubscriptionResetPreview: mocks.preview, resetSubscription: mocks.reset } }))
vi.mock('@/components/layout/AppLayout.vue', () => ({ default: { template: '<main><slot /></main>' } }))

enableAutoUnmount(afterEach)
const now = Date.parse('2026-11-24T12:00:00Z')
const makeSubscription = (id: number, days: number, status = 'active') => ({
  id, group_id: 1, status, expires_at: new Date(now + days * 86400000).toISOString(),
  group: { name: 'Test subscription', platform: 'openai' },
})
const makePreview = (id = 17) => ({
  eligible: true, subscription_id: id, group_id: 1, group_name: 'Test subscription',
  current_order_no: 'P20261101001', next_order_no: 'P20261124002', current_validity_days: 30,
  next_validity_days: 31, pending_count: 2, remaining_pending_count: 1, total_validity_days: 61,
  old_expires_at: new Date(now + 78 * 86400000).toISOString(),
  new_expires_at: new Date(now + 61 * 86400000).toISOString(),
  discarded_remaining_seconds: 17 * 86400, preview_token: `snapshot-for-${id}`,
})
const mountPage = () => mount(SubscriptionsView, { attachTo: document.body, global: { stubs: {
  Icon: true,
  BaseDialog: { props: ['show'], template: '<section v-if="show"><button data-close @click="$emit(\'close\')">close</button><slot /><slot name="footer" /></section>' },
} } })
const openReset = async () => {
  const wrapper = mountPage()
  await flushPromises()
  await wrapper.get('[data-reset-subscription-id="17"]').trigger('click')
  await flushPromises()
  return wrapper
}

beforeEach(() => {
  vi.spyOn(Date, 'now').mockReturnValue(now)
  mocks.list.mockReset().mockResolvedValue([makeSubscription(17, 78)])
  mocks.preview.mockReset().mockResolvedValue(makePreview())
  mocks.reset.mockReset().mockResolvedValue({ old_subscription_id: 17, new_subscription_id: 18, replayed: false })
  mocks.fetchSettings.mockReset().mockResolvedValue(null)
  mocks.showError.mockReset()
  mocks.showSuccess.mockReset()
  app.cachedPublicSettings.subscription_reset_enabled = true
})
afterEach(() => { vi.restoreAllMocks(); document.body.innerHTML = '' })

describe('native subscription reset', () => {
  it('shows only active subscriptions with more than 31 days', async () => {
    mocks.list.mockResolvedValue([makeSubscription(17, 31), makeSubscription(18, 31.001), makeSubscription(19, 60, 'revoked')])
    const wrapper = mountPage()
    await flushPromises()
    expect(wrapper.findAll('[data-reset-subscription-id]').map(x => x.attributes('data-reset-subscription-id'))).toEqual(['18'])
  })

  it.each([false, undefined])('hides the entry when the server switch is %s', async enabled => {
    app.cachedPublicSettings.subscription_reset_enabled = enabled
    const wrapper = mountPage()
    await flushPromises()
    expect(wrapper.find('[data-reset-subscription-id]').exists()).toBe(false)
  })

  it('shows the verified orders, actual period and remaining queue in a native confirmation', async () => {
    const wrapper = await openReset()
    expect(mocks.preview).toHaveBeenCalledWith(17)
    expect(wrapper.find('iframe').exists()).toBe(false)
    expect(wrapper.get('[data-reset-current-order]').text()).toBe('P20261101001')
    expect(wrapper.get('[data-reset-next-order]').text()).toBe('P20261124002')
    expect(wrapper.text()).toContain('userSubscriptions.resetFullDays:{"days":31}')
    expect(wrapper.text()).toContain('userSubscriptions.resetQueuedCount:{"count":1}')
    expect(wrapper.text()).toContain('userSubscriptions.resetExpiryHint:{"days":61}')
    expect(wrapper.text()).toContain('userSubscriptions.resetWarning')
    expect(wrapper.text()).toContain('userSubscriptions.resetRefundNotice')
    expect(wrapper.get<HTMLButtonElement>('[data-reset-confirm]').element.disabled).toBe(true)
    await wrapper.get('[data-reset-confirm]').trigger('click')
    expect(mocks.reset).not.toHaveBeenCalled()
    await wrapper.get('[data-reset-cancel]').trigger('click')
    expect(mocks.reset).not.toHaveBeenCalled()
    expect(mocks.list).toHaveBeenCalledTimes(1)
  })

  it('does not permit reset when the server cannot prove an unused renewal', async () => {
    mocks.preview.mockResolvedValue({ ...makePreview(), eligible: false, reason: 'No unconsumed renewal' })
    const wrapper = await openReset()
    expect(wrapper.get('[data-reset-unavailable]').text()).toBe('No unconsumed renewal')
    expect(wrapper.find('[data-reset-confirm]').exists()).toBe(false)
    expect(mocks.reset).not.toHaveBeenCalled()
  })

  it('requires confirmation, blocks duplicate submits and closing, then refreshes after success', async () => {
    let resolveReset!: (result: unknown) => void
    mocks.reset.mockReturnValue(new Promise(resolve => { resolveReset = resolve }))
    const wrapper = await openReset()
    await wrapper.get('[data-reset-accept]').setValue(true)
    await wrapper.get('[data-reset-confirm]').trigger('click')
    await wrapper.get('[data-reset-confirm]').trigger('click')
    await wrapper.get('[data-close]').trigger('click')
    expect(mocks.reset).toHaveBeenCalledTimes(1)
    expect(mocks.reset).toHaveBeenCalledWith(17, 'snapshot-for-17')
    expect(wrapper.get<HTMLButtonElement>('[data-reset-cancel]').element.disabled).toBe(true)
    expect(wrapper.find('[data-reset-confirm]').exists()).toBe(true)
    resolveReset({ old_subscription_id: 17, new_subscription_id: 18 })
    await flushPromises()
    expect(mocks.list).toHaveBeenCalledTimes(2)
    expect(mocks.showSuccess).toHaveBeenCalledWith('userSubscriptions.resetSuccess')
    expect(wrapper.find('[data-reset-confirm]').exists()).toBe(false)
  })

  it('retries the same preview after a lost result without selecting another purchase', async () => {
    mocks.reset.mockRejectedValueOnce({ message: 'Connection interrupted' })
    const wrapper = await openReset()
    await wrapper.get('[data-reset-accept]').setValue(true)
    await wrapper.get('[data-reset-confirm]').trigger('click')
    await flushPromises()
    expect(wrapper.get('[data-reset-error]').text()).toBe('Connection interrupted')
    expect(wrapper.find('[data-reset-reload]').exists()).toBe(false)
    expect(wrapper.get('[data-reset-confirm]').text()).toBe('userSubscriptions.resetRetry')
    await wrapper.get('[data-reset-confirm]').trigger('click')
    await flushPromises()
    expect(mocks.reset.mock.calls).toEqual([[17, 'snapshot-for-17'], [17, 'snapshot-for-17']])
    expect(mocks.preview).toHaveBeenCalledTimes(1)
    expect(mocks.list).toHaveBeenCalledTimes(2)
  })

  it('refreshes subscriptions after closing an uncertain result', async () => {
    mocks.reset.mockRejectedValue({ message: 'Connection interrupted' })
    const wrapper = await openReset()
    await wrapper.get('[data-reset-accept]').setValue(true)
    await wrapper.get('[data-reset-confirm]').trigger('click')
    await flushPromises()
    await wrapper.get('[data-reset-cancel]').trigger('click')
    await flushPromises()
    expect(mocks.list).toHaveBeenCalledTimes(2)
  })

  it('lets a failed preview be checked again without executing anything', async () => {
    mocks.preview.mockRejectedValueOnce({ message: 'Please retry' })
    const wrapper = await openReset()
    expect(wrapper.get('[data-reset-error]').text()).toBe('Please retry')
    expect(wrapper.find('[data-reset-confirm]').exists()).toBe(false)
    await wrapper.get('[data-reset-reload]').trigger('click')
    await flushPromises()
    expect(wrapper.find('[data-reset-error]').exists()).toBe(false)
    expect(wrapper.find('[data-reset-confirm]').exists()).toBe(true)
    expect(mocks.reset).not.toHaveBeenCalled()
  })

  it('ignores an older preview response after another subscription is selected', async () => {
    let resolveFirst!: (result: unknown) => void
    mocks.list.mockResolvedValue([makeSubscription(17, 78), makeSubscription(18, 62)])
    mocks.preview.mockImplementation((id: number) => id === 17 ? new Promise(resolve => { resolveFirst = resolve }) : Promise.resolve(makePreview(18)))
    const wrapper = mountPage()
    await flushPromises()
    await wrapper.get('[data-reset-subscription-id="17"]').trigger('click')
    await wrapper.get('[data-close]').trigger('click')
    await wrapper.get('[data-reset-subscription-id="18"]').trigger('click')
    await flushPromises()
    resolveFirst({ ...makePreview(17), next_order_no: 'WRONG-OLD-ORDER' })
    await flushPromises()
    expect(wrapper.text()).not.toContain('WRONG-OLD-ORDER')
    await wrapper.get('[data-reset-accept]').setValue(true)
    await wrapper.get('[data-reset-confirm]').trigger('click')
    await flushPromises()
    expect(mocks.reset).toHaveBeenCalledWith(18, 'snapshot-for-18')
  })

  it('rejects a preview returned for a different subscription', async () => {
    mocks.preview.mockResolvedValue(makePreview(18))
    const wrapper = await openReset()
    expect(wrapper.find('[data-reset-confirm]').exists()).toBe(false)
    expect(wrapper.get('[data-reset-error]').text()).toBe('userSubscriptions.resetPreviewFailed')
  })
})
