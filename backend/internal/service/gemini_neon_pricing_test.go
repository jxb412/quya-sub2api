//go:build unit

package service

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/stretchr/testify/require"
)

func TestGeminiNeonAndHydrogenPricingAliases(t *testing.T) {
	for _, base := range []string{"gemini-4-neon", "gemini-4-hydrogen"} {
		basePrice := &LiteLLMModelPricing{InputCostPerToken: 3e-6}
		svc := &PricingService{pricingData: map[string]*LiteLLMModelPricing{base: basePrice}}
		for _, suffix := range []string{"", "-low", "-medium", "-high", "-xhigh", "-tiered", "-tiered-high", "-tiered-xhigh", "-sonar"} {
			for _, prefix := range []string{"", "models/", "google/", "publishers/google/models/", "projects/p/locations/global/publishers/google/models/"} {
				model := prefix + base + suffix
				t.Run(model, func(t *testing.T) {
					require.Same(t, basePrice, svc.GetModelPricing(model))
					require.Same(t, basePrice, svc.GetIdentifiedModelPricing("  "+strings.ToUpper(model)+"  "))
				})
			}
		}

		// Exact prices win over the normalized base, including provider namespaces.
		tierPrice := &LiteLLMModelPricing{InputCostPerToken: 7e-6}
		svc.pricingData[base+"-xhigh"] = tierPrice
		for _, prefix := range []string{"", "models/", "google/", "projects/p/locations/global/publishers/google/models/"} {
			require.Same(t, tierPrice, svc.GetModelPricing(prefix+base+"-xhigh"))
			require.Same(t, tierPrice, svc.GetIdentifiedModelPricing(prefix+base+"-xhigh"))
		}
		namespacedPrice := &LiteLLMModelPricing{InputCostPerToken: 9e-6}
		svc.pricingData["models/"+base+"-xhigh"] = namespacedPrice
		require.Same(t, namespacedPrice, svc.GetModelPricing("models/"+base+"-xhigh"))
		require.Same(t, tierPrice, svc.GetModelPricing(base+"-xhigh"))
	}
}

func TestGeminiNeonAndHydrogenCatalogAndFallbackBilling(t *testing.T) {
	tokens := UsageTokens{InputTokens: 800_000, OutputTokens: 12_000, CacheReadTokens: 200_000, CacheCreationTokens: 1000}
	for source, catalog := range map[string]*PricingService{
		"catalog": gemini4CatalogForTest(t), "missing_remote": nil,
		"stale_remote": {pricingData: map[string]*LiteLLMModelPricing{}},
	} {
		billing := NewBillingService(&config.Config{}, catalog)
		for _, tc := range []struct {
			base                string
			input, output, read float64
		}{{"gemini-4-neon", 3, 15, .15}, {"gemini-4-hydrogen", 1.5, 7.5, .075}} {
			for _, suffix := range []string{"", "-low", "-medium", "-high", "-xhigh", "-tiered", "-tiered-high", "-tiered-xhigh", "-sonar"} {
				for _, prefix := range []string{"", "models/", "google/", "projects/p/locations/global/publishers/google/models/"} {
					model := prefix + tc.base + suffix
					t.Run(source+"/"+model, func(t *testing.T) {
						for _, tier := range []struct {
							name string
							rate float64
						}{{"", 1}, {"priority", 1.8}} {
							cost, err := billing.CalculateCostWithServiceTier(model, tokens, 1.2, tier.name)
							require.NoError(t, err)
							require.InDelta(t, .8*tc.input*tier.rate, cost.InputCost, 1e-12)
							require.InDelta(t, .012*tc.output*tier.rate, cost.OutputCost, 1e-12)
							require.InDelta(t, .2*tc.read*tier.rate, cost.CacheReadCost, 1e-12)
							want := (.8*tc.input + .012*tc.output + .2*tc.read) * tier.rate
							require.InDelta(t, want, cost.TotalCost, 1e-12)
							require.InDelta(t, want*1.2, cost.ActualCost, 1e-12)
							require.Zero(t, cost.CacheCreationCost)
							require.False(t, cost.LongContextBillingApplied)
						}
						require.Zero(t, billing.CacheStoragePricePerTokenHour(model))
					})
				}
			}
		}
	}
}

func TestDefaultPricingIncludesGeminiNeon(t *testing.T) {
	catalog := gemini4CatalogForTest(t)
	pricing := catalog.GetIdentifiedModelPricing("gemini-4-neon")
	require.NotNil(t, pricing)
	require.Equal(t, "gemini", pricing.LiteLLMProvider)
	require.InDelta(t, 3e-6, pricing.InputCostPerToken, 1e-15)
	require.InDelta(t, 15e-6, pricing.OutputCostPerToken, 1e-15)
	require.InDelta(t, .15e-6, pricing.CacheReadInputTokenCost, 1e-15)
	require.InDelta(t, 5.4e-6, pricing.InputCostPerTokenPriority, 1e-15)
	require.InDelta(t, 27e-6, pricing.OutputCostPerTokenPriority, 1e-15)
	require.InDelta(t, .27e-6, pricing.CacheReadInputTokenCostPriority, 1e-15)
	require.Zero(t, pricing.CacheCreationInputTokenCost)
	require.Zero(t, pricing.CacheCreationInputTokenCostPriority)
	require.Zero(t, pricing.CacheStorageCostPerTokenPerHour)
	require.Zero(t, pricing.LongContextInputTokenThreshold)
	require.Contains(t, catalog.ListModelNamesByProvider("gemini"), "gemini-4-neon")

	body, err := os.ReadFile(filepath.Join("..", "..", "resources", "model-pricing", "model_prices_and_context_window.json"))
	require.NoError(t, err)
	var raw map[string]map[string]any
	require.NoError(t, json.Unmarshal(body, &raw))
	require.Equal(t, float64(1048576), raw["gemini-4-neon"]["max_input_tokens"])
	require.Equal(t, float64(65536), raw["gemini-4-neon"]["max_output_tokens"])
	require.Equal(t, float64(65536), raw["gemini-4-neon"]["max_tokens"])
}

