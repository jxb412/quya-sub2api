//go:build unit

package service

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/stretchr/testify/require"
)

func gpt62CatalogForTest(t *testing.T) *PricingService {
	t.Helper()
	body, err := os.ReadFile(filepath.Join("..", "..", "resources", "model-pricing", "model_prices_and_context_window.json"))
	require.NoError(t, err)
	svc := &PricingService{}
	svc.pricingData, err = svc.parsePricingData(body)
	require.NoError(t, err)
	return svc
}

func TestGPT62PricingCatalogAndFallbackStandardCharges(t *testing.T) {
	// A cached remote catalog can contain generic/older GPT cards while missing
	// these new names. Neither of those cards may determine GPT-6.2 charges.
	staleCatalog := &PricingService{pricingData: map[string]*LiteLLMModelPricing{
		"gpt-6.2":       {InputCostPerToken: 99e-6, OutputCostPerToken: 99e-6},
		"gpt-6-astra":   {InputCostPerToken: 10e-6, OutputCostPerToken: 50e-6},
		"gpt-5.1-codex": {InputCostPerToken: 1e-6, OutputCostPerToken: 5e-6},
	}}
	for source, catalog := range map[string]*PricingService{
		"bundled": gpt62CatalogForTest(t), "no_service": nil,
		"empty_catalog": {}, "stale_remote": staleCatalog,
	} {
		billing := NewBillingService(&config.Config{}, catalog)
		for _, tc := range []struct {
			model               string
			input, output, read float64
		}{
			{"gpt-6.2-astra", 8, 40, .4},
			{"gpt-6.2-sol", 2, 10, .1},
		} {
			t.Run(source+"/"+tc.model, func(t *testing.T) {
				require.True(t, billing.HasIdentifiedTokenPricing(tc.model))
				cost, err := billing.CalculateCost(tc.model, UsageTokens{
					InputTokens: 80_000, OutputTokens: 12_000, CacheReadTokens: 20_000,
				}, 1.2)
				require.NoError(t, err)
				require.InDelta(t, .08*tc.input, cost.InputCost, 1e-12)
				require.InDelta(t, .012*tc.output, cost.OutputCost, 1e-12)
				require.InDelta(t, .02*tc.read, cost.CacheReadCost, 1e-12)
				want := .08*tc.input + .012*tc.output + .02*tc.read
				require.InDelta(t, want, cost.TotalCost, 1e-12)
				require.InDelta(t, want*1.2, cost.ActualCost, 1e-12)
				require.False(t, cost.LongContextBillingApplied)

				pricing, err := billing.GetModelPricing(tc.model)
				require.NoError(t, err)
				require.InDelta(t, tc.input*2e-6, pricing.InputPricePerTokenPriority, 1e-15)
				require.InDelta(t, tc.output*2e-6, pricing.OutputPricePerTokenPriority, 1e-15)
				require.InDelta(t, tc.read*2e-6, pricing.CacheReadPricePerTokenPriority, 1e-15)
				require.InDelta(t, tc.input*1.25e-6, pricing.CacheCreationPricePerToken, 1e-15)
				require.Equal(t, 272_000, pricing.LongContextInputThreshold)
			})
		}
	}
}

func TestGPT62PricingFallbackDoesNotRecognizeUnknownSuffixes(t *testing.T) {
	catalog := gpt62CatalogForTest(t)
	billing := NewBillingService(&config.Config{}, catalog)
	for _, name := range []string{
		"gpt-6.2", "gpt-6.2-astra-preview", "gpt-6.2-sol-preview", "gpt-6.2-solitude", "not-gpt-6.2-astra",
	} {
		t.Run(name, func(t *testing.T) {
			require.Nil(t, catalog.GetIdentifiedModelPricing(name))
			require.False(t, billing.HasIdentifiedTokenPricing(name))
			require.Nil(t, billing.getFallbackPricing(name))
		})
	}
}

