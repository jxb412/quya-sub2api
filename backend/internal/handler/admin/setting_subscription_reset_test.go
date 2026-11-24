//go:build unit

package admin

import (
	"encoding/json"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/stretchr/testify/require"
)

func TestSubscriptionResetSettingAuditAndOmission(t *testing.T) {
	before := &service.SystemSettings{}
	after := &service.SystemSettings{SubscriptionResetEnabled: true}
	changed := diffSettings(before, after, nil, nil, UpdateSettingsRequest{})
	require.Contains(t, changed, "subscription_reset_enabled")
	require.Contains(t, omittedSettingKeys(map[string]json.RawMessage{}), service.SettingKeySubscriptionResetEnabled)
	require.NotContains(t, omittedSettingKeys(map[string]json.RawMessage{"subscription_reset_enabled": json.RawMessage(`false`)}), service.SettingKeySubscriptionResetEnabled)
}