func TestGeminiNeonPricingBundledFallbackCompletesStaleCatalog(t *testing.T) {
	cfg := &config.Config{}
	cfg.Pricing.FallbackFile = filepath.Join("..", "..", "resources", "model-pricing", "model_prices_and_context_window.json")
	catalog := &PricingService{cfg: cfg}
	var err error
	// Existing downloaded rates stay authoritative while bundled resources add Neon.
	catalog.pricingData, _, err = catalog.buildPricingData([]byte(`{
		"gemini-4-hydrogen": {"input_cost_per_token": 9e-6, "output_cost_per_token": 45e-6, "litellm_provider": "gemini"}
	}`))
	require.NoError(t, err)
	require.InDelta(t, 9e-6, catalog.GetModelPricing("gemini-4-hydrogen-xhigh").InputCostPerToken, 1e-15)
	require.Contains(t, catalog.ListModelNamesByProvider("gemini"), "gemini-4-neon")

	models := []SupportedModel{{Name: "gemini-4-neon-high"}}
	fillGlobalPricingFallback(catalog, models)
	require.NotNil(t, models[0].Pricing)
	require.NotNil(t, models[0].Pricing.InputPrice)
	require.InDelta(t, 3e-6, *models[0].Pricing.InputPrice, 1e-15)

	// The admin default-price endpoint reads the same BillingService method.
	pricing, err := NewBillingService(cfg, catalog).GetModelPricing("gemini-4-neon-high")
	require.NoError(t, err)
	require.InDelta(t, 3e-6, pricing.InputPricePerToken, 1e-15)
	require.InDelta(t, 15e-6, pricing.OutputPricePerToken, 1e-15)
	require.InDelta(t, .15e-6, pricing.CacheReadPricePerToken, 1e-15)
}

func TestGeminiNeonAndHydrogenPricingRejectsUnknownVariants(t *testing.T) {
	catalog := gemini4CatalogForTest(t)
	for _, base := range []string{"gemini-4-neon", "gemini-4-hydrogen"} {
		for _, model := range []string{
			base + "-max", base + "-cyber", base + "-preview", base + "-image",
			base + "-high-preview", base + "-tiered-max", base + "ium", "not-" + base,
		} {
			for _, prefix := range []string{"", "models/"} {
				t.Run(prefix+model, func(t *testing.T) {
					require.Equal(t, model, normalizeGeminiThinkingTierAlias(model))
					require.Nil(t, catalog.GetIdentifiedModelPricing(prefix+model))
					for _, prices := range []*PricingService{catalog, nil} {
						billing := NewBillingService(&config.Config{}, prices)
						cost, err := billing.CalculateCost(prefix+model, UsageTokens{InputTokens: 1000}, 1)
						require.ErrorIs(t, err, ErrModelPricingUnavailable)
						require.Nil(t, cost)
					}
				})
			}
		}
	}
	for _, model := range []string{"gemini-4-carbon-xhigh", "gemini-4-argon-sonar", "gemini-3.8-flash-tiered-xhigh"} {
		require.Equal(t, model, normalizeGeminiThinkingTierAlias(model), "older families retain their existing aliases")
	}
}

func TestGeminiNeonAndHydrogenOverrideFileRemainsAuthoritative(t *testing.T) {
	body, err := os.ReadFile(filepath.Join("..", "..", "resources", "model-pricing", "model_prices_and_context_window.json"))
	require.NoError(t, err)
	for _, base := range []string{"gemini-4-neon", "gemini-4-hydrogen"} {
		t.Run(base, func(t *testing.T) {
			cfg := &config.Config{}
			cfg.Pricing.OverrideFile = filepath.Join(t.TempDir(), "override.json")
			override := map[string]any{
				base: map[string]any{"input_cost_per_token": 5e-6, "output_cost_per_token": 25e-6, "cache_read_input_token_cost": 0},
				base + "-xhigh": map[string]any{
					"input_cost_per_token": 7e-6, "output_cost_per_token": 35e-6, "cache_read_input_token_cost": .7e-6,
					"input_cost_per_token_priority": 11e-6, "output_cost_per_token_priority": 55e-6, "cache_read_input_token_cost_priority": 1.1e-6,
				},
				base + "-cyber": map[string]any{"input_cost_per_token": 9e-6, "output_cost_per_token": 45e-6},
			}
			patch, err := json.Marshal(override)
			require.NoError(t, err)
			require.NoError(t, os.WriteFile(cfg.Pricing.OverrideFile, patch, 0600))
			catalog := &PricingService{cfg: cfg}
			catalog.pricingData, _, err = catalog.buildPricingData(body)
			require.NoError(t, err)
			billing := NewBillingService(cfg, catalog)
			tokens := UsageTokens{InputTokens: 1_000_000, OutputTokens: 1_000_000, CacheReadTokens: 1_000_000}
			for _, tc := range []struct {
				suffix, tier string
				want         float64
			}{{"-low", "", 30}, {"-xhigh", "", 42.7}, {"-xhigh", "priority", 67.1}, {"-cyber", "", 54}} {
				for _, prefix := range []string{"", "models/", "google/", "projects/p/locations/global/publishers/google/models/"} {
					cost, err := billing.CalculateCostWithServiceTier(prefix+base+tc.suffix, tokens, 1, tc.tier)
					require.NoError(t, err)
					require.InDelta(t, tc.want, cost.ActualCost, 1e-12)
				}
			}
		})
	}
}
