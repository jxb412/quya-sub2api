package service

import (
	"context"
	"net/http"
	"strings"

	"github.com/Wei-Shaw/sub2api/internal/pkg/logger"
	"github.com/gin-gonic/gin"
	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
	"go.uber.org/zap"
)

// 2026-11-28: opt-in API-key transport adaptation. The original client mode
// remains synchronous for scheduling, response formatting and billing; only
// the outbound HTTP body requests SSE. No database migration is required.
func (a *Account) IsOpenAISyncUpstreamStreamEnabled() bool {
	return a != nil && a.IsOpenAIApiKey() && a.Extra["openai_sync_upstream_stream"] == true
}

func shouldForceOpenAISyncUpstreamStream(c *gin.Context, account *Account, body []byte) bool {
	if !account.IsOpenAISyncUpstreamStreamEnabled() || GetOpenAIClientTransport(c) != OpenAIClientTransportHTTP || isOpenAIResponsesCompactPath(c) || gjson.GetBytes(body, "background").Bool() {
		return false
	}
	if c != nil {
		if value, exists := c.Get(openAISyncUpstreamStreamEligibilityKey); exists {
			if eligible, ok := value.(bool); ok {
				return eligible
			}
		}
	}
	if c != nil && c.Request != nil {
		if policy, ok := openAIInboundRequestPolicyFromContext(c.Request.Context()); ok && policy.ClientRequestType != RequestTypeUnknown {
			return policy.ClientRequestType == RequestTypeSync
		}
	}
	return !gjson.GetBytes(body, "stream").Bool()
}

func prepareOpenAISyncUpstreamStream(c *gin.Context, account *Account, body []byte, chat bool) ([]byte, bool, error) {
	if !shouldForceOpenAISyncUpstreamStream(c, account, body) {
		return body, false, nil
	}
	patched, err := sjson.SetBytes(body, "stream", true)
	if err == nil && chat {
		patched, err = sjson.SetBytes(patched, "stream_options.include_usage", true)
	}
	if err != nil {
		return nil, false, err
	}
	return patched, true, nil
}

type openAISyncUpstreamStreamContextKey struct{}

const openAISyncUpstreamStreamEligibilityKey = "quya_openai_sync_upstream_stream_eligible"

// Fixed transport metadata only: no prompt, body, headers, credentials or URL.
// This distinguishes the upstream protocol from the client-side "sync" label.
func logOpenAISyncUpstreamStreamResponse(ctx context.Context, account *Account, response *http.Response, err error) {
	if enabled, _ := ctx.Value(openAISyncUpstreamStreamContextKey{}).(bool); !enabled {
		return
	}
	status, sse := 0, false
	if response != nil {
		status = response.StatusCode
		sse = strings.Contains(strings.ToLower(response.Header.Get("Content-Type")), "text/event-stream")
	}
	logger.FromContext(ctx).Info("openai.sync_upstream_stream",
		zap.Int64("account_id", account.ID), zap.Bool("client_stream", false),
		zap.Bool("upstream_stream", true), zap.Bool("upstream_sse", sse),
		zap.Int("upstream_status", status), zap.Bool("transport_error", err != nil))
}

// A well-formed final event can still omit usage. Opted-in SSE aggregation
// must not silently make that answer free. Explicit numeric zero is respected;
// estimates never invent cache hits or hidden reasoning and stay in admin logs.
func ensureOpenAISyncUpstreamUsage(c *gin.Context, account *Account, response []byte, sseBody, model string, usage *OpenAIUsage, requestBody ...[]byte) ([]byte, error) {
	var input []byte
	if len(requestBody) > 0 {
		input = requestBody[0]
	}
	// Only non-streaming response handlers call this helper. Their input may
	// already be a stream=true compatibility body, so do not reclassify it.
	if !account.IsOpenAISyncUpstreamStreamEnabled() || GetOpenAIClientTransport(c) != OpenAIClientTransportHTTP || isOpenAIResponsesCompactPath(c) || gjson.GetBytes(input, "background").Bool() {
		return response, nil
	}
	if value, exists := c.Get(openAISyncUpstreamStreamEligibilityKey); exists && value == false {
		return response, nil
	}
	if hasOpenAISSEUsage(sseBody) {
		if hasOpenAINumericUsage(response) {
			return response, nil
		}
		// Earlier numeric usage remains authoritative when the final event has
		// no usage. Return the same token counts that enter the billing pipeline.
		return sjson.SetBytes(response, "usage", gin.H{
			"input_tokens": usage.InputTokens, "output_tokens": usage.OutputTokens,
			"total_tokens": usage.InputTokens + usage.OutputTokens,
			"input_tokens_details": gin.H{
				"cached_tokens": usage.CacheReadInputTokens, "image_tokens": usage.ImageInputTokens,
				"cache_write_tokens":         usage.CacheCreationInputTokens,
				"cache_write_tokens_details": gin.H{"ttl_30m_tokens": usage.CacheCreation30mInputTokens, "ttl_12h_tokens": usage.CacheCreation12hInputTokens},
			},
			"output_tokens_details": gin.H{"image_tokens": usage.ImageOutputTokens},
		})
	}
	estimated, err := estimateOpenAINonStreamingRecoveredUsage(input, []byte(gjson.GetBytes(response, "output").Raw), model)
	if err != nil {
		return nil, err
	}
	patched, err := sjson.SetBytes(response, "usage", gin.H{
		"input_tokens": estimated.InputTokens, "output_tokens": estimated.OutputTokens,
		"total_tokens":         estimated.InputTokens + estimated.OutputTokens,
		"input_tokens_details": gin.H{"cached_tokens": 0},
	})
	if err != nil {
		return nil, err
	}
	*usage = estimated
	logger.FromContext(c.Request.Context()).Info("openai.sync_upstream_usage",
		zap.Int64("account_id", account.ID), zap.String("usage_source", "local_estimate"),
		zap.Int("input_tokens", usage.InputTokens), zap.Int("output_tokens", usage.OutputTokens))
	return patched, nil
}
