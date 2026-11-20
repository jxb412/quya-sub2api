package service

import "strings"

// 2026-11-21：适配上游 PR #8960；只有管理员显式配置基础模型及档位自映射时，
// Codex 才合并目录并公布可用的推理档位。固定别名、独立路由和仅档位目录保持原样，
// 避免新增 Neon/Hydrogen 后误改现有账号映射或 Quya 的跨平台调度策略。
var codexAntigravityGeminiTiers = [...]string{"low", "medium", "high", "xhigh"}

func codexAntigravityGeminiTier(model string) (base, tier string, ok bool) {
	if !strings.HasPrefix(model, "gemini-") {
		return "", "", false
	}
	for _, suffix := range []string{"tiered-xhigh", "tiered-high", "low", "medium", "high", "xhigh"} {
		if base, ok := strings.CutSuffix(model, "-"+suffix); ok {
			return base, strings.TrimPrefix(suffix, "tiered-"), true
		}
	}
	return "", "", false
}

func hasConfiguredAntigravitySelfMapping(account *Account, model string) bool {
	if account.Platform != PlatformAntigravity {
		return false
	}
	mapping, _ := account.Credentials["model_mapping"].(map[string]any)
	target, _ := mapping[model].(string)
	return strings.TrimSpace(target) == model && account.GetModelMapping()[model] == model
}

// codexAntigravityTierMetadata describes a self-mapped base. An explicit
// mapping to another model still describes that single target.
func codexAntigravityTierMetadata(account *Account, base string) (UpstreamModelMetadata, bool) {
	if !strings.HasPrefix(base, "gemini-") || !hasConfiguredAntigravitySelfMapping(account, base) {
		return UpstreamModelMetadata{}, false
	}
	levels := make([]string, 0, len(codexAntigravityGeminiTiers))
	var candidates []UpstreamModelMetadata
	for _, tier := range codexAntigravityGeminiTiers {
		target, ok := resolveGeminiThinkingVariantForLevel(account, base, tier)
		targetBase, targetTier, isTier := codexAntigravityGeminiTier(target)
		if !ok || !isTier || targetBase != base || targetTier != tier {
			continue
		}
		levels = append(levels, tier)
		metadata, _ := account.GetUpstreamModelMetadata(target)
		candidates = append(candidates, metadata)
	}
	if len(levels) == 0 {
		return UpstreamModelMetadata{}, false
	}

	configuredBase, _ := account.GetUpstreamModelMetadata(base)
	metadata := intersectCodexAntigravityTierMetadata(base, append([]UpstreamModelMetadata{configuredBase}, candidates...))
	metadata.DisplayName = configuredBase.DisplayName
	metadata.Description = configuredBase.Description
	reasoning := true
	metadata.Reasoning = &reasoning
	metadata.SupportedReasoningLevels = levels
	metadata.DefaultReasoningLevel = levels[0]
	if target, ok := resolveGeminiThinkingVariantForLevel(account, base, ""); ok {
		if _, tier, _ := codexAntigravityGeminiTier(target); stringSliceContains(levels, tier) {
			metadata.DefaultReasoningLevel = tier
		}
	}
	return metadata, true
}

// Missing tier fields do not erase known limits on the base model. Every known
// tier bound still narrows the family, including snapshots that omit other fields.
func intersectCodexAntigravityTierMetadata(base string, candidates []UpstreamModelMetadata) UpstreamModelMetadata {
	metadata := intersectUpstreamModelMetadata(base, candidates).UpstreamModelMetadata
	metadata.ContextWindow, metadata.MaxContextWindow, metadata.MaxOutputTokens = 0, 0, 0
	metadata.InputModalities = nil
	modalitiesKnown := false
	minimumKnown := func(current, candidate int64) int64 {
		if candidate > 0 && (current <= 0 || candidate < current) {
			return candidate
		}
		return current
	}
	for _, candidate := range candidates {
		metadata.ContextWindow = minimumKnown(metadata.ContextWindow, candidate.ContextWindow)
		maxContext := candidate.MaxContextWindow
		if maxContext <= 0 {
			maxContext = candidate.ContextWindow
		}
		metadata.MaxContextWindow = minimumKnown(metadata.MaxContextWindow, maxContext)
		metadata.MaxOutputTokens = minimumKnown(metadata.MaxOutputTokens, candidate.MaxOutputTokens)
		if modalities := normalizeCodexInputModalities(candidate.InputModalities); len(modalities) > 0 {
			if !modalitiesKnown {
				metadata.InputModalities = modalities
				modalitiesKnown = true
			} else {
				metadata.InputModalities = intersectOrderedStrings(metadata.InputModalities, modalities)
			}
		}
	}
	if metadata.MaxContextWindow > 0 {
		metadata.ContextWindow = minimumKnown(metadata.ContextWindow, metadata.MaxContextWindow)
	}
	if modalitiesKnown && len(metadata.InputModalities) == 0 {
		// Preserve the existing fail-closed text descriptor for conflicting snapshots.
		metadata.InputModalities = []string{"text"}
	}
	return metadata
}

func foldCodexAntigravityGeminiTiers(platform string, modelIDs []string, accounts []Account, group *Group, routes []CompositeModelRoute) []string {
	present := make(map[string]bool, len(modelIDs))
	for _, model := range modelIDs {
		present[strings.TrimSpace(model)] = true
	}
	out := make([]string, 0, len(modelIDs))
	for _, model := range modelIDs {
		modelID := strings.TrimSpace(model)
		base, tier, ok := codexAntigravityGeminiTier(modelID)
		if !ok || !present[base] || !canFoldCodexAntigravityTier(platform, modelID, base, tier, accounts, group, routes) {
			out = append(out, model)
		}
	}
	return out
}

func canFoldCodexAntigravityTier(platform, model, base, tier string, accounts []Account, group *Group, routes []CompositeModelRoute) bool {
	for _, id := range []string{model, base} {
		if len(codexModelRoutingAccountIDs(group, id)) > 0 {
			return false
		}
		if codexCompositeRouteMatchesModel(routes, id) {
			return false
		}
		if platform == PlatformComposite {
			targetPlatform, _, resolved := resolveCodexCompositeModelTarget(id, accounts, routes, true)
			if !resolved || targetPlatform != PlatformAntigravity {
				return false
			}
		}
	}
	if platform == PlatformComposite {
		platform = PlatformAntigravity
	}
	matched := false
	for i := range accounts {
		account := &accounts[i]
		if !codexAccountServesPlatformModel(account, platform, model) {
			continue
		}
		if !account.IsModelSupported(base) && !account.IsModelSupported(model) {
			continue
		}
		if !hasConfiguredAntigravitySelfMapping(account, base) || !hasConfiguredAntigravitySelfMapping(account, model) {
			return false
		}
		if target, ok := resolveGeminiThinkingVariantForLevel(account, base, tier); !ok || target != model {
			return false
		}
		matched = true
	}
	return matched
}
