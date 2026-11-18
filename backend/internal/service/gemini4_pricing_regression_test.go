//go:build unit

package service

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/stretchr/testify/require"
)

func gemini4CatalogForTest(t *testing.T) *PricingService {
	t.Helper()
	body, err := os.ReadFile(filepath.Join("..", "..", "resources", "model-pricing", "model_prices_and_context_window.json"))
	require.NoError(t, err)
	svc := &PricingService{}
	svc.pricingData, err = svc.parsePricingData(body)
	require.NoError(t, err)
	return svc
}

// Exercise charged amounts, including cached input and the group multiplier,
// at a context size above the historical Gemini Pro 200k price boundary.
func TestGemini4PricingCatalogAndFallbackChargeAllRequestedModels(t *testing.T) {
	models := []struct {
		name                string
		input, output, read float64
	}{
		{"gemini-4-argon", 2, 10, .1},
		{"gemini-4-argon-high", 2, 10, .1},
		{"gemini-4-argon-low", 2, 10, .1},
		{"gemini-4-argon-medium", 2, 10, .1},
		{"gemini-4-carbon", 2, 10, .1},
		{"gemini-4-carbon-high", 2, 10, .1},
		{"gemini-4-carbon-low", 2, 10, .1},
		{"gemini-4-carbon-medium", 2, 10, .1},
		{"gemini-4-hydrogen", 1.5, 7.5, .075},
		{"gemini-4-hydrogen-high", 1.5, 7.5, .075},
		{"gemini-4-hydrogen-low", 1.5, 7.5, .075},
		{"gemini-4-hydrogen-medium", 1.5, 7.5, .075},
		{"gemini-4-hydrogen-xhigh", 1.5, 7.5, .075},
		{"gemini-4-argon-cyber-permissive", 4, 20, .2},
	}
	for source, catalog := range map[string]*PricingService{"catalog": gemini4CatalogForTest(t), "missing_remote": nil} {
		svc := NewBillingService(&config.Config{}, catalog)
		for _, tc := range models {
			for _, prefix := range []string{"", "models/", "projects/p/locations/global/publishers/google/models/"} {
				t.Run(source+"/"+prefix+tc.name, func(t *testing.T) {
					model := prefix + tc.name
					for _, tier := range []struct {
						name string
						rate float64
					}{{"", 1}, {"priority", 1.8}} {
						// Upstream v0.2.23 only supplies explicit Priority cards for
						// Carbon/Hydrogen in the hardcoded fallback. Argon/Cyber
						// retain the existing generic 2x tier when no catalog exists.
						if source == "missing_remote" && tier.name == "priority" && strings.HasPrefix(tc.name, "gemini-4-argon") {
							tier.rate = 2
						}
						cost, err := svc.CalculateCostWithServiceTier(model, UsageTokens{
							InputTokens: 800_000, OutputTokens: 12_000, CacheReadTokens: 200_000,
						}, 1.2, tier.name)
						require.NoError(t, err)
						require.InDelta(t, .8*tc.input*tier.rate, cost.InputCost, 1e-12)
						require.InDelta(t, .012*tc.output*tier.rate, cost.OutputCost, 1e-12)
						require.InDelta(t, .2*tc.read*tier.rate, cost.CacheReadCost, 1e-12)
						want := (.8*tc.input + .012*tc.output + .2*tc.read) * tier.rate
						require.InDelta(t, want, cost.TotalCost, 1e-12)
						require.InDelta(t, want*1.2, cost.ActualCost, 1e-12)
						require.False(t, cost.LongContextBillingApplied)
					}
					require.Zero(t, svc.CacheStoragePricePerTokenHour(model))
				})
			}
		}
	}
}

func TestGemini4CarbonPricingRejectsUnknownFamilySuffixes(t *testing.T) {
	catalog := gemini4CatalogForTest(t)
	for _, svc := range []*BillingService{NewBillingService(&config.Config{}, catalog), NewBillingService(&config.Config{}, nil)} {
		for _, name := range []string{
			"gemini-4-carbon-xhigh", "gemini-4-carbon-cyber", "gemini-4-carbon-high-preview",
			"not-gemini-4-carbon", "gemini-4-carbonium", "models/gemini-4-carbon-image",
		} {
			t.Run(name, func(t *testing.T) {
				require.Nil(t, catalog.GetIdentifiedModelPricing(name))
				cost, err := svc.CalculateCost(name, UsageTokens{InputTokens: 1000}, 1)
				require.ErrorIs(t, err, ErrModelPricingUnavailable)
				require.Nil(t, cost)
			})
		}
	}
}

