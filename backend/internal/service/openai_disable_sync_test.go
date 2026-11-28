package service

import (
	"context"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
)

// 2026-11-28: exercise actual account selection, including a sticky account
// that becomes ineligible, rather than only checking the metadata accessor.
func TestOpenAIDisableSyncSelectsAllowedAccount(t *testing.T) {
	for _, mode := range []string{"legacy", "load_batch", "advanced"} {
		for _, sticky := range []bool{false, true} {
			t.Run(mode+map[bool]string{false: "/fresh", true: "/sticky"}[sticky], func(t *testing.T) {
				resetOpenAIAdvancedSchedulerSettingCacheForTest()
				t.Cleanup(resetOpenAIAdvancedSchedulerSettingCacheForTest)
				groupID := int64(1128)
				accounts := []Account{
					{ID: 1, Platform: PlatformOpenAI, Type: AccountTypeAPIKey, Status: StatusActive, Schedulable: true, Concurrency: 1, Priority: 0, GroupIDs: []int64{groupID}, Extra: map[string]any{"openai_disable_sync": true}},
					{ID: 2, Platform: PlatformOpenAI, Type: AccountTypeAPIKey, Status: StatusActive, Schedulable: true, Concurrency: 1, Priority: 5, GroupIDs: []int64{groupID}},
				}
				cache := &schedulerTestGatewayCache{}
				session := ""
				if sticky {
					session = "disable_sync"
					cache.sessionBindings = map[string]int64{"openai:" + session: 1}
				}
				cfg := &config.Config{}
				cfg.Gateway.Scheduling.LoadBatchEnabled = mode == "load_batch"
				svc := &OpenAIGatewayService{
					accountRepo: schedulerTestOpenAIAccountRepo{accounts: accounts}, cache: cache, cfg: cfg,
					concurrencyService: NewConcurrencyService(schedulerTestConcurrencyCache{}),
				}
				if mode == "advanced" {
					svc.rateLimitService = newOpenAIAdvancedSchedulerRateLimitService("true")
				}
				ctx := WithOpenAIInboundRequestPolicy(context.Background(), OpenAIInboundProtocolResponses, nil, RequestTypeSync)
				selection, _, err := svc.SelectAccountWithScheduler(ctx, &groupID, "", session, "gpt-6.2-sol", nil, OpenAIUpstreamTransportAny, false)
				require.NoError(t, err)
				require.NotNil(t, selection)
				require.Equal(t, int64(2), selection.Account.ID)
				if selection.ReleaseFunc != nil {
					selection.ReleaseFunc()
				}

				// Once every account opts out, do not silently use the restricted
				// sticky or best-priority candidate to satisfy the request.
				accounts[1].Extra = map[string]any{"openai_disable_sync": true}
				selection, _, err = svc.SelectAccountWithScheduler(ctx, &groupID, "", session, "gpt-6.2-sol", nil, OpenAIUpstreamTransportAny, false)
				require.ErrorIs(t, err, ErrNoAvailableAccounts)
				require.Nil(t, selection)
			})
		}
	}
}

func TestOpenAIDisableSyncClientModesAndAccountTypes(t *testing.T) {
	for _, typ := range []string{AccountTypeOAuth, AccountTypeSetupToken, AccountTypeAPIKey} {
		for _, protocol := range []OpenAIInboundProtocol{OpenAIInboundProtocolResponses, OpenAIInboundProtocolChatCompletions, OpenAIInboundProtocolMessages} {
			for _, mode := range []RequestType{RequestTypeSync, RequestTypeStream, RequestTypeWSV2, RequestTypeUnknown} {
				t.Run(typ+"/"+string(protocol)+"/"+mode.String(), func(t *testing.T) {
					account := &Account{Platform: PlatformOpenAI, Type: typ, Extra: map[string]any{"openai_disable_sync": true}}
					ctx := WithOpenAIInboundRequestPolicy(context.Background(), protocol, nil, mode)
					require.Equal(t, mode != RequestTypeSync, openAIInboundAccountAllowed(ctx, account))
					account.Extra["openai_disable_sync"] = false
					require.True(t, openAIInboundAccountAllowed(ctx, account))
					delete(account.Extra, "openai_disable_sync")
					require.True(t, openAIInboundAccountAllowed(ctx, account))
					account.Platform = PlatformAnthropic
					account.Extra["openai_disable_sync"] = true
					require.True(t, openAIInboundAccountAllowed(ctx, account), "OpenAI setting must not restrict other platforms")
				})
			}
		}
	}

	ctx := WithOpenAIInboundRequestPolicy(context.Background(), OpenAIInboundProtocolMessages, nil, RequestTypeStream)
	combined := &Account{Platform: PlatformOpenAI, Type: AccountTypeOAuth, Extra: map[string]any{"openai_disable_sync": true, "openai_responses_only": true}}
	require.False(t, openAIInboundAccountAllowed(ctx, combined), "streaming must not bypass existing Responses-only restriction")
}

func TestOpenAIDisableSyncGuardsBeforeUpstreamTransport(t *testing.T) {
	for _, protocol := range []OpenAIInboundProtocol{OpenAIInboundProtocolResponses, OpenAIInboundProtocolChatCompletions, OpenAIInboundProtocolMessages} {
		t.Run(string(protocol), func(t *testing.T) {
			c, _ := gin.CreateTestContext(httptest.NewRecorder())
			ctx := WithOpenAIInboundRequestPolicy(context.Background(), protocol, nil, RequestTypeSync)
			c.Request = httptest.NewRequest(http.MethodPost, "/v1/responses", nil).WithContext(ctx)
			SetOpenAIClientTransport(c, OpenAIClientTransportHTTP)
			account := &Account{ID: 1, Platform: PlatformOpenAI, Type: AccountTypeOAuth, Extra: map[string]any{
				"openai_disable_sync": true, "openai_oauth_responses_websockets_v2_mode": OpenAIWSIngressModeCtxPool,
			}}
			// No config, credential provider or upstream is wired: the policy must
			// reject before any of those are consulted, even with rewritten stream.
			svc := &OpenAIGatewayService{}
			body := []byte(`{"model":"gpt-6.2-sol","stream":true,"input":"hello"}`)
			var result *OpenAIForwardResult
			var err error
			switch protocol {
			case OpenAIInboundProtocolResponses:
				result, err = svc.Forward(ctx, c, account, body)
			case OpenAIInboundProtocolChatCompletions:
				result, err = svc.ForwardAsChatCompletions(ctx, c, account, body, "", "")
			case OpenAIInboundProtocolMessages:
				result, err = svc.ForwardAsAnthropic(ctx, c, account, body, "", "")
			}
			require.Nil(t, result)
			var policyErr *UpstreamFailoverError
			require.ErrorAs(t, err, &policyErr)
			require.Equal(t, OpenAIInboundSyncRestrictionReason, policyErr.Reason)
			require.True(t, policyErr.RequestScopedTransient)
			require.False(t, c.Writer.Written(), "guard must let the handler select another account before output")
		})
	}
}
