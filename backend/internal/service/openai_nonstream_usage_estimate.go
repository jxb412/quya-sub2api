package service

import (
	"encoding/json"
	"fmt"
	"strings"

	"github.com/tidwall/gjson"
)

// 2026-11-28: when a non-streaming response is recovered from an interrupted
// upstream stream without usage, estimate the received content for normal local
// model-price billing. This uses the existing input-token codec and never
// invents cache hits, hidden reasoning, or image tokens. Callers record the
// estimation source in administrator diagnostics, not in the client payload.
func estimateOpenAINonStreamingRecoveredUsage(requestBody, output []byte, model string) (OpenAIUsage, error) {
	if !gjson.ValidBytes(requestBody) || !gjson.ParseBytes(requestBody).IsObject() {
		return OpenAIUsage{}, fmt.Errorf("estimate recovered response usage: request must be a JSON object")
	}
	if !gjson.ValidBytes(output) || !gjson.ParseBytes(output).IsArray() {
		return OpenAIUsage{}, fmt.Errorf("estimate recovered response usage: output must be a JSON array")
	}

	var request openAIInputTokensCountRequest
	if err := json.Unmarshal(requestBody, &request); err != nil {
		return OpenAIUsage{}, fmt.Errorf("estimate recovered response input: %w", err)
	}
	if normalized := strings.TrimSpace(model); normalized != "" {
		request.Model = normalized
	}
	inputTokens, err := estimateOpenAIInputTokens(request)
	if err != nil {
		return OpenAIUsage{}, fmt.Errorf("estimate recovered response input: %w", err)
	}
	codec, err := openAIInputTokensCodecForModel(request.Model)
	if err != nil {
		return OpenAIUsage{}, fmt.Errorf("estimate recovered response output: %w", err)
	}

	outputTokens := 0
	countText := func(value gjson.Result) error {
		// Only semantic string fields count. IDs, statuses, JSON framing, opaque
		// encrypted fields, and image payloads must not become output tokens.
		if value.Type != gjson.String || value.Str == "" {
			return nil
		}
		n, err := codec.Count(value.Str)
		if err != nil {
			return err
		}
		outputTokens += n
		return nil
	}
	countContent := func(content gjson.Result) error {
		if content.Type == gjson.String {
			return countText(content)
		}
		for _, part := range content.Array() {
			switch part.Get("type").String() {
			case "output_text", "text":
				if err := countText(part.Get("text")); err != nil {
					return err
				}
			case "refusal":
				if err := countText(part.Get("refusal")); err != nil {
					return err
				}
			}
		}
		return nil
	}
	for _, item := range gjson.ParseBytes(output).Array() {
		var countErr error
		switch item.Get("type").String() {
		case "message":
			countErr = countContent(item.Get("content"))
		case "reasoning":
			// A visible reasoning summary is available for estimation; the
			// upstream's hidden reasoning and encrypted_content are not.
			for _, summary := range item.Get("summary").Array() {
				if summary.Get("type").String() == "summary_text" {
					if countErr = countText(summary.Get("text")); countErr != nil {
						break
					}
				}
			}
		case "function_call", "custom_tool_call":
			countErr = countText(item.Get("name"))
			if countErr == nil {
				field := "arguments"
				if item.Get("type").String() == "custom_tool_call" {
					field = "input"
				}
				countErr = countText(item.Get(field))
			}
		}
		if countErr != nil {
			return OpenAIUsage{}, fmt.Errorf("estimate recovered response output: %w", countErr)
		}
	}

	return OpenAIUsage{InputTokens: inputTokens, OutputTokens: outputTokens}, nil
}
