package service

import (
	"encoding/json"
	"net/http"

	"github.com/gin-gonic/gin"
	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

const openAINonStreamingIncompleteCode = "upstream_incomplete_response"

func hasOpenAINumericUsage(data []byte) bool {
	for _, path := range []string{"usage", "response.usage", "data.usage", "data.response.usage"} {
		value := gjson.GetBytes(data, path)
		for _, field := range []string{"input_tokens", "output_tokens", "prompt_tokens", "completion_tokens"} {
			if value.Get(field).Type == gjson.Number {
				return true
			}
		}
	}
	return false
}

func hasOpenAISSEUsage(body string) bool {
	observed := false
	forEachOpenAISSEFrame(body, func(_ string, data []byte) {
		if observed || !gjson.ValidBytes(data) {
			return
		}
		// An empty usage object is not a report of zero tokens. Explicit
		// numeric zero remains authoritative and must never trigger estimation.
		observed = hasOpenAINumericUsage(data)
	})
	return observed
}

// 2026-11-28: preserve usable output when the upstream closes before its final
// response. Report an incomplete response, and feed actual or locally estimated
// tokens into the existing pricing/balance/subscription pipeline exactly once.
// Estimation provenance stays in administrator diagnostics, not client fields.
func (s *OpenAIGatewayService) recoverOpenAINonStreamingResponse(resp *http.Response, c *gin.Context, account *Account, passthrough bool, body, terminalType, model string, usage *OpenAIUsage, requestBody ...[]byte) ([]byte, string, bool) {
	response, upstreamResponseID, ok := recoverOpenAINonStreamingOutput(body, model)
	if !ok {
		return nil, "", false
	}
	observed := hasOpenAISSEUsage(body)
	if !observed {
		var input []byte
		if len(requestBody) > 0 {
			input = requestBody[0]
		}
		estimated, err := estimateOpenAINonStreamingRecoveredUsage(input, []byte(gjson.GetBytes(response, "output").Raw), model)
		if err != nil {
			return nil, "", false
		}
		*usage = estimated
	}
	usagePayload := gin.H{
		"input_tokens": usage.InputTokens, "output_tokens": usage.OutputTokens,
		"total_tokens": usage.InputTokens + usage.OutputTokens,
		"input_tokens_details": gin.H{
			"cached_tokens":      usage.CacheReadInputTokens,
			"image_tokens":       usage.ImageInputTokens,
			"cache_write_tokens": usage.CacheCreationInputTokens,
			"cache_write_tokens_details": gin.H{
				"ttl_30m_tokens": usage.CacheCreation30mInputTokens,
				"ttl_12h_tokens": usage.CacheCreation12hInputTokens,
			},
		},
		"output_tokens_details": gin.H{"image_tokens": usage.ImageOutputTokens},
	}
	response, err := sjson.SetBytes(response, "usage", usagePayload)
	if err != nil {
		return nil, "", false
	}
	recordOpenAINonStreamingIncomplete(resp, c, account, passthrough, body, terminalType, usage, true)
	// HTTP 200 now carries usable but incomplete content. Keep the anomaly in
	// Ops without adding a client-facing estimation notice or triggering replay.
	MarkOpsStreamErrorValue(c, OpsStreamError{
		ErrType: "upstream_error", Code: openAINonStreamingIncompleteCode,
		Message:        "Upstream stream ended before its final response; available output recovered",
		IntendedStatus: http.StatusBadGateway, NonStream: !openAICompactClientWantsStream(c),
	})
	return response, upstreamResponseID, true
}

func recordOpenAINonStreamingIncomplete(resp *http.Response, c *gin.Context, account *Account, passthrough bool, body, terminalType string, usage *OpenAIUsage, recovered bool) {
	message := "Upstream stream ended without a valid final response"
	detail := struct {
		Reason          string       `json:"reason"`
		TerminalType    string       `json:"terminal_type,omitempty"`
		OutputRecovered bool         `json:"output_recovered"`
		UsageObserved   bool         `json:"usage_observed"`
		UsageSource     string       `json:"usage_source"`
		ObservedUsage   *OpenAIUsage `json:"observed_usage,omitempty"`
		BilledUsage     *OpenAIUsage `json:"billed_usage,omitempty"`
	}{Reason: "missing_terminal_response", TerminalType: terminalType, OutputRecovered: recovered, UsageSource: "unavailable"}
	if terminalType != "" {
		detail.Reason = "invalid_terminal_response"
	}
	detail.UsageObserved = hasOpenAISSEUsage(body)
	if detail.UsageObserved {
		detail.ObservedUsage = usage
		detail.UsageSource = "upstream"
	} else if recovered {
		detail.UsageSource = "local_estimate"
	}
	if recovered {
		detail.BilledUsage = usage
	}
	// Persist only numeric usage and fixed metadata, never prompt/output text.
	detailJSON, _ := json.Marshal(detail)
	setOpsUpstreamError(c, resp.StatusCode, message, string(detailJSON))
	event := OpsUpstreamErrorEvent{
		Passthrough: passthrough, UpstreamStatusCode: resp.StatusCode,
		UpstreamRequestID: resp.Header.Get("x-request-id"),
		Kind:              "protocol_error", Reason: openAINonStreamingIncompleteCode,
		Message: message, Detail: string(detailJSON),
	}
	if account != nil {
		event.Platform, event.AccountID, event.AccountName = account.Platform, account.ID, account.Name
	}
	event.ProxyID, event.ProxyName = opsUpstreamProxyAttribution(account)
	// The last attempt wins in Ops; do not let an earlier retry hide recovery.
	appendOpsUpstreamError(c, event)
}

// Keep compact's already-committed SSE protocol intact without fabricating a
// response.completed or marking partially received tool calls as done.
func writeOpenAIRecoveredCompactSSE(c *gin.Context, response []byte) bool {
	if !openAICompactClientWantsStream(c) {
		return false
	}
	committed := StopOpenAICompactSSEKeepaliveCommitted(c)
	if !committed {
		c.Header("Content-Type", "text/event-stream")
		c.Header("Cache-Control", "no-cache")
		c.Header("Connection", "keep-alive")
		c.Header("X-Accel-Buffering", "no")
		c.Status(http.StatusOK)
	}
	terminal, _ := json.Marshal(gin.H{
		"type": "response.incomplete", "sequence_number": 0,
		"response": json.RawMessage(response),
	})
	_, _ = c.Writer.Write([]byte("event: response.incomplete\ndata: "))
	_, _ = c.Writer.Write(terminal)
	_, _ = c.Writer.Write([]byte("\n\n"))
	c.Writer.Flush()
	return true
}
