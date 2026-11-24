package service

import (
	"context"
	"errors"
	"fmt"
	"io"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/pkg/ctxkey"
	"github.com/stretchr/testify/require"
)

func httpWSHeaderOverrideContext(ctx context.Context, userID int64, requestID string) context.Context {
	groupID := int64(9)
	ctx = context.WithValue(ctx, ctxkey.RequestID, requestID)
	return WithHeaderOverrideRequestScope(ctx, userID, 31, &groupID)
}

func configureHTTPWSHeaderOverrides(account *Account) {
	account.Type = AccountTypeAPIKey
	if account.Credentials == nil {
		account.Credentials = make(map[string]any)
	}
	account.Credentials[credKeyHeaderOverrideEnabled] = true
	account.Credentials[credKeyHeaderOverrides] = map[string]any{
		"x-gateway-user":    "{{user_id}}",
		"x-gateway-key":     "{{api_key_id}}",
		"x-gateway-group":   "{{group_id}}",
		"x-gateway-request": "{{request_id}}",
		"x-request-id":      "{{request_id}}",
		"x-mixed-identity":  "user={{user_id}};request={{request_id}}",
		"x-static":          "static-relay-value",
	}
}

func TestOpenAIHTTPWSHeaderOverridesKeepConnectionScopeAndContinuation(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	configureHTTPWSHeaderOverrides(account)
	for i := 0; i < 2; i++ {
		body := `{"input":[]}`
		if i == 1 {
			body = `{"input":[],"previous_response_id":"resp_1"}`
		}
		req, _ := newHTTPWSTestRequest(t, "user:7/key:31/session:1", body)
		requestID := fmt.Sprintf("request-%d", i)
		req = req.WithContext(httpWSHeaderOverrideContext(req.Context(), 7, requestID))
		account.ApplyHeaderOverrides(req.Context(), req.Header)
		originalHeaders := cloneHeader(req.Header)
		resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
		require.NoError(t, err)
		_, err = io.ReadAll(resp.Body)
		require.NoError(t, err)
		require.NoError(t, resp.Body.Close())
		require.Equal(t, originalHeaders, req.Header, "HTTP fallback headers must remain unchanged")
		require.Equal(t, map[string]string{
			"x-gateway-user": "7", "x-gateway-key": "31", "x-gateway-group": "9",
		}, headerOverrideReportedHeaders(req.Context(), account.ID), "WS audit must omit request-only identities")
	}
	require.Equal(t, 1, dialer.count, "changing request IDs must not break continuation reuse")
	header := dialer.headers[0]
	for _, name := range []string{"x-gateway-request", "x-request-id", "x-mixed-identity"} {
		require.Empty(t, headerValuesFold(header, name))
	}
	require.Equal(t, []string{"7"}, headerValuesFold(header, "x-gateway-user"))
	require.Equal(t, []string{"31"}, headerValuesFold(header, "x-gateway-key"))
	require.Equal(t, []string{"9"}, headerValuesFold(header, "x-gateway-group"))
	require.Equal(t, []string{"static-relay-value"}, headerValuesFold(header, "x-static"))
	require.JSONEq(t, `"resp_1"`, string(dialer.conns[0].writes[1]["previous_response_id"]))
}

func TestOpenAIHTTPWSHeaderOverridesSeparateReportedUsers(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	configureHTTPWSHeaderOverrides(account)
	for _, userID := range []int64{7, 8} {
		// Hold the adapter scope constant to verify the handshake identity itself
		// includes the expanded user header, in addition to normal scope isolation.
		req, _ := newHTTPWSTestRequest(t, "same-test-scope", `{"input":[]}`)
		req = req.WithContext(httpWSHeaderOverrideContext(req.Context(), userID, "request-1"))
		account.ApplyHeaderOverrides(req.Context(), req.Header)
		resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
		require.NoError(t, err)
		_, err = io.ReadAll(resp.Body)
		require.NoError(t, err)
		require.NoError(t, resp.Body.Close())
	}
	require.Equal(t, 2, dialer.count)
	require.Equal(t, []string{"7"}, headerValuesFold(dialer.headers[0], "x-gateway-user"))
	require.Equal(t, []string{"8"}, headerValuesFold(dialer.headers[1], "x-gateway-user"))
}

func TestHTTPWSPolicyHeaderOverridesPreserveHTTPFallbackIdentity(t *testing.T) {
	svc, account, upstream := newHTTPWSPolicyTestService(t, "https://unused.example")
	configureHTTPWSHeaderOverrides(account)
	dialer := &httpWSTestDialer{dialErr: errors.New("connection refused")}
	svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
	c, _ := newHTTPWSPolicyTestContext("/v1/responses")
	ctx := httpWSHeaderOverrideContext(c.Request.Context(), 7, "fallback-request")
	c.Request = c.Request.WithContext(ctx)
	result, err := svc.Forward(ctx, c, account, []byte(`{"model":"gpt-6.1-sol","stream":false,"input":"hello"}`))
	require.NoError(t, err)
	require.NotNil(t, result)
	require.False(t, result.OpenAIWSMode)
	require.Equal(t, 4, dialer.count)
	for _, header := range dialer.headers {
		require.Empty(t, headerValuesFold(header, "x-gateway-request"))
		require.Empty(t, headerValuesFold(header, "x-request-id"))
		require.Empty(t, headerValuesFold(header, "x-mixed-identity"))
	}
	require.Len(t, upstream.requests, 1)
	require.Equal(t, []string{"fallback-request"}, headerValuesFold(upstream.requests[0].Header, "x-gateway-request"))
	require.Equal(t, []string{"user=7;request=fallback-request"}, headerValuesFold(upstream.requests[0].Header, "x-mixed-identity"))
	reported := headerOverrideReportedHeaders(ctx, account.ID)
	require.Equal(t, "fallback-request", reported["x-gateway-request"])
	require.Equal(t, "7", reported["x-gateway-user"])
}
