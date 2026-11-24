import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { enableAutoUnmount, flushPromises, mount } from '@vue/test-utils'
import { reactive } from 'vue'
import SubscriptionsView from '../SubscriptionsView.vue'

const mocks = vi.hoisted(() => ({ list: vi.fn(), fetchSettings: vi.fn(), showError: vi.fn(), push: vi.fn() }))
const app = reactive({ cachedPublicSettings: { subscription_reset_url: 'https://www.yunqiao.me/subscription-reset' }, fetchPublicSettings: mocks.fetchSettings, showError: mocks.showError })
const auth = reactive({ user: { id: 7 }, token: 'current-login-token' })
vi.mock('vue-i18n', async importOriginal => ({
  ...await importOriginal<typeof import('vue-i18n')>(),
  useI18n: () => ({ t: (key: string) => key, locale: { value: 'zh' } }),
}))
vi.mock('vue-router', () => ({ useRouter: () => ({ push: mocks.push }) }))
vi.mock('@/stores/app', () => ({ useAppStore: () => app }))
vi.mock('@/stores/auth', () => ({ useAuthStore: () => auth }))
vi.mock('@/api/subscriptions', () => ({ default: { getMySubscriptions: mocks.list } }))
vi.mock('@/components/layout/AppLayout.vue', () => ({ default: { template: '<main><slot /></main>' } }))

enableAutoUnmount(afterEach)
const now = Date.parse('2026-11-24T12:00:00Z')
const makeSubscription = (id: number, days: number, status = 'active') => ({
  id, group_id: 1, status, expires_at: new Date(now + days * 86400000).toISOString(),
  group: { name: 'Test subscription', platform: 'openai' },
})
const mountPage = () => mount(SubscriptionsView, { attachTo: document.body, global: { stubs: {
  Icon: true,
  BaseDialog: { props: ['show'], template: '<section v-if="show"><button data-close @click="$emit(\'close\')">close</button><slot /></section>' },
} } })

beforeEach(() => {
  vi.spyOn(Date, 'now').mockReturnValue(now)
  mocks.list.mockReset().mockResolvedValue([makeSubscription(17, 45)])
  mocks.fetchSettings.mockReset().mockResolvedValue(null)
  mocks.showError.mockReset()
  auth.token = 'current-login-token'
  app.cachedPublicSettings.subscription_reset_url = 'https://www.yunqiao.me/subscription-reset'
})
afterEach(() => { vi.restoreAllMocks(); document.body.innerHTML = '' })

describe('subscription reset entry', () => {
  it('shows only active subscriptions with more than 31 days', async () => {
    mocks.list.mockResolvedValue([makeSubscription(17, 31), makeSubscription(18, 31.001), makeSubscription(19, 60, 'revoked')])
    const wrapper = mountPage()
    await flushPromises()
    expect(wrapper.findAll('[data-reset-subscription-id]').map(x => x.attributes('data-reset-subscription-id'))).toEqual(['18'])
  })
  it.each(['', 'http://untrusted.example/reset', 'https://user:pass@store.example/reset'])('hides an unavailable or invalid destination (%s)', async value => {
    app.cachedPublicSettings.subscription_reset_url = value
    const wrapper = mountPage()
    await flushPromises()
    expect(wrapper.find('[data-reset-subscription-id]').exists()).toBe(false)
  })
  it('opens the configured page with current identity, then reloads when closed', async () => {
    const wrapper = mountPage()
    await flushPromises()
    auth.token = 'new-current-token'
    await wrapper.get('[data-reset-subscription-id="17"]').trigger('click')
    const url = new URL(wrapper.get('iframe').attributes('src'))
    expect(url.origin).toBe('https://www.yunqiao.me')
    expect(url.pathname).toBe('/subscription-reset')
    expect(url.searchParams.get('token')).toBe('new-current-token')
    expect(url.searchParams.get('user_id')).toBe('7')
    expect(url.searchParams.get('subscription_id')).toBe('17')
    expect(url.searchParams.get('ui_mode')).toBe('embedded')
    await wrapper.get('[data-close]').trigger('click')
    await flushPromises()
    expect(mocks.list).toHaveBeenCalledTimes(2)
    expect(wrapper.find('iframe').exists()).toBe(false)
  })
  it('ignores forged completion messages and accepts only its selected iframe', async () => {
    const wrapper = mountPage()
    await flushPromises()
    await wrapper.get('[data-reset-subscription-id="17"]').trigger('click')
    const frame = wrapper.get('iframe').element as HTMLIFrameElement
    const data = { type: 'quya:subscription-reset-completed', old_subscription_id: 17, new_subscription_id: 18 }
    for (const init of [
      { origin: 'https://evil.example', source: frame.contentWindow, data },
      { origin: 'https://www.yunqiao.me', source: window, data },
      { origin: 'https://www.yunqiao.me', source: frame.contentWindow, data: { ...data, old_subscription_id: 19 } },
    ]) window.dispatchEvent(new MessageEvent('message', init))
    await flushPromises()
    expect(mocks.list).toHaveBeenCalledTimes(1)
    window.dispatchEvent(new MessageEvent('message', { origin: 'https://www.yunqiao.me', source: frame.contentWindow, data }))
    await flushPromises()
    expect(mocks.list).toHaveBeenCalledTimes(2)
    expect(wrapper.find('iframe').exists()).toBe(false)
  })
})
