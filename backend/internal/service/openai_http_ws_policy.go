package service

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"sync/atomic"

	"github.com/Wei-Shaw/sub2api/internal/pkg/logger"
	"github.com/gin-gonic/gin"
	"github.com/google/uuid"
)

type openAIHTTPWSAttemptContextKey struct{}

// One generation attempt, shared by nested protocol conversions and retries.
// sent is latched BEFORE response.create is written: an ambiguous write must
// never authorize replay through HTTP or another account.
type openAIHTTPWSAttempt struct {
	scope         string
	groupID       int64
	sent          atomic.Bool
	used          atomic.Bool
	fallback      atomic.Bool
	reused        atomic.Bool
	responseID    atomic.Value
	terminalEvent atomic.Value
	// 2026-11-19：记录完整历史恢复结果，确保重试/HTTP 兜底不再发送原增量体。
	recovery          atomic.Pointer[openAIHTTPWSRecoveredRequest]
	continuationBound atomic.Bool
	// 2026-11-19：HTTP 兜底成功的完整终态也用于下一轮恢复，失败尝试不得入库。
	httpReplay atomic.Pointer[openAIHTTPWSHTTPReplay]
}

func openAIHTTPWSAttemptFromContext(ctx context.Context) *openAIHTTPWSAttempt {
	if ctx == nil {
		return nil
	}
	attempt, _ := ctx.Value(openAIHTTPWSAttemptContextKey{}).(*openAIHTTPWSAttempt)
	return attempt
}

func explicitOpenAIHTTPWSCtxPool(account *Account) bool {
	if account == nil || !account.IsOpenAI() {
		return false
	}
	key := ""
	if account.IsOpenAIOAuthLike() {
		key = "openai_oauth_responses_websockets_v2_mode"
	} else if account.IsOpenAIApiKey() {
		key = "openai_apikey_responses_websockets_v2_mode"
	}
	mode, _ := account.Extra[key].(string)
	return strings.TrimSpace(strings.ToLower(mode)) == OpenAIWSIngressModeCtxPool
}

func (s *OpenAIGatewayService) withOpenAIHTTPWSPolicy(ctx context.Context, c *gin.Context, account *Account, body []byte, inbound, defaultModel string, forward func(context.Context) (*OpenAIForwardResult, error)) (*OpenAIForwardResult, error) {
	if openAIHTTPWSAttemptFromContext(ctx) != nil || GetOpenAIClientTransport(c) != OpenAIClientTransportHTTP || !explicitOpenAIHTTPWSCtxPool(account) || isOpenAIResponsesCompactPath(c) {
		return forward(ctx)
	}
	// Reject contradictory settings before any transport is invoked. A plugin
	// that owns HTTP transport cannot be silently bypassed by a native WS pool.
	reason := ""
	decision := s.getOpenAIWSProtocolResolver().Resolve(account)
	switch {
	case s.cfg == nil || !s.cfg.Gateway.OpenAIWS.ModeRouterV2Enabled:
		reason = "mode_router_v2_disabled"
	case decision.Transport != OpenAIUpstreamTransportResponsesWebsocketV2:
		reason = decision.Reason
	case s.pluginManager != nil && s.pluginManager.ShouldRouteOpenAIOAuth(account):
		reason = "http_transport_plugin_bound"
	case s.resolveUpstreamProtocolFor(ctx, account, inbound, upstreamRoutingModel(account, body, defaultModel)) != APIProtocolResponses:
		reason = "responses_protocol_required"
	}
	if reason != "" {
		message := "ctx_pool requires an enabled Responses WebSocket v2 upstream: " + reason
		if c != nil && !c.Writer.Written() {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": gin.H{"type": "server_error", "code": "ctx_pool_configuration_error", "message": message}})
		}
		return nil, errors.New(message)
	}
	apiKeyID := getAPIKeyIDFromContext(c)
	groupID := getOpenAIGroupIDFromContext(c)
	var userID int64
	if raw, ok := c.Get("api_key"); ok {
		if key, ok := raw.(*APIKey); ok && key != nil {
			userID = key.UserID
		}
	}
	session, _ := resolveOpenAIWSExecutionScope(c, body, apiKeyID)
	if apiKeyID <= 0 {
		// Internal callers without an authenticated key never share connections.
		session = uuid.NewString()
	}
	digest := sha256.Sum256([]byte(fmt.Sprintf("http-ws:%d:%d:%d:%s", groupID, userID, apiKeyID, session)))
	attempt := &openAIHTTPWSAttempt{scope: hex.EncodeToString(digest[:]), groupID: groupID}
	ctx = context.WithValue(ctx, openAIHTTPWSAttemptContextKey{}, attempt)
	result, err := forward(ctx)
	s.completeOpenAIHTTPWSHTTPHistory(ctx, attempt, result, err)
	var continuationErr *openAIHTTPWSContinuationError
	if errors.As(err, &continuationErr) {
		if c != nil && !c.Writer.Written() {
			if inbound == APIProtocolAnthropic {
				writeAnthropicError(c, http.StatusBadRequest, "invalid_request_error", continuationErr.Error())
			} else {
				c.JSON(http.StatusBadRequest, gin.H{"error": gin.H{
					"type": "invalid_request_error", "code": "ctx_pool_continuation_unavailable",
					"param": "previous_response_id", "message": continuationErr.Error(),
				}})
			}
		}
		return nil, continuationErr
	}
	if result != nil && attempt.sent.Load() {
		result.OpenAIWSMode = true
		if id, _ := attempt.responseID.Load().(string); id != "" {
			result.RequestID = id
			result.ResponseID = id
		}
		if terminal, _ := attempt.terminalEvent.Load().(string); terminal != "" {
			result.UpstreamTerminalEvent = terminal
		}
	}
	if err != nil && attempt.used.Load() && !attempt.fallback.Load() {
		var failover *UpstreamFailoverError
		if errors.As(err, &failover) {
			// Keep the upstream status/error envelope for normal handler mapping,
			// but prevent replay after dispatch or an adapter validation failure.
			stopped := *failover
			stopped.NextAccountAction = NextAccountStop
			stopped.RetryableOnSameAccount = false
			stopped.SafeToFailoverAfterWrite = false
			err = &stopped
		}
	}
	return result, err
}

