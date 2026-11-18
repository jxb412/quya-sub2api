package service

import (
	"io"
	"net/http"
	"testing"

	"github.com/stretchr/testify/require"
)

func completeHTTPWSSearchTestTurn(t *testing.T, svc *OpenAIGatewayService, account *Account, body string) {
	t.Helper()
	req, _ := newHTTPWSTestRequest(t, "key:1/session:1", body)
	resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
	require.NoError(t, err)
	_, err = io.ReadAll(resp.Body)
	require.NoError(t, err)
	require.NoError(t, resp.Body.Close())
}

func TestOpenAIHTTPWSSearchActivationFreshConnectionBeforeDispatch(t *testing.T) {
	for _, tools := range []string{
		`"tools":[{"type":"web_search"}],"input":[{"role":"user","content":"preserve my complete question"}]`,
		`"input":[{"role":"user","content":"preserve my complete question"},{"type":"additional_tools","tools":[{"type":"web_search"}]}]`,
	} {
		t.Run(tools, func(t *testing.T) {
			dialer := &httpWSTestDialer{}
			svc, account := newHTTPWSTestService(t, dialer)
			completeHTTPWSSearchTestTurn(t, svc, account, `{"input":[]}`)
			req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"model":"gpt-6.1-sol",`+tools+`}`)
			resp, handled, err := svc.connectOpenAIHTTPWSPool(req, "", account, attempt)
			require.NoError(t, err)
			require.True(t, handled)
			_, err = io.ReadAll(resp.Body)
			require.NoError(t, err)
			require.NoError(t, resp.Body.Close())
			require.True(t, attempt.sent.Load())
			require.False(t, attempt.reused.Load())
			require.False(t, attempt.fallback.Load())
			require.Equal(t, 2, dialer.count)
			require.Len(t, dialer.conns[0].writes, 1, "search turn must never reach the old socket")
			require.Len(t, dialer.conns[1].writes, 1, "search turn is submitted exactly once")
			require.Contains(t, string(dialer.conns[1].writes[0]["input"]), "preserve my complete question")
		})
	}
}

func TestOpenAIHTTPWSSearchActivationContinuationRequiresCompleteInput(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	// 2026-11-19：未知父链即使当前轮成功，也没有完整历史可供搜索切换恢复。
	completeHTTPWSSearchTestTurn(t, svc, account, `{"previous_response_id":"resp_unknown_parent","input":[]}`)
	req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"previous_response_id":"resp_1","input":[],"tools":[{"type":"web_search"}]}`)
	resp, handled, err := svc.connectOpenAIHTTPWSPool(req, "", account, attempt)
	require.ErrorContains(t, err, "complete input history")
	require.Nil(t, resp)
	require.True(t, handled)
	require.False(t, attempt.sent.Load())
	require.False(t, attempt.fallback.Load(), "partial continuation must not move to HTTP")
	require.Equal(t, 1, dialer.count)
	require.Len(t, dialer.conns[0].writes, 1, "no generation or replay on rejection")
	// Rejection releases the lease and preserves a valid non-search continuation.
	completeHTTPWSSearchTestTurn(t, svc, account, `{"previous_response_id":"resp_1","input":[]}`)
	require.Equal(t, 1, dialer.count)
	require.Len(t, dialer.conns[0].writes, 2)
}

func TestOpenAIHTTPWSSearchEnabledConnectionRemainsReusable(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	completeHTTPWSSearchTestTurn(t, svc, account, `{"input":[],"tools":[{"type":"web_search"}]}`)
	completeHTTPWSSearchTestTurn(t, svc, account, `{"previous_response_id":"resp_1","input":[],"tools":[{"type":"web_search"}]}`)
	require.Equal(t, 1, dialer.count)
	require.Len(t, dialer.conns[0].writes, 2)
}

func TestOpenAIHTTPWSSearchFreshHandshakeFailureFallsBackBeforeGeneration(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	completeHTTPWSSearchTestTurn(t, svc, account, `{"input":[]}`)
	dialer.status = http.StatusForbidden
	req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[],"tools":[{"type":"web_search"}]}`)
	resp, handled, err := svc.connectOpenAIHTTPWSPool(req, "", account, attempt)
	require.NoError(t, err)
	require.Nil(t, resp)
	require.False(t, handled)
	require.True(t, attempt.fallback.Load())
	require.False(t, attempt.sent.Load())
	require.Equal(t, 5, dialer.count, "initial generation plus four bounded search handshakes")
	require.Len(t, dialer.conns, 1)
	require.Len(t, dialer.conns[0].writes, 1, "search was never sent to the old socket")
}
