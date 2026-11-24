package service

import "context"

// 2026-11-24: read the native reset switch on each request. A stale public
// setting may show a button, but must not open mutations during an upgrade.
func (s *SettingService) IsSubscriptionResetEnabled(ctx context.Context) bool {
	if s == nil || s.settingRepo == nil || (s.cfg != nil && s.cfg.SubscriptionMaintenance.AutoResetEnabled) {
		return false
	}
	value, err := s.settingRepo.GetValue(ctx, SettingKeySubscriptionResetEnabled)
	return err == nil && value == "true"
}
