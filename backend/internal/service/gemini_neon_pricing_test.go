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

// Hydrogen's remote card as observed on 2026-11-21 incorrectly charges writes
// at the input rate despite Google's published implicit-caching-only policy.
const geminiHydrogenRemoteCachePriceJSON = `{
	"input_cost_per_token": 1.5e-6, "input_cost_per_token_priority": 2.7e-6,
	"output_cost_per_token": 7.5e-6, "output_cost_per_token_priority": 13.5e-6,
	"cache_read_input_token_cost": 0.075e-6, "cache_read_input_token_cost_priority": 0.135e-6,
	"cache_creation_input_token_cost": 1.5e-6, "cache_creation_input_token_cost_priority": 2.7e-6,
	"litellm_provider": "gemini", "supports_prompt_caching": true, "supports_service_tier": true
}`

func TestGeminiImplicitCachingIgnoresRemoteWriteAndStoragePrices(t *testing.T) {
	for _, base := range []string{"gemini-4-hydrogen", "gemini-4-neon"} {
		var fields map[string]any
		require.NoError(t, json.Unmarshal([]byte(geminiHydrogenRemoteCachePriceJSON), &fields))
		// Include the other supported explicit-cache fields to guard against a
		// later remote catalog adding storage or TTL prices for these models.
		fields["cache_creation_input_token_cost_above_1hr"] = 4e-6
		fields["cache_storage_cost_per_token_per_hour"] = 5e-6
		factor := 1.0
		if base == "gemini-4-neon" {
			factor = 2
			for _, key := range []string{"input_cost_per_token", "input_cost_per_token_priority", "output_cost_per_token", "output_cost_per_token_priority", "cache_read_input_token_cost", "cache_read_input_token_cost_priority"} {
				fields[key] = fields[key].(float64) * factor
			}
		}
		raw := make(map[string]any)
		for _, suffix := range []string{"", "-low", "-medium", "-high", "-xhigh", "-tiered", "-tiered-high", "-tiered-xhigh", "-sonar"} {
			for _, prefix := range []string{"", "models/", "google/", "projects/p/locations/global/publishers/google/models/"} {
				raw[prefix+base+suffix] = fields
			}
		}
		body, err := json.Marshal(raw)
		require.NoError(t, err)
		catalog := &PricingService{}
		catalog.pricingData, err = catalog.parsePricingData(body)
		require.NoError(t, err)
		billing := NewBillingService(&config.Config{}, catalog)
		for model := range raw {
			t.Run(model, func(t *testing.T) {
				parsed := catalog.pricingData[model]
				require.NotNil(t, parsed)
				require.Zero(t, parsed.CacheCreationInputTokenCost)
				require.Zero(t, parsed.CacheCreationInputTokenCostPriority)
				require.Zero(t, parsed.CacheCreationInputTokenCostAbove1hr)
				require.Zero(t, parsed.CacheStorageCostPerTokenPerHour)
				require.False(t, parsed.CacheCreationInputTokenCostExplicit)
				// The admin default-price endpoint calls this same method.
				price, err := billing.GetModelPricing(model)
				require.NoError(t, err)
				require.Zero(t, price.CacheCreationPricePerToken)
				require.Zero(t, price.CacheCreationPricePerTokenPriority)
				require.Zero(t, price.CacheCreation5mPrice)
				require.Zero(t, price.CacheCreation1hPrice)
				require.False(t, price.SupportsCacheBreakdown)
				require.Zero(t, billing.CacheStoragePricePerTokenHour(model))
				for tier, rate := range map[string]float64{"": 1, "priority": 1.8} {
					cost, err := billing.CalculateCostWithServiceTier(model, UsageTokens{
						InputTokens: 1_000_000, OutputTokens: 1_000_000, CacheReadTokens: 1_000_000,
						CacheCreationTokens: 1_000_000, CacheCreation5mTokens: 300_000, CacheCreation1hTokens: 700_000,
					}, 1, tier)
					require.NoError(t, err)
					require.Zero(t, cost.CacheCreationCost)
					require.InDelta(t, 9.075*factor*rate, cost.TotalCost, 1e-12)
				}
			})
		}
	}
}

