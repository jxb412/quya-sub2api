//go:build unit

package service

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/stretchr/testify/require"
)

// 2026-11-28: recovered non-streaming output must use the established local
// model-price and user/group multiplier pipeline, for observed and estimated
// usage alike. Exercise RecordUsage and its balance debit, not a duplicate
// implementation of billing in the recovery handler.
func TestOpenAINonStreamingRecoveredUsageUsesLocalPricesAndMultipliers(t *testing.T) {
	const groupID int64 = 777
	const usageFrame = "event: response.in_progress\n" +
		`data: {"type":"response.in_progress","response":{"id":"resp_synthetic","usage":{"input_tokens":17,"output_tokens":3,"input_tokens_details":{"cached_tokens":11}}}}` + "\n\n"
	for _, passthrough := range []bool{false, true} {
		path := "normal"
		if passthrough {
			path = "passthrough"
		}
		for _, observed := range []bool{false, true} {
			usageName := "estimated_usage"
			body := nonstreamMissingTerminalDelta
			if observed {
				usageName = "upstream_usage"
				body += usageFrame
			}
			for _, userOverride := range []bool{false, true} {
				rateName := "group_rate"
				if userOverride {
					rateName = "user_rate"
				}
				t.Run(path+"/"+usageName+"/"+rateName, func(t *testing.T) {
					parsed := replayNonstreamTerminalFixture(t, passthrough, "text/event-stream", body)
					requireNonstreamRecoveredJSON(t, parsed)
					usageRepo := &openAIRecordUsageLogRepoStub{inserted: true}
					userRepo := &openAIRecordUsageUserRepoStub{}
					rateRepo := &openAIUserGroupRateRepoStub{}
					multiplier := 1.2
					if userOverride {
						multiplier = 0.8
						rateRepo.rate = &multiplier
					}
					svc := newOpenAIRecordUsageServiceForTest(usageRepo, userRepo, &openAIRecordUsageSubRepoStub{}, rateRepo)
					var pricing ChannelModelPricing
					require.NoError(t, json.Unmarshal([]byte(`{"platform":"openai","models":["gpt-5.4"],"billing_mode":"token","input_price":0.000003,"output_price":0.000009,"cache_read_price":0.0000005}`), &pricing))
					channel := Channel{ID: 1, Status: StatusActive, GroupIDs: []int64{groupID}, ModelPricing: []ChannelModelPricing{pricing}, ApplyPricingToAccountStats: true}
					svc.channelService = &ChannelService{}
					svc.channelService.cache.Store(populateChannelCache([]Channel{channel}, map[int64]string{groupID: PlatformOpenAI}))
					svc.resolver = NewModelPricingResolver(svc.channelService, svc.billingService)
					err := svc.RecordUsage(context.Background(), &OpenAIRecordUsageInput{
						Result: &OpenAIForwardResult{RequestID: "resp_synthetic", Model: "gpt-5.4", UpstreamModel: "gpt-5.4", Usage: *parsed.usage},
						APIKey: &APIKey{ID: 1, GroupID: i64p(groupID), Group: &Group{ID: groupID, Platform: PlatformOpenAI, RateMultiplier: 1.2}},
						User:   &User{ID: 1}, Account: &Account{ID: 1, Platform: PlatformOpenAI, Type: AccountTypeAPIKey},
						ChannelUsageFields: ChannelUsageFields{OriginalModel: "gpt-5.4", BillingModelSource: BillingModelSourceRequested},
					})
					require.NoError(t, err)
					log := usageRepo.lastLog
					require.NotNil(t, log)
					uncachedInput := parsed.usage.InputTokens - parsed.usage.CacheReadInputTokens
					totalCost := float64(uncachedInput)*3e-6 + float64(parsed.usage.OutputTokens)*9e-6 + float64(parsed.usage.CacheReadInputTokens)*0.5e-6
					require.Positive(t, totalCost)
					require.Equal(t, uncachedInput, log.InputTokens)
					require.Equal(t, parsed.usage.OutputTokens, log.OutputTokens)
					require.Equal(t, parsed.usage.CacheReadInputTokens, log.CacheReadTokens)
					require.Equal(t, multiplier, log.RateMultiplier)
					require.InDelta(t, totalCost, log.TotalCost, 1e-12)
					require.InDelta(t, totalCost*multiplier, log.ActualCost, 1e-12)
					require.InDelta(t, totalCost*multiplier, userRepo.lastAmount, 1e-12)
					require.Equal(t, 1, userRepo.deductCalls)
					require.Equal(t, 1, usageRepo.calls)
					if observed {
						require.Equal(t, 6, log.InputTokens)
						require.Equal(t, 11, log.CacheReadTokens)
					} else {
						require.Zero(t, log.CacheReadTokens)
					}
				})
			}
		}
	}
}
