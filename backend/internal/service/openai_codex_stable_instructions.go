package service

import (
	"strings"
	"time"

	"github.com/Wei-Shaw/sub2api/internal/pkg/openai"
)

// Only models with a documented turn-scoped top-level instructions contract
// use this policy. Do not infer it for an unknown future model family.
func useCodexStableInstructionPrefix(model string) bool {
	return codexStableInstructionModelID(model) != ""
}

func codexStableInstructionModelID(model string) string {
	canonical := openai.CanonicalizeOpenAIModelAliasSpelling(model)
	for _, base := range []string{"gpt-6.2-sol", "gpt-6.2-astra"} {
		if canonical == base {
			return base
		}
		suffix, ok := strings.CutPrefix(canonical, base+"-")
		if !ok {
			continue
		}
		for _, effort := range []string{"none", "minimal", "low", "medium", "high", "xhigh", "max", "openai-compact"} {
			if suffix == effort {
				return base
			}
			if date, ok := strings.CutSuffix(suffix, "-"+effort); ok {
				suffix = date
				break
			}
		}
		if _, err := time.Parse("2006-01-02", suffix); err == nil {
			return base
		}
	}
	return ""
}

func hasCodexInstructionContinuation(reqBody map[string]any) bool {
	if id, ok := reqBody["previous_response_id"].(string); ok && strings.TrimSpace(id) != "" {
		return true
	}
	conversation, present := reqBody["conversation"]
	if !present || conversation == nil {
		return false
	}
	if id, ok := conversation.(string); ok {
		return strings.TrimSpace(id) != ""
	}
	// An object names a persisted conversation. Invalid shapes should reach
	// upstream validation without an extra synthetic conversation item.
	return true
}

// prepareCodexStableInstructions preserves caller instructions as turn-scoped
// text, keeps system guidance in its original input position as developer, and
// adds our default only to an otherwise unguided initial request.
func prepareCodexStableInstructions(reqBody map[string]any, injectDefault bool) bool {
	if reqBody == nil {
		return false
	}
	modified, hasGuidance := false, false
	if input, ok := reqBody["input"].([]any); ok {
		for _, raw := range input {
			item, ok := raw.(map[string]any)
			if !ok {
				continue
			}
			switch item["role"] {
			case "system":
				hasGuidance = true
				item["role"] = "developer"
				modified = true
			case "developer":
				hasGuidance = true
			}
		}
	}
	if injectDefault && !hasGuidance && isInstructionsEmpty(reqBody) && !hasCodexInstructionContinuation(reqBody) {
		model, _ := reqBody["model"].(string)
		if canonical := codexStableInstructionModelID(model); canonical != "" {
			model = canonical
		}
		if prependCodexStableInstructions(reqBody, defaultCodexSynthInstructions(model)) {
			modified = true
		}
	}
	// The Codex endpoint still requires the field, even when guidance lives in
	// input. An explicitly supplied string (including whitespace) is untouched.
	if _, ok := reqBody["instructions"].(string); !ok {
		reqBody["instructions"] = ""
		modified = true
	}
	return modified
}

// prependCodexStableInstructions changes only input. Callers retain control of
// model gating, continuation policy, and any explicit top-level instructions.
func prependCodexStableInstructions(reqBody map[string]any, text string) bool {
	if reqBody == nil || strings.TrimSpace(text) == "" {
		return false
	}
	var input []any
	switch original := reqBody["input"].(type) {
	case []any:
		input = original
	case string:
		if strings.TrimSpace(original) != "" {
			input = []any{map[string]any{"type": "message", "role": "user", "content": original}}
		}
	default:
		// Preserve missing or malformed input for normal upstream validation.
		return false
	}
	if len(input) > 0 {
		if first, ok := input[0].(map[string]any); ok && first["role"] == "developer" {
			if existing, lossless := extractLosslessTextFromContent(first["content"]); lossless && existing == text {
				return false
			}
		}
	}
	prefix := map[string]any{
		"type": "message", "role": "developer",
		"content": []any{map[string]any{"type": "input_text", "text": text}},
	}
	reqBody["input"] = append([]any{prefix}, input...)
	return true
}