func TestGeminiImplicitCachingPreservesOtherModelCards(t *testing.T) {
	var fields map[string]any
	require.NoError(t, json.Unmarshal([]byte(geminiHydrogenRemoteCachePriceJSON), &fields))
	fields["cache_creation_input_token_cost_above_1hr"] = 4e-6
	fields["cache_storage_cost_per_token_per_hour"] = 5e-6
	raw := make(map[string]any)
	for _, model := range []string{"gemini-4-carbon", "gemini-2.5-flash", "gemini-4-neon-preview", "gemini-4-hydrogen-cyber", "not-gemini-4-neon"} {
		raw[model] = fields
	}
	body, err := json.Marshal(raw)
	require.NoError(t, err)
	data, err := (&PricingService{}).parsePricingData(body)
	require.NoError(t, err)
	for model, price := range data {
		require.InDelta(t, 1.5e-6, price.CacheCreationInputTokenCost, 1e-15, model)
		require.InDelta(t, 2.7e-6, price.CacheCreationInputTokenCostPriority, 1e-15, model)
		require.InDelta(t, 4e-6, price.CacheCreationInputTokenCostAbove1hr, 1e-15, model)
		require.InDelta(t, 5e-6, price.CacheStorageCostPerTokenPerHour, 1e-15, model)
		require.True(t, price.CacheCreationInputTokenCostExplicit, model)
	}
}

func TestGeminiImplicitCachingOverrideFileKeepsExplicitCacheRates(t *testing.T) {
	for _, base := range []string{"gemini-4-neon", "gemini-4-hydrogen"} {
		for _, tc := range []struct {
			name                          string
			patch                         map[string]float64
			write, priority, hour, stored float64
		}{
			{"write", map[string]float64{"cache_creation_input_token_cost": 7e-6}, 7e-6, 0, 0, 0},
			{"priority", map[string]float64{"cache_creation_input_token_cost_priority": 11e-6}, 0, 11e-6, 0, 0},
			{"hour", map[string]float64{"cache_creation_input_token_cost_above_1hr": 13e-6}, 0, 0, 13e-6, 0},
			{"storage", map[string]float64{"cache_storage_cost_per_token_per_hour": 17e-6}, 0, 0, 0, 17e-6},
			{"zero", map[string]float64{
				"cache_creation_input_token_cost": 0, "cache_creation_input_token_cost_priority": 0,
				"cache_creation_input_token_cost_above_1hr": 0, "cache_storage_cost_per_token_per_hour": 0,
			}, 0, 0, 0, 0},
		} {
			t.Run(base+"/"+tc.name, func(t *testing.T) {
				body, err := json.Marshal(map[string]json.RawMessage{base: json.RawMessage(geminiHydrogenRemoteCachePriceJSON)})
				require.NoError(t, err)
				patch, err := json.Marshal(map[string]any{
					base: tc.patch,
					base + "-xhigh": map[string]float64{
						"input_cost_per_token": 9e-6, "cache_creation_input_token_cost": 19e-6,
						"cache_creation_input_token_cost_priority": 23e-6, "cache_storage_cost_per_token_per_hour": 29e-6,
					},
				})
				require.NoError(t, err)
				catalog := newPricingServiceWithOverride(t, string(patch))
				catalog.pricingData, _, err = catalog.buildPricingData(body)
				require.NoError(t, err)
				billing := NewBillingService(&config.Config{}, catalog)
				model := "models/" + base + "-high"
				parsed := catalog.GetModelPricing(model)
				_, explicitWrite := tc.patch["cache_creation_input_token_cost"]
				require.Equal(t, explicitWrite, parsed.CacheCreationInputTokenCostExplicit)
				price, err := billing.GetModelPricing(model)
				require.NoError(t, err)
				require.InDelta(t, tc.write, price.CacheCreationPricePerToken, 1e-15)
				require.InDelta(t, tc.priority, price.CacheCreationPricePerTokenPriority, 1e-15)
				require.InDelta(t, tc.hour, price.CacheCreation1hPrice, 1e-15)
				require.InDelta(t, tc.stored, billing.CacheStoragePricePerTokenHour(model), 1e-15)
				for _, tier := range []string{"", "priority"} {
					wantWrite := tc.write
					if tier == "priority" && tc.priority > 0 {
						wantWrite = tc.priority
					}
					if tc.hour > tc.write {
						wantWrite = tc.hour
					}
					cost, err := billing.CalculateCostWithServiceTier(model, UsageTokens{CacheCreationTokens: 1_000_000, CacheCreation1hTokens: 1_000_000}, 1, tier)
					require.NoError(t, err)
					require.InDelta(t, wantWrite*1_000_000, cost.CacheCreationCost, 1e-12)
					// An override-only exact tier is added after default catalog normalization.
					tierCost, err := billing.CalculateCostWithServiceTier("google/"+base+"-xhigh", UsageTokens{CacheCreationTokens: 1_000_000}, 1, tier)
					require.NoError(t, err)
					wantTier := 19.0
					if tier == "priority" {
						wantTier = 23
					}
					require.InDelta(t, wantTier, tierCost.CacheCreationCost, 1e-12)
				}
				require.InDelta(t, 29e-6, billing.CacheStoragePricePerTokenHour("google/"+base+"-xhigh"), 1e-15)
			})
		}
	}
}
