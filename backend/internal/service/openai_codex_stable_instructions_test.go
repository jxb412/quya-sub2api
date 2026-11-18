package service

import (
	"encoding/json"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestCodexStableInstructionPrefixModelBoundary(t *testing.T) {
	for _, model := range []string{
		"gpt-6.2-sol", "gpt-6.2-astra", "openai/GPT-6.2-SOL", "gpt-6.2-sol-high",
		"gpt-6.2-astra-2026-11-19", "gpt-6.2-sol-2026-11-19-xhigh", "gpt-6.2-astra-openai-compact",
	} {
		require.True(t, useCodexStableInstructionPrefix(model), model)
	}
	for _, model := range []string{
		"gpt-6.20-sol", "gpt-6.2", "gpt-6.3-sol", "gpt-6.1-sol", "gpt-6.2-sol-preview",
		"gpt-6.2-astra-2026-99-19", "gpt-6.2-sol-highest", "gpt-6.2-sol-2026-11-19-preview",
	} {
		require.False(t, useCodexStableInstructionPrefix(model), model)
	}
}

func TestCodexStableInstructionsKeepClientGuidanceAndOrdering(t *testing.T) {
	for _, omitPromoted := range []bool{false, true} {
		body := map[string]any{
			"model": "gpt-6.2-sol", "instructions": "  Only for this turn.\n",
			"input": []any{
				map[string]any{"role": "system", "content": "Project rules."},
				map[string]any{"role": "user", "content": "First question"},
				map[string]any{"role": "system", "content": []any{map[string]any{"type": "input_text", "text": "Later notice"}, map[string]any{"type": "input_image", "image_url": "https://example.invalid/image.png"}}},
				map[string]any{"role": "developer", "content": "Existing developer note"},
			},
		}
		result := applyCodexOAuthTransformWithOptions(body, codexOAuthTransformOptions{OmitPromotedSystemMessagesFromInput: omitPromoted})
		require.NoError(t, result.Error)
		require.Equal(t, "  Only for this turn.\n", body["instructions"])
		input := codexStableTestInput(t, body)
		require.Len(t, input, 4)
		require.Equal(t, "developer", codexStableTestMessage(t, input[0])["role"])
		require.Equal(t, "Project rules.", codexStableTestMessage(t, input[0])["content"])
		require.Equal(t, "user", codexStableTestMessage(t, input[1])["role"])
		require.Equal(t, "developer", codexStableTestMessage(t, input[2])["role"])
		require.Len(t, codexStableTestMessage(t, input[2])["content"], 2)
		require.Equal(t, "Existing developer note", codexStableTestMessage(t, input[3])["content"])
		before, err := json.Marshal(body)
		require.NoError(t, err)
		applyCodexOAuthTransformWithOptions(body, codexOAuthTransformOptions{OmitPromotedSystemMessagesFromInput: omitPromoted})
		after, err := json.Marshal(body)
		require.NoError(t, err)
		require.JSONEq(t, string(before), string(after))
	}
}

func TestCodexStableInstructionsDefaultIsCachedPrefixOnce(t *testing.T) {
	for _, model := range []string{"gpt-6.2-sol", "gpt-6.2-astra-2026-11-19"} {
		body := map[string]any{"model": model, "input": "  hello\n"}
		result := applyCodexOAuthTransform(body, false, false)
		require.NoError(t, result.Error)
		require.Equal(t, "", body["instructions"])
		input := codexStableTestInput(t, body)
		require.Len(t, input, 2)
		first := codexStableTestMessage(t, input[0])
		require.Equal(t, "developer", first["role"])
		text, lossless := extractLosslessTextFromContent(first["content"])
		require.True(t, lossless)
		require.Equal(t, defaultCodexSynthInstructions(codexStableInstructionModelID(model)), text)
		require.Equal(t, "  hello\n", codexStableTestMessage(t, input[1])["content"])
		applyCodexOAuthTransform(body, false, false)
		require.Len(t, body["input"], 2)
	}
}

func TestCodexStableInstructionsDoNotInjectOnContinuation(t *testing.T) {
	for _, continuation := range []map[string]any{
		{"previous_response_id": "resp_previous"},
		{"conversation": "conv_existing"},
		{"conversation": map[string]any{"id": "conv_existing"}},
	} {
		body := map[string]any{"model": "gpt-6.2-sol", "input": []any{map[string]any{"role": "user", "content": "next"}}}
		for key, value := range continuation {
			body[key] = value
		}
		applyCodexOAuthTransform(body, false, false)
		require.Equal(t, "", body["instructions"])
		require.Len(t, body["input"], 1)
		require.Equal(t, "user", codexStableTestMessage(t, codexStableTestInput(t, body)[0])["role"])
		for key, value := range continuation {
			require.Equal(t, value, body[key])
		}
	}
}

func TestCodexStableInstructionsExistingGuidanceAndSkipDefault(t *testing.T) {
	for _, role := range []string{"system", "developer"} {
		body := map[string]any{"model": "gpt-6.2-sol", "input": []any{map[string]any{"role": role, "content": "Project prompt"}}}
		applyCodexOAuthTransform(body, false, false)
		require.Equal(t, "", body["instructions"])
		require.Len(t, body["input"], 1)
		require.Equal(t, "Project prompt", codexStableTestMessage(t, codexStableTestInput(t, body)[0])["content"])
	}
	body := map[string]any{"model": "gpt-6.2-sol", "input": "hi"}
	applyCodexOAuthTransformWithOptions(body, codexOAuthTransformOptions{SkipDefaultInstructions: true})
	require.Equal(t, "", body["instructions"])
	require.Len(t, body["input"], 1)

	body = map[string]any{"model": "gpt-6.2-sol", "instructions": "This turn only", "input": "hi"}
	applyCodexOAuthTransform(body, false, false)
	require.Equal(t, "This turn only", body["instructions"])
	require.Len(t, body["input"], 1)
}

func TestCodexStableInstructionsLegacyAndCompactRetainWireShape(t *testing.T) {
	for _, tc := range []struct {
		model   string
		compact bool
	}{{"gpt-6.1-sol", false}, {"gpt-6.20-sol", false}, {"gpt-6.2-sol", true}} {
		body := map[string]any{"model": tc.model, "input": []any{map[string]any{"role": "system", "content": "Project instructions"}, map[string]any{"role": "user", "content": "hi"}}}
		applyCodexOAuthTransformWithOptions(body, codexOAuthTransformOptions{IsCompact: tc.compact, OmitPromotedSystemMessagesFromInput: true})
		require.Equal(t, "Project instructions", body["instructions"])
		require.Len(t, body["input"], 1)
	}
}

func TestPrependCodexStableInstructionsIsLosslessAndIdempotent(t *testing.T) {
	body := map[string]any{"input": []any{map[string]any{"role": "developer", "content": []any{map[string]any{"type": "input_text", "text": "project"}, map[string]any{"type": "input_image", "image_url": "https://example.invalid/image"}}}}, "instructions": "turn"}
	require.True(t, prependCodexStableInstructions(body, "project"))
	require.Len(t, body["input"], 2, "mixed content must not be mistaken for an existing text prefix")
	require.False(t, prependCodexStableInstructions(body, "project"))
	require.Equal(t, "turn", body["instructions"])
	require.Len(t, body["input"], 2)
}

func codexStableTestInput(t *testing.T, body map[string]any) []any {
	t.Helper()
	input, ok := body["input"].([]any)
	require.True(t, ok)
	return input
}

func codexStableTestMessage(t *testing.T, item any) map[string]any {
	t.Helper()
	message, ok := item.(map[string]any)
	require.True(t, ok)
	return message
}
