package service

import (
	"context"
	"fmt"
	"io"
	"strings"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestOpenAIHTTPWSHandshakeDropsLegacyResponsesBetaOnly(t *testing.T) {
	for _, existing := range []string{
		"responses=experimental, unrelated-feature=2026-11-19",
		"unrelated-feature=2026-11-19, responses=experimental, " + openAIWSBetaV2Value,
	} {
		t.Run(existing, func(t *testing.T) {
			dialer := &httpWSTestDialer{}
			svc, account := newHTTPWSTestService(t, dialer)
			req, _ := newHTTPWSTestRequest(t, "beta-test", `{"model":"gpt-6.1-sol","input":"hello"}`)
			req.Header.Set("OpenAI-Beta", existing)
			req.Header.Add("OpenAI-Beta", "another-feature=2026-11-20")
			resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
			require.NoError(t, err)
			defer func() { _ = resp.Body.Close() }()
			_, err = io.ReadAll(resp.Body)
			require.NoError(t, err)
			dialer.mu.Lock()
			headers := cloneHeader(dialer.headers[0])
			dialer.mu.Unlock()
			beta := strings.Join(headers.Values("OpenAI-Beta"), ", ")
			require.NotContains(t, beta, "responses=experimental")
			require.Contains(t, beta, "unrelated-feature=2026-11-19")
			require.Contains(t, beta, "another-feature=2026-11-20")
			require.Equal(t, 1, strings.Count(beta, openAIWSBetaV2Value))
			require.Equal(t, existing, req.Header.Get("OpenAI-Beta"), "HTTP fallback headers must remain unchanged")
			require.Equal(t, []string{existing, "another-feature=2026-11-20"}, req.Header.Values("OpenAI-Beta"))
		})
	}
}

func TestHTTPWSPolicyOAuthMessagesUsesOnlyWebSocketResponsesBeta(t *testing.T) {
	for _, stream := range []bool{false, true} {
		t.Run(fmt.Sprintf("stream_%t", stream), func(t *testing.T) {
			svc, account, upstream := newHTTPWSPolicyTestService(t, "https://unused.example")
			account.Type = AccountTypeOAuth
			account.Credentials = map[string]any{"access_token": "oauth-test-token", "chatgpt_account_id": "chatgpt-test-account"}
			account.Extra = map[string]any{"openai_oauth_responses_websockets_v2_mode": OpenAIWSIngressModeCtxPool}
			dialer := &httpWSTestDialer{}
			svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
			c, _ := newHTTPWSPolicyTestContext("/v1/messages")
			body := []byte(fmt.Sprintf(`{"model":"gpt-6.1-sol","stream":%t,"system":"Reply only OK.","messages":[{"role":"user","content":"hello"}],"max_tokens":10,"output_config":{"effort":"low"}}`, stream))
			result, err := svc.ForwardAsAnthropic(context.Background(), c, account, body, "", "")
			require.NoError(t, err)
			require.True(t, result.OpenAIWSMode)
			require.Empty(t, upstream.requests)
			dialer.mu.Lock()
			count := dialer.count
			headers := cloneHeader(dialer.headers[0])
			dialer.mu.Unlock()
			require.Equal(t, 1, count)
			require.Equal(t, openAIWSBetaV2Value, headers.Get("OpenAI-Beta"))
			require.NotEmpty(t, headers.Get("originator"))
			require.NotEmpty(t, headers.Get("session_id"))
		})
	}
}