func TestGPT62PricingRemoteAndFileOverridePrecedence(t *testing.T) {
	for _, remotePresent := range []bool{false, true} {
		for _, overridePresent := range []bool{false, true} {
			cfg := &config.Config{}
			cfg.Pricing.FallbackFile = filepath.Join("..", "..", "resources", "model-pricing", "model_prices_and_context_window.json")
			if overridePresent {
				cfg.Pricing.OverrideFile = filepath.Join(t.TempDir(), "pricing-override.json")
				// A partial override must preserve its base input/output prices,
				// including when the new model exists only in the bundled catalog.
				require.NoError(t, os.WriteFile(cfg.Pricing.OverrideFile, []byte(`{
					"gpt-6.2-astra":{"cache_read_input_token_cost":0},
					"gpt-6.2-sol":{"cache_read_input_token_cost":0}
				}`), 0600))
			}
			// Real cached catalogs remain valid while lacking the new models.
			body := []byte(`{"gpt-5.1-codex":{"input_cost_per_token":0.000001,"output_cost_per_token":0.000005}}`)
			if remotePresent {
				body = []byte(`{
					"gpt-6.2-astra":{"input_cost_per_token":0.000009,"output_cost_per_token":0.000045,"cache_read_input_token_cost":0.0000009},
					"gpt-6.2-sol":{"input_cost_per_token":0.000003,"output_cost_per_token":0.000015,"cache_read_input_token_cost":0.0000003}
				}`)
			}
			catalog := &PricingService{cfg: cfg}
			var err error
			catalog.pricingData, _, err = catalog.buildPricingData(body)
			require.NoError(t, err)
			billing := NewBillingService(cfg, catalog)
			for _, tc := range []struct {
				model                   string
				bundledCost, remoteCost float64
				bundledRead, remoteRead float64
			}{
				{"gpt-6.2-astra", 48, 54, .4, .9},
				{"gpt-6.2-sol", 12, 18, .1, .3},
			} {
				want, read := tc.bundledCost, tc.bundledRead
				if remotePresent {
					want, read = tc.remoteCost, tc.remoteRead
				}
				if overridePresent {
					read = 0
				}
				cost, err := billing.CalculateCost(tc.model, UsageTokens{
					InputTokens: 100_000, OutputTokens: 100_000, CacheReadTokens: 100_000,
				}, 1)
				require.NoError(t, err)
				require.InDelta(t, read*.1, cost.CacheReadCost, 1e-12)
				require.InDelta(t, (want+read)*.1, cost.ActualCost, 1e-12)
			}
		}
	}
}

func TestGPT62PricingGroupAndChannelOverridesRemainAuthoritative(t *testing.T) {
	ctx := context.Background()
	const groupID int64 = 100
	for _, model := range []string{"gpt-6.2-astra", "gpt-6.2-sol"} {
		t.Run(model, func(t *testing.T) {
			input, output, read := 7e-6, 35e-6, .7e-6
			repo := &mockChannelRepository{
				listAllFn: func(context.Context) ([]Channel, error) {
					return []Channel{{ID: 1, Status: StatusActive, GroupIDs: []int64{groupID}, ModelPricing: []ChannelModelPricing{{
						Platform: PlatformOpenAI, Models: []string{model}, BillingMode: BillingModeToken,
						InputPrice: &input, OutputPrice: &output, CacheReadPrice: &read,
					}}}}, nil
				},
				getGroupPlatformsFn: func(context.Context, []int64) (map[int64]string, error) {
					return map[int64]string{groupID: PlatformOpenAI}, nil
				},
			}
			billing := NewBillingService(&config.Config{}, gpt62CatalogForTest(t))
			resolver := NewModelPricingResolver(NewChannelService(repo, nil, nil, nil, nil), billing)
			id := groupID
			group := &Group{ID: id, LongContextPricingEnabled: true}
			costInput := CostInput{Ctx: ctx, Model: model, GroupID: &id, Group: group, Resolver: resolver,
				Tokens: UsageTokens{InputTokens: 80_000, OutputTokens: 12_000, CacheReadTokens: 20_000}, RateMultiplier: 1.2}
			cost, err := billing.CalculateCostUnified(costInput)
			require.NoError(t, err)
			require.InDelta(t, .994*1.2, cost.ActualCost, 1e-12)

			groupInput, groupOutput, groupRead := 3e-6, 15e-6, 0.0
			group.ModelPricing = []ChannelModelPricing{{Models: []string{model}, BillingMode: BillingModeToken,
				InputPrice: &groupInput, OutputPrice: &groupOutput, CacheReadPrice: &groupRead}}
			cost, err = billing.CalculateCostUnified(costInput)
			require.NoError(t, err)
			require.Zero(t, cost.CacheReadCost)
			require.InDelta(t, .42*1.2, cost.ActualCost, 1e-12)
			require.False(t, cost.LongContextBillingApplied)
		})
	}
}
