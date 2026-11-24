//go:build unit

package service

import (
	"context"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/stretchr/testify/require"
)

func TestSubscriptionResetURLValidation(t *testing.T) {
	for _, value := range []string{"", "  ", "https://www.yunqiao.me/subscription-reset", "https://store.example:8443/reset?mode=confirm"} {
		require.NoError(t, ValidateSubscriptionResetURL(value), value)
	}
	for _, value := range []string{
		"http://store.example/reset", "javascript:alert(1)", "//store.example/reset", "/reset",
		"https://user:pass@store.example/reset", "https://@store.example/reset", "https://store.example/reset#other",
		"https://store.example\\@evil.example/reset", "https://store.example/reset\nother", "https:///reset",
	} {
		require.Error(t, ValidateSubscriptionResetURL(value), value)
	}
}

func TestSubscriptionResetSettingPublicBootstrapAndFrameOrigin(t *testing.T) {
	for _, test := range []struct{ name, stored, exposed string }{
		{"disabled by default", "", ""},
		{"trusted https", " https://www.yunqiao.me/subscription-reset ", "https://www.yunqiao.me/subscription-reset"},
		{"invalid stored url fails closed", "https://user:pass@untrusted.example/reset", ""},
	} {
		t.Run(test.name, func(t *testing.T) {
			repo := &settingPublicRepoStub{values: map[string]string{SettingKeySubscriptionResetURL: test.stored}}
			svc := NewSettingService(repo, &config.Config{})
			settings, err := svc.GetPublicSettings(context.Background())
			require.NoError(t, err)
			require.Equal(t, test.exposed, settings.SubscriptionResetURL)
			payload, err := svc.GetPublicSettingsForInjection(context.Background())
			require.NoError(t, err)
			require.Equal(t, test.exposed, payload.(*PublicSettingsInjectionPayload).SubscriptionResetURL)
			origins, err := svc.GetFrameSrcOrigins(context.Background())
			require.NoError(t, err)
			if test.exposed == "" {
				require.Empty(t, origins)
			} else {
				require.Equal(t, []string{"https://www.yunqiao.me"}, origins)
			}
		})
	}
}

func TestSubscriptionResetSettingPersistValidateAndOmit(t *testing.T) {
	t.Run("persist and read", func(t *testing.T) {
		repo := &settingUpdateRepoStub{}
		svc := NewSettingService(repo, &config.Config{})
		err := svc.UpdateSettings(context.Background(), &SystemSettings{SubscriptionResetURL: " https://www.yunqiao.me/subscription-reset "})
		require.NoError(t, err)
		require.Equal(t, "https://www.yunqiao.me/subscription-reset", repo.updates[SettingKeySubscriptionResetURL])
		reader := NewSettingService(&settingGetAllRepoStub{values: repo.updates}, &config.Config{})
		stored, err := reader.GetAllSettings(context.Background())
		require.NoError(t, err)
		require.Equal(t, "https://www.yunqiao.me/subscription-reset", stored.SubscriptionResetURL)
	})
	t.Run("reject before saving", func(t *testing.T) {
		repo := &settingUpdateRepoStub{}
		svc := NewSettingService(repo, &config.Config{})
		err := svc.UpdateSettings(context.Background(), &SystemSettings{SubscriptionResetURL: "http://store.example/reset"})
		require.Error(t, err)
		require.Nil(t, repo.updates)
	})
	t.Run("omitted setting is not cleared", func(t *testing.T) {
		updates := map[string]string{SettingKeySubscriptionResetURL: ""}
		OmittedSettingKeys{SettingKeySubscriptionResetURL: {}}.dropFrom(updates)
		require.NotContains(t, updates, SettingKeySubscriptionResetURL)
	})
}