func (s *OpenAIGatewayService) Forward(ctx context.Context, c *gin.Context, account *Account, body []byte) (*OpenAIForwardResult, error) {
	return s.withOpenAIHTTPWSPolicy(ctx, c, account, body, APIProtocolResponses, "", func(ctx context.Context) (*OpenAIForwardResult, error) {
		return s.forwardOpenAIPrepared(ctx, c, account, body)
	})
}

func (s *OpenAIGatewayService) forwardAsChatCompletions(ctx context.Context, c *gin.Context, account *Account, body []byte, promptCacheKey, defaultMappedModel string, tenantIsolated bool) (*OpenAIForwardResult, error) {
	return s.withOpenAIHTTPWSPolicy(ctx, c, account, body, APIProtocolChatCompletions, defaultMappedModel, func(ctx context.Context) (*OpenAIForwardResult, error) {
		return s.forwardAsChatCompletionsPrepared(ctx, c, account, body, promptCacheKey, defaultMappedModel, tenantIsolated)
	})
}

func (s *OpenAIGatewayService) ForwardAsAnthropic(ctx context.Context, c *gin.Context, account *Account, body []byte, promptCacheKey, defaultMappedModel string) (*OpenAIForwardResult, error) {
	return s.withOpenAIHTTPWSPolicy(ctx, c, account, body, APIProtocolAnthropic, defaultMappedModel, func(ctx context.Context) (*OpenAIForwardResult, error) {
		return s.forwardAsAnthropicPrepared(ctx, c, account, body, promptCacheKey, defaultMappedModel)
	})
}

// connectOpenAIHTTPWSPool tries the initial connection plus three retries.
// Each acquire has a five-second deadline; generation reads use their normal
// timeout. HTTP fallback is allowed only before any response.create write.
func (s *OpenAIGatewayService) connectOpenAIHTTPWSPool(req *http.Request, proxyURL string, account *Account, attempt *openAIHTTPWSAttempt) (*http.Response, bool, error) {
	if attempt.sent.Load() {
		return nil, true, errors.New("ctx_pool generation already dispatched; refusing to replay")
	}
	if attempt.fallback.Load() {
		return nil, false, nil
	}
	for i := 0; i <= 3; i++ {
		resp, err := s.doOpenAIHTTPWSPool(req, proxyURL, account)
		if attempt.sent.Load() {
			// Quya 2026-11-19：区分实际派发时的新建/复用，辅助定位混合 WS/HTTP 日志。
			logger.LegacyPrintf("service.openai_gateway", "[OpenAI ctx_pool] upstream=websocket account=%d connection_attempt=%d reused=%t", account.ID, i+1, attempt.reused.Load())
			return resp, true, err
		}
		var capacityErr *openAIHTTPWSCodexCapacityError
		if errors.As(err, &capacityErr) {
			if err := req.Context().Err(); err != nil {
				return nil, true, err
			}
			if err := s.prepareOpenAIHTTPWSFallback(req, proxyURL, account, attempt); err != nil {
				return nil, true, err
			}
			attempt.fallback.Store(true)
			logger.LegacyPrintf("service.openai_gateway", "[OpenAI ctx_pool] upstream=http fallback=codex_ws_payload_capacity account=%d wire_bytes=%d limit_bytes=%d", account.ID, capacityErr.wireBytes, openAIHTTPWSCodexRequestLimitBytes)
			return nil, false, nil
		}
		var connectErr *openAIHTTPWSConnectError
		if err != nil && !errors.As(err, &connectErr) {
			return resp, true, err
		}
		status := 0
		if resp != nil {
			status = resp.StatusCode
			if resp.Body != nil {
				_ = resp.Body.Close()
			}
		}
		if req.Context().Err() != nil {
			return nil, true, req.Context().Err()
		}
		logger.LegacyPrintf("service.openai_gateway", "[OpenAI ctx_pool] ws_connection_failed account=%d connection_attempt=%d status=%d", account.ID, i+1, status)
	}
	if err := s.prepareOpenAIHTTPWSFallback(req, proxyURL, account, attempt); err != nil {
		return nil, true, err
	}
	attempt.fallback.Store(true)
	logger.LegacyPrintf("service.openai_gateway", "[OpenAI ctx_pool] upstream=http fallback=ws_connection_exhausted account=%d retries=3 timeout_seconds=5", account.ID)
	return nil, false, nil
}
