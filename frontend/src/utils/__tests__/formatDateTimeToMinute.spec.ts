import { describe, expect, it } from 'vitest'

import { formatDateTimeToMinute } from '../format'

describe('formatDateTimeToMinute', () => {
  it('formats Beijing date and time without seconds', () => {
    const value = new Date('2026-07-19T12:30:45Z')

    expect(formatDateTimeToMinute(value, 'en-GB')).toBe('19/07/2026, 20:30')
  })

  it('returns an empty string for an invalid date', () => {
    expect(formatDateTimeToMinute(new Date('invalid'), 'en-GB')).toBe('')
  })
})
