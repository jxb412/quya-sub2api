package handler

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

// 2026-11-28: compatibility layers may request upstream streaming for an HTTP
// sync client. The original client mode installed by the handler must win.
func TestBindOpenAIInboundPolicySyncSurvivesUpstreamStreamRewrite(t *testing.T) {
	for _, protocol := range []service.OpenAIInboundProtocol{
		service.OpenAIInboundProtocolResponses,
		service.OpenAIInboundProtocolChatCompletions,
		service.OpenAIInboundProtocolMessages,
	} {
		t.Run(string(protocol), func(t *testing.T) {
			gin.SetMode(gin.TestMode)
			c, _ := gin.CreateTestContext(httptest.NewRecorder())
			c.Request = httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-6.2-sol"}`))
			gateway := &service.OpenAIGatewayService{}
			h := &OpenAIGatewayHandler{gatewayService: gateway}
			h.bindOpenAIInboundRequestPolicy(c, protocol, []byte(`{"model":"gpt-6.2-sol"}`), service.RequestTypeSync)
			account := &service.Account{
				ID: 10, Platform: service.PlatformOpenAI, Type: service.AccountTypeAPIKey,
				Extra: map[string]any{"openai_disable_sync": true},
			}
			result, err := gateway.Forward(c.Request.Context(), c, account, []byte(`{"model":"gpt-6.2-sol","stream":true}`))
			var failover *service.UpstreamFailoverError
			require.ErrorAs(t, err, &failover)
			require.Nil(t, result)
			require.Equal(t, service.OpenAIInboundSyncRestrictionReason, failover.Reason)
			require.True(t, failover.RequestScopedTransient)
			require.Equal(t, service.NextAccountRetry, failover.NextAccountAction)
			require.False(t, c.Writer.Written(), "the scheduler may still choose another account")
		})
	}
}

func TestOpenAIDisableSyncFailoverUsesLocalPolicyError(t *testing.T) {
	for _, anthropic := range []bool{false, true} {
		name := "responses"
		if anthropic {
			name = "messages"
		}
		t.Run(name, func(t *testing.T) {
			gin.SetMode(gin.TestMode)
			recorder := httptest.NewRecorder()
			c, _ := gin.CreateTestContext(recorder)
			c.Request = httptest.NewRequest(http.MethodPost, "/v1/responses", nil)
			failure := &service.UpstreamFailoverError{
				StatusCode: http.StatusForbidden, Reason: service.OpenAIInboundSyncRestrictionReason,
				ClientStatusCode: http.StatusForbidden,
				ClientMessage:    "No account in this group accepts synchronous client requests",
			}
			h := &OpenAIGatewayHandler{}
			if anthropic {
				h.handleAnthropicFailoverExhausted(c, failure, false)
			} else {
				h.handleFailoverExhausted(c, failure, false)
			}
			require.Equal(t, http.StatusForbidden, recorder.Code)
			require.Equal(t, failure.ClientMessage, gjson.Get(recorder.Body.String(), "error.message").String())
			require.NotContains(t, recorder.Body.String(), "upstream_error")
		})
	}
}
