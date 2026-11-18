package service

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"testing"
	"time"

	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

func TestOpenAIHTTPWSPoolKnownContinuationWithoutHistoryRejectsUnavailableIdentityWithoutFallback(t *testing.T) {
	for _, change := range []string{"scope", "authorization", "session", "turn_state", "closed_connection"} {
		t.Run(change, func(t *testing.T) {
			dialer := &httpWSTestDialer{}
			svc, account := newHTTPWSTestService(t, dialer)
			// 2026-11-19：允许已证明完整的父历史自动恢复；此处专门覆盖
			// 已绑定连接但父链仍未知的情形，不能仅凭本轮成功就伪造完整历史。
			first, _ := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":"first","previous_response_id":"resp_unknown_parent"}`)
			response, err := svc.doOpenAIHTTPWSPool(first, "", account)
			require.NoError(t, err)
			_, err = io.ReadAll(response.Body)
			require.NoError(t, err)
			require.NoError(t, response.Body.Close())
			_, bound := svc.getOpenAIWSStateStore().GetResponseConn("resp_1")
			require.True(t, bound)

			scope := "key:1/session:1"
			if change == "scope" {
				scope = "key:2/session:1"
			}
			body := `{"input":[{"role":"user","content":"next"}],"previous_response_id":"resp_1"}`
			next, attempt := newHTTPWSTestRequest(t, scope, body)
			switch change {
			case "authorization":
				next.Header.Set("Authorization", "Bearer other-credential")
			case "session":
				next.Header.Set("session_id", "other-session")
			case "turn_state":
				next.Header.Set(openAIWSTurnStateHeader, "new-state")
			case "closed_connection":
				pool := svc.getOpenAIWSConnPool()
				connID, _ := svc.getOpenAIWSStateStore().GetResponseConn("resp_1")
				pool.evictConn(account.ID, connID)
			}
			response, handled, err := svc.connectOpenAIHTTPWSPool(next, "", account, attempt)
			var continuationErr *openAIHTTPWSContinuationError
			require.ErrorAs(t, err, &continuationErr)
			require.Contains(t, err.Error(), "complete input history")
			require.Contains(t, err.Error(), "omit previous_response_id")
			require.True(t, handled)
			require.Nil(t, response)
			require.False(t, attempt.sent.Load())
			require.False(t, attempt.fallback.Load())
			require.Equal(t, 1, dialer.count, "a bound continuation must not dial a different identity")
			require.Len(t, dialer.conns[0].writes, 1, "the rejected continuation must never be sent")
			original, err := next.GetBody()
			require.NoError(t, err)
			preserved, err := io.ReadAll(original)
			require.NoError(t, err)
			require.NoError(t, original.Close())
			require.Equal(t, body, string(preserved), "never strip previous_response_id or rewrite the user's input")
		})
	}
}

