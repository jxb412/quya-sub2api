//go:build unit

package service

import (
	"context"
	"errors"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/stretchr/testify/require"
)

func TestSubscriptionResetSettingPublicBootstrapAndFrameOrigin(t *testing.T) {
	for _, test := range []struct {
		stored  string
		exposed bool
	}{{"", false}, {"true", true}, {"false", false}, {"invalid", false}} {
		t.Run(test.stored, func(t *testing.T) {
			repo := &settingPublicRepoStub{values: map[string]string{SettingKeySubscriptionResetEnabled: test.stored, "subscription_reset_url": "https://legacy.example/reset"}}
			svc := NewSettingService(repo, &config.Config{})
			settings, err := svc.GetPublicSettings(context.Background())
			require.NoError(t, err)
			require.Equal(t, test.exposed, settings.SubscriptionResetEnabled)
			payload, err := svc.GetPublicSettingsForInjection(context.Background())
			require.NoError(t, err)
			require.Equal(t, test.exposed, payload.(*PublicSettingsInjectionPayload).SubscriptionResetEnabled)
			origins, err := svc.GetFrameSrcOrigins(context.Background())
			require.NoError(t, err)
			require.Empty(t, origins, "native reset must not grant a legacy storefront frame origin")
		})
	}
}

type nativeResetUpdateRepo struct{ settingUpdateRepoStub }

func (r *nativeResetUpdateRepo) GetAll(context.Context) (map[string]string, error) {
	return r.updates, nil
}

func TestSubscriptionResetSettingPersistAndOmit(t *testing.T) {
	repo := &nativeResetUpdateRepo{}
	svc := NewSettingService(repo, &config.Config{})
	require.NoError(t, svc.UpdateSettings(context.Background(), &SystemSettings{SubscriptionResetEnabled: true}))
	require.Equal(t, "true", repo.updates[SettingKeySubscriptionResetEnabled])
	reader := NewSettingService(&settingGetAllRepoStub{values: repo.updates}, &config.Config{})
	stored, err := reader.GetAllSettings(context.Background())
	require.NoError(t, err)
	require.True(t, stored.SubscriptionResetEnabled)
	require.NoError(t, svc.UpdateSettingsOmitting(context.Background(), &SystemSettings{}, OmittedSettingKeys{SettingKeySubscriptionResetEnabled: {}}))
	require.NotContains(t, repo.updates, SettingKeySubscriptionResetEnabled)
	require.NoError(t, svc.UpdateSettings(context.Background(), &SystemSettings{}))
	require.Equal(t, "false", repo.updates[SettingKeySubscriptionResetEnabled])
}

type renewalSwitchRepo struct {
	SettingRepository
	value string
	err   error
}

func (r *renewalSwitchRepo) GetValue(context.Context, string) (string, error) { return r.value, r.err }

func TestSubscriptionResetSettingServerSwitchFailsClosed(t *testing.T) {
	ctx := context.Background()
	var missing *SettingService
	require.False(t, missing.IsSubscriptionResetEnabled(ctx))
	repo := &renewalSwitchRepo{value: "true"}
	cfg := &config.Config{}
	svc := NewSettingService(repo, cfg)
	require.True(t, svc.IsSubscriptionResetEnabled(ctx))
	repo.value = "false"
	require.False(t, svc.IsSubscriptionResetEnabled(ctx), "switch must not remain enabled in another instance cache")
	repo.value = "true"
	repo.err = errors.New("database unavailable")
	require.False(t, svc.IsSubscriptionResetEnabled(ctx))
	repo.err = nil
	cfg.SubscriptionMaintenance.AutoResetEnabled = true
	require.False(t, svc.IsSubscriptionResetEnabled(ctx))
}
