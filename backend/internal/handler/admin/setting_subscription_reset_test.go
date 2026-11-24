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
	after := &service.SystemSettings{SubscriptionResetURL: "https://www.yunqiao.me/subscription-reset"}
	changed := diffSettings(before, after, nil, nil, UpdateSettingsRequest{})
	require.Contains(t, changed, "subscription_reset_url")
	require.Contains(t, omittedSettingKeys(map[string]json.RawMessage{}), service.SettingKeySubscriptionResetURL)
	require.NotContains(t, omittedSettingKeys(map[string]json.RawMessage{"subscription_reset_url": json.RawMessage(`""`)}), service.SettingKeySubscriptionResetURL)
}
