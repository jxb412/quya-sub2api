//go:build unit

package repository

import (
	"context"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/stretchr/testify/require"
)

func TestSchedulerCacheSnapshotPreservesQuyaInboundAndCodexPolicies(t *testing.T) {
	for _, accountType := range []string{service.AccountTypeOAuth, service.AccountTypeSetupToken, service.AccountTypeAPIKey} {
		t.Run(accountType, func(t *testing.T) {
			ctx := context.Background()
			cache := newSchedulerCacheUnit(t)
			bucket := service.SchedulerBucket{GroupID: 26, Platform: service.PlatformOpenAI, Mode: service.SchedulerModeSingle}
			account := service.Account{
				ID:       2601,
				Platform: service.PlatformOpenAI,
				Type:     accountType,
				Credentials: map[string]any{
					"model_mapping":       map[string]any{"gpt-6.2-sol": "gpt-6.2-sol"},
					"openai_capabilities": []string{"seedance"},
					"base_url":            "https://relay.example.com/api/v3",
				},
				Extra: map[string]any{
					"openai_responses_only":           true,
					"codex_cli_only":                  true,
					"codex_cli_only_allow_app_server": true,
					"openai_passthrough":              true,
					"openai_oauth_passthrough":        true,
					"openai_passthrough_model_filter": true,
					"unlisted_private_setting":        "must-be-dropped",
				},
			}
			token, err := cache.CaptureBucketWriteToken(ctx, bucket)
			require.NoError(t, err)
			require.NoError(t, cache.SetSnapshot(ctx, bucket, token, []service.Account{account}))

			read := func() service.Account {
				t.Helper()
				accounts, hit, err := cache.GetSnapshotValues(ctx, bucket)
				require.NoError(t, err)
				require.True(t, hit)
				require.Len(t, accounts, 1)
				got := accounts[0]
				// The custom projection must retain the official capability and
				// opt-in passthrough whitelist semantics at the same time.
				require.Equal(t, []any{"seedance"}, got.Credentials["openai_capabilities"])
				require.Equal(t, accountType == service.AccountTypeAPIKey,
					got.SupportsOpenAIEndpointCapability(service.OpenAIEndpointCapabilitySeedance))
				require.True(t, got.IsOpenAIPassthroughModelFilterEnabled())
				require.True(t, got.IsModelSupported("gpt-6.2-sol"))
				require.False(t, got.IsModelSupported("gpt-6.2-astra"))
				require.NotContains(t, got.Extra, "unlisted_private_setting")
				return got
			}

			before := read()
			require.True(t, before.IsCodexCLIOnlyEnabled())
			require.True(t, before.IsCodexCLIOnlyAppServerAllowed())
			require.True(t, before.SupportsOpenAIInboundProtocol(service.OpenAIInboundProtocolResponses))
			require.False(t, before.SupportsOpenAIInboundProtocol(service.OpenAIInboundProtocolChatCompletions))
			require.False(t, before.SupportsOpenAIInboundProtocol(service.OpenAIInboundProtocolMessages))

			// An account edit must invalidate the memoized projection without a
			// whole-bucket rebuild, including false values and a new allowlist.
			account.Extra = map[string]any{
				"openai_inbound_capabilities":     []string{"chat_completions"},
				"openai_responses_only":           false,
				"codex_cli_only":                  false,
				"codex_cli_only_allow_app_server": false,
				"openai_passthrough":              true,
				"openai_oauth_passthrough":        true,
				"openai_passthrough_model_filter": true,
			}
			require.NoError(t, cache.SetAccount(ctx, &account))
			after := read()
			require.False(t, after.IsCodexCLIOnlyEnabled())
			require.False(t, after.IsCodexCLIOnlyAppServerAllowed())
			require.False(t, after.SupportsOpenAIInboundProtocol(service.OpenAIInboundProtocolResponses))
			require.True(t, after.SupportsOpenAIInboundProtocol(service.OpenAIInboundProtocolChatCompletions))
			require.False(t, after.SupportsOpenAIInboundProtocol(service.OpenAIInboundProtocolMessages))
			require.True(t, before.IsCodexCLIOnlyEnabled(), "refresh must not mutate a previous snapshot")
			require.True(t, before.IsOpenAIResponsesOnly(), "refresh must not mutate a previous snapshot")
		})
	}
}
