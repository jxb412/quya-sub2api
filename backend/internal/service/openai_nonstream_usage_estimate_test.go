package service

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestEstimateOpenAINonStreamingRecoveredUsage_TextAndNoInventedUsage(t *testing.T) {
	usage, err := estimateOpenAINonStreamingRecoveredUsage(
		[]byte(`{"model":"gpt-6.2-sol","input":"hello world"}`),
		[]byte(`[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello world!"}]}]`),
		"gpt-6.2-sol",
	)
	require.NoError(t, err)
	require.Equal(t, OpenAIUsage{InputTokens: 2, OutputTokens: 3}, usage)
	require.Equal(t, 2, usage.billingUsageTokens().InputTokens)
	require.Zero(t, usage.billingUsageTokens().CacheReadTokens)
}

func TestEstimateOpenAINonStreamingRecoveredUsage_PreservesExistingInputEstimator(t *testing.T) {
	request := []byte(`{"model":"gpt-5","instructions":"You are helpful.","input":[{"role":"user","content":"lookup weather in shanghai"}],"tools":[{"type":"function","name":"lookup_weather","description":"Look up current weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}]}`)
	usage, err := estimateOpenAINonStreamingRecoveredUsage(request, []byte(`[]`), "")
	require.NoError(t, err)
	// This is the established count_tokens fixture, including instructions
	// and function schema. Recovery must not replace it with bytes/4.
	require.Equal(t, 50, usage.InputTokens)
	require.Zero(t, usage.OutputTokens)
}

func TestEstimateOpenAINonStreamingRecoveredUsage_OnlySemanticOutput(t *testing.T) {
	request := []byte(`{"model":"gpt-6.2-astra","input":"hello world"}`)
	output := []byte(`[
		{"type":"reasoning","summary":[{"type":"summary_text","text":"Visible summary."}],"encrypted_content":"opaque hidden reasoning must not be counted","content":[{"type":"reasoning_text","text":"unobservable internal content"}]},
		{"type":"message","id":"arbitrary metadata must not be counted","status":"incomplete","content":[{"type":"output_text","text":"正文已返回。","annotations":[{"text":"citation metadata"}]},{"type":"refusal","refusal":"I cannot do that."}]},
		{"type":"function_call","name":"lookup_weather","arguments":"{\"city\":\"Shanghai\"}","id":"fc_ignore","call_id":"call_ignore","encrypted_function_args":"secret opaque payload"},
		{"type":"custom_tool_call","name":"python","input":"print(1)","arguments":"unused alternate field"},
		{"type":"image_generation_call","result":"aGVsbG8gaW1hZ2U=","output_format":"png"},
		{"type":"unknown_vendor_item","text":"unknown metadata"}
	]`)
	usage, err := estimateOpenAINonStreamingRecoveredUsage(request, output, "gpt-6.2-astra")
	require.NoError(t, err)
	codec, err := openAIInputTokensCodecForModel("gpt-6.2-astra")
	require.NoError(t, err)
	expected := 0
	for _, text := range []string{"Visible summary.", "正文已返回。", "I cannot do that.", "lookup_weather", `{"city":"Shanghai"}`, "python", "print(1)"} {
		n, countErr := codec.Count(text)
		require.NoError(t, countErr)
		expected += n
	}
	require.Equal(t, OpenAIUsage{InputTokens: 2, OutputTokens: expected}, usage)
}

func TestEstimateOpenAINonStreamingRecoveredUsage_ImagePayloadSizeDoesNotAffectEstimate(t *testing.T) {
	makeRequest := func(payload string) []byte {
		body, err := json.Marshal(map[string]any{
			"model": "gpt-6.2-sol",
			"input": []any{map[string]any{"role": "user", "content": []any{
				map[string]any{"type": "input_text", "text": "describe"},
				map[string]any{"type": "input_image", "image_url": "data:image/png;base64," + payload},
			}}},
		})
		require.NoError(t, err)
		return body
	}
	makeOutput := func(payload string) []byte {
		body, err := json.Marshal([]any{
			map[string]any{"type": "message", "content": []any{map[string]any{"type": "output_text", "text": "Hello world!"}}},
			map[string]any{"type": "image_generation_call", "result": payload, "output_format": "png"},
		})
		require.NoError(t, err)
		return body
	}
	small, err := estimateOpenAINonStreamingRecoveredUsage(makeRequest("AAAA"), makeOutput("AAAA"), "gpt-6.2-sol")
	require.NoError(t, err)
	large, err := estimateOpenAINonStreamingRecoveredUsage(makeRequest(strings.Repeat("AAAA", 32768)), makeOutput(strings.Repeat("AAAA", 32768)), "gpt-6.2-sol")
	require.NoError(t, err)
	require.Equal(t, small, large)
	require.Equal(t, 3, large.OutputTokens)
	require.Zero(t, large.ImageInputTokens)
	require.Zero(t, large.ImageOutputTokens)
}

func TestEstimateOpenAINonStreamingRecoveredUsage_ModelCodecCompatibility(t *testing.T) {
	request := []byte(`{"model":"gpt-6.2-sol","input":"antidisestablishmentarianism"}`)
	output := []byte(`[{"type":"message","content":[{"type":"output_text","text":"antidisestablishmentarianism"}]}]`)
	for _, model := range []string{"gpt-4", "gpt-4.1", "gpt-6.2-astra", "unknown-relay-model"} {
		t.Run(model, func(t *testing.T) {
			usage, err := estimateOpenAINonStreamingRecoveredUsage(request, output, model)
			require.NoError(t, err)
			codec, err := openAIInputTokensCodecForModel(model)
			require.NoError(t, err)
			n, err := codec.Count("antidisestablishmentarianism")
			require.NoError(t, err)
			require.Equal(t, OpenAIUsage{InputTokens: n, OutputTokens: n}, usage)
		})
	}
}

func TestEstimateOpenAINonStreamingRecoveredUsage_RejectsInvalidShapes(t *testing.T) {
	for _, tc := range []struct{ name, request, output string }{
		{"invalid request", `{`, `[]`},
		{"null request", `null`, `[]`},
		{"nonobject request", `[]`, `[]`},
		{"invalid input type", `{"instructions":42}`, `[]`},
		{"invalid output", `{"input":"hello"}`, `[`},
		{"null output", `{"input":"hello"}`, `null`},
		{"object output", `{"input":"hello"}`, `{}`},
	} {
		t.Run(tc.name, func(t *testing.T) {
			usage, err := estimateOpenAINonStreamingRecoveredUsage([]byte(tc.request), []byte(tc.output), "gpt-6.2-sol")
			require.Error(t, err)
			require.Equal(t, OpenAIUsage{}, usage)
		})
	}
}