func TestHTTPWSPolicyKnownContinuationReturns400WithoutHTTPOrFailover(t *testing.T) {
	for _, oauth := range []bool{false, true} {
		for _, passthrough := range []bool{false, true} {
			for _, stream := range []bool{false, true} {
				t.Run(fmt.Sprintf("oauth_%t_passthrough_%t_stream_%t", oauth, passthrough, stream), func(t *testing.T) {
					svc, account, upstream := newHTTPWSPolicyTestService(t, "https://unused.example")
					if oauth {
						account.Type = AccountTypeOAuth
						account.Credentials = map[string]any{"access_token": "oauth-test-token", "chatgpt_account_id": "chatgpt-test-account"}
						account.Extra = map[string]any{"openai_oauth_responses_websockets_v2_mode": OpenAIWSIngressModeCtxPool}
					}
					account.Extra["openai_passthrough"] = passthrough
					dialer := &httpWSTestDialer{}
					svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
					svc.getOpenAIWSStateStore().BindResponseConn("resp_bound", "missing-original-connection", time.Minute)
					c, recorder := newHTTPWSPolicyTestContext("/v1/responses")
					body := []byte(fmt.Sprintf(`{"model":"gpt-6.1-sol","stream":%t,"input":[{"role":"user","content":"next"}],"previous_response_id":"resp_bound"}`, stream))
					original := string(body)
					result, err := svc.Forward(context.Background(), c, account, body)
					var continuationErr *openAIHTTPWSContinuationError
					require.ErrorAs(t, err, &continuationErr)
					var failover *UpstreamFailoverError
					require.False(t, errors.As(err, &failover))
					require.Nil(t, result)
					require.Equal(t, http.StatusBadRequest, recorder.Code)
					require.Equal(t, "invalid_request_error", gjson.GetBytes(recorder.Body.Bytes(), "error.type").String())
					require.Equal(t, "ctx_pool_continuation_unavailable", gjson.GetBytes(recorder.Body.Bytes(), "error.code").String())
					require.Equal(t, "previous_response_id", gjson.GetBytes(recorder.Body.Bytes(), "error.param").String())
					require.Contains(t, recorder.Body.String(), "complete input history")
					require.Zero(t, dialer.count)
					require.Empty(t, upstream.requests)
					require.Equal(t, original, string(body))
				})
			}
		}
	}
}

func TestHTTPWSPolicyOAuthLargeBoundContinuationDoesNotFallBack(t *testing.T) {
	svc, account, upstream := newHTTPWSPolicyTestService(t, "https://unused.example")
	account.Type = AccountTypeOAuth
	account.Credentials = map[string]any{"access_token": "oauth-test-token", "chatgpt_account_id": "chatgpt-test-account"}
	account.Extra = map[string]any{"openai_oauth_responses_websockets_v2_mode": OpenAIWSIngressModeCtxPool}
	dialer := &httpWSTestDialer{}
	svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
	svc.getOpenAIWSStateStore().BindResponseConn("resp_bound", "original-ws-connection", time.Minute)
	c, recorder := newHTTPWSPolicyTestContext("/v1/responses")
	body := []byte(`{"model":"gpt-6.1-sol","stream":false,"input":"` + strings.Repeat("a", 17<<20) + `","previous_response_id":"resp_bound"}`)
	original := string(body)
	result, err := svc.Forward(context.Background(), c, account, body)
	var continuationErr *openAIHTTPWSContinuationError
	require.ErrorAs(t, err, &continuationErr)
	require.Contains(t, continuationErr.Error(), "complete input history")
	var capacityErr *openAIHTTPWSCodexCapacityError
	require.False(t, errors.As(err, &capacityErr), "a known WS continuation must not authorize HTTP capacity fallback")
	var failover *UpstreamFailoverError
	require.False(t, errors.As(err, &failover))
	require.Nil(t, result)
	require.Equal(t, http.StatusBadRequest, recorder.Code)
	require.Equal(t, "ctx_pool_continuation_unavailable", gjson.GetBytes(recorder.Body.Bytes(), "error.code").String())
	require.Contains(t, recorder.Body.String(), "complete input history")
	require.Contains(t, recorder.Body.String(), "omit previous_response_id")
	require.Zero(t, dialer.count)
	require.Empty(t, upstream.requests)
	require.Equal(t, original, string(body))
}

func TestOpenAIHTTPWSPoolUnknownContinuationKeepsExistingPolicy(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":"next","previous_response_id":"resp_unbound"}`)
	response, handled, err := svc.connectOpenAIHTTPWSPool(req, "", account, attempt)
	require.NoError(t, err)
	require.True(t, handled)
	_, err = io.ReadAll(response.Body)
	require.NoError(t, err)
	require.NoError(t, response.Body.Close())
	require.True(t, attempt.sent.Load())
	require.Equal(t, 1, dialer.count)
	require.JSONEq(t, `"resp_unbound"`, string(dialer.conns[0].writes[0]["previous_response_id"]))
}