func TestGemini4PricingExplicitTierAndBaseOverridesRemainAuthoritative(t *testing.T) {
	for _, base := range []string{"gemini-4-argon", "gemini-4-carbon"} {
		t.Run(base, func(t *testing.T) {
			catalog := gemini4CatalogForTest(t)
			// A deployment override or later remote price must supersede bundled rates.
			catalog.pricingData[base] = &LiteLLMModelPricing{InputCostPerToken: 5e-6, OutputCostPerToken: 25e-6, CacheReadInputTokenCost: .5e-6}
			catalog.pricingData[base+"-high"] = &LiteLLMModelPricing{InputCostPerToken: 7e-6, OutputCostPerToken: 35e-6, CacheReadInputTokenCost: .7e-6}
			svc := NewBillingService(&config.Config{}, catalog)
			for _, tc := range []struct {
				suffix string
				want   float64
			}{{"-low", 30.5}, {"-high", 42.7}} {
				for _, prefix := range []string{"", "models/", "projects/p/locations/global/publishers/google/models/"} {
					cost, err := svc.CalculateCost(prefix+base+tc.suffix, UsageTokens{InputTokens: 1_000_000, OutputTokens: 1_000_000, CacheReadTokens: 1_000_000}, 1)
					require.NoError(t, err)
					require.InDelta(t, tc.want, cost.ActualCost, 1e-12)
				}
			}
		})
	}
}

func TestGemini4PricingGroupAndChannelOverrideOfficialCharges(t *testing.T) {
	ctx := context.Background()
	const groupID int64 = 100
	for _, model := range []string{"gemini-4-argon-medium", "gemini-4-carbon-high", "gemini-4-hydrogen-xhigh"} {
		t.Run(model, func(t *testing.T) {
			input, output, read := 7e-6, 35e-6, .7e-6
			repo := &mockChannelRepository{
				listAllFn: func(context.Context) ([]Channel, error) {
					return []Channel{{ID: 1, Status: StatusActive, GroupIDs: []int64{groupID}, ModelPricing: []ChannelModelPricing{{
						Platform: PlatformGemini, Models: []string{model}, BillingMode: BillingModeToken,
						InputPrice: &input, OutputPrice: &output, CacheReadPrice: &read,
					}}}}, nil
				},
				getGroupPlatformsFn: func(context.Context, []int64) (map[int64]string, error) {
					return map[int64]string{groupID: PlatformGemini}, nil
				},
			}
			billing := NewBillingService(&config.Config{}, gemini4CatalogForTest(t))
			resolver := NewModelPricingResolver(NewChannelService(repo, nil, nil, nil, nil), billing)
			id := groupID
			group := &Group{ID: id, LongContextPricingEnabled: true}
			inputCost := CostInput{Ctx: ctx, Model: model, GroupID: &id, Group: group, Resolver: resolver,
				Tokens: UsageTokens{InputTokens: 800_000, OutputTokens: 12_000, CacheReadTokens: 200_000}, RateMultiplier: 1.2}
			resolved := resolver.Resolve(ctx, PricingInput{Model: model, GroupID: &id, Group: group})
			require.Equal(t, PricingSourceChannel, resolved.Source)
			cost, err := billing.CalculateCostUnified(inputCost)
			require.NoError(t, err)
			require.InDelta(t, 6.16*1.2, cost.ActualCost, 1e-12)

			// Explicit free cached input remains zero; group rates override the channel.
			groupInput, groupOutput, groupRead := 3e-6, 15e-6, 0.0
			group.ModelPricing = []ChannelModelPricing{{Models: []string{model}, BillingMode: BillingModeToken,
				InputPrice: &groupInput, OutputPrice: &groupOutput, CacheReadPrice: &groupRead}}
			resolved = resolver.Resolve(ctx, PricingInput{Model: model, GroupID: &id, Group: group})
			require.Equal(t, PricingSourceGroup, resolved.Source)
			cost, err = billing.CalculateCostUnified(inputCost)
			require.NoError(t, err)
			require.Zero(t, cost.CacheReadCost)
			require.InDelta(t, 2.58*1.2, cost.ActualCost, 1e-12)
			require.False(t, cost.LongContextBillingApplied)
		})
	}
}
