package service

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"testing"

	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

// 2026-11-19：通过实际 Forward 验证恢复完整父历史后只生成一次，避免只测缓存
// 的拼接函数却漏掉 OAuth 转换、连接切换和最终 HTTP 回退中的历史丢失。
type httpWSRecoveryTestDialer struct {
	mu          sync.Mutex
	dials       int
	status      int
	headers     []http.Header
	writes      [][]byte
	firstOutput json.RawMessage
	firstStatus string
}

func (d *httpWSRecoveryTestDialer) Dial(_ context.Context, _ string, headers http.Header, _ string) (openAIWSClientConn, int, http.Header, error) {
	d.mu.Lock()
	defer d.mu.Unlock()
	d.dials++
	d.headers = append(d.headers, cloneHeader(headers))
	if d.status != 0 {
		return nil, d.status, nil, &openAIWSHandshakeError{Err: errors.New("test handshake refused")}
	}
	return &httpWSRecoveryTestConn{dialer: d}, http.StatusSwitchingProtocols, nil, nil
}

type httpWSRecoveryTestConn struct {
	mu      sync.Mutex
	dialer  *httpWSRecoveryTestDialer
	pending [][]byte
	closed  bool
}

func (c *httpWSRecoveryTestConn) WriteJSON(_ context.Context, value any) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return io.ErrClosedPipe
	}
	body, err := json.Marshal(value)
	if err != nil {
		return err
	}
	c.dialer.mu.Lock()
	c.dialer.writes = append(c.dialer.writes, body)
	number := len(c.dialer.writes)
	output := json.RawMessage(`[{"id":"msg_parent","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"parent answer","annotations":[]}]}]`)
	status := "completed"
	if number == 1 {
		if c.dialer.firstOutput != nil {
			output = c.dialer.firstOutput
		}
		if c.dialer.firstStatus != "" {
			status = c.dialer.firstStatus
		}
	}
	c.dialer.mu.Unlock()
	id := fmt.Sprintf("resp_recovery_%d", number)
	response := map[string]any{"id": id, "object": "response", "status": status, "model": "gpt-6.1-sol", "output": output,
		"usage": map[string]int{"input_tokens": 12, "output_tokens": 7, "total_tokens": 19}}
	if status == "failed" {
		response["error"] = map[string]string{"code": "server_error", "message": "parent failed"}
	}
	created, _ := json.Marshal(map[string]any{"type": "response.created", "response": map[string]any{"id": id, "status": "in_progress"}})
	terminal, _ := json.Marshal(map[string]any{"type": "response." + status, "response": response})
	c.pending = [][]byte{created, terminal}
	return nil
}

func (c *httpWSRecoveryTestConn) ReadMessage(context.Context) ([]byte, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil, io.ErrClosedPipe
	}
	if len(c.pending) == 0 {
		return nil, io.EOF
	}
	value := c.pending[0]
	c.pending = c.pending[1:]
	return value, nil
}

func (c *httpWSRecoveryTestConn) Ping(context.Context) error { return nil }
func (c *httpWSRecoveryTestConn) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.closed = true
	return nil
}

func newHTTPWSRecoveryTestService(t *testing.T) (*OpenAIGatewayService, *Account, *httpUpstreamRecorder, *httpWSRecoveryTestDialer) {
	t.Helper()
	svc, account, upstream := newHTTPWSPolicyTestService(t, "https://unused.example")
	account.Type = AccountTypeOAuth
	account.Credentials = map[string]any{"access_token": "oauth-recovery-token", "chatgpt_account_id": "chatgpt-recovery-account"}
	account.Extra = map[string]any{"openai_oauth_responses_websockets_v2_mode": OpenAIWSIngressModeCtxPool}
	dialer := &httpWSRecoveryTestDialer{}
	svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
	upstream.resp.Header.Set("Content-Type", "text/event-stream")
	upstream.resp.Body = io.NopCloser(strings.NewReader("data: {\"type\":\"response.completed\",\"response\":" + httpWSPolicyResponse + "}\n\n"))
	return svc, account, upstream, dialer
}

func httpWSRecoveryTestBody(t *testing.T, input any, previous string, search bool) []byte {
	t.Helper()
	request := map[string]any{"model": "gpt-6.1-sol", "stream": false, "instructions": "Preserve the conversation.", "input": input}
	if previous != "" {
		request["previous_response_id"] = previous
	}
	if search {
		request["tools"] = []any{map[string]any{"type": "web_search"}}
	}
	body, err := json.Marshal(request)
	require.NoError(t, err)
	return body
}

func httpWSRecoveryTestUser(text string) map[string]any {
	return map[string]any{"type": "message", "role": "user", "content": text}
}

func forwardHTTPWSRecoveryTest(t *testing.T, svc *OpenAIGatewayService, account *Account, body []byte) *OpenAIForwardResult {
	t.Helper()
	c, recorder := newHTTPWSPolicyTestContext("/v1/responses")
	result, err := svc.Forward(context.Background(), c, account, body)
	require.NoError(t, err)
	require.NotNil(t, result)
	require.Equal(t, http.StatusOK, recorder.Code)
	return result
}

func requireHTTPWSRecoveryHistory(t *testing.T, payload []byte, parent, child string) {
	t.Helper()
	require.False(t, gjson.GetBytes(payload, "previous_response_id").Exists(), "restored complete history must not retain a dead socket's parent ID")
	items := gjson.GetBytes(payload, "input").Array()
	require.Len(t, items, 3)
	require.Equal(t, "user", items[0].Get("role").String())
	require.Equal(t, len(parent), len(items[0].Get("content").String()))
	require.Equal(t, sha256.Sum256([]byte(parent)), sha256.Sum256([]byte(items[0].Get("content").String())))
	require.Equal(t, "assistant", items[1].Get("role").String())
	require.Equal(t, "parent answer", items[1].Get("content.0.text").String())
	require.Equal(t, "user", items[2].Get("role").String())
	require.Equal(t, len(child), len(items[2].Get("content").String()))
	require.Equal(t, sha256.Sum256([]byte(child)), sha256.Sum256([]byte(items[2].Get("content").String())))
}

func TestHTTPWSRecoveryForwardRestoresCompletedHistoryOnFreshSocket(t *testing.T) {
	for _, search := range []bool{false, true} {
		t.Run(fmt.Sprintf("activate_search_%t", search), func(t *testing.T) {
			svc, account, upstream, dialer := newHTTPWSRecoveryTestService(t)
			first := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("parent question")}, "", false))
			require.Equal(t, "resp_recovery_1", first.ResponseID)
			if !search {
				svc.getOpenAIWSConnPool().ClearAccount(account.ID)
			}
			second := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("child question")}, first.ResponseID, search))
			require.True(t, second.OpenAIWSMode)
			require.Equal(t, "resp_recovery_2", second.ResponseID)
			require.Equal(t, 2, dialer.dials)
			require.Len(t, dialer.writes, 2, "one generation per turn, without replay")
			requireHTTPWSRecoveryHistory(t, dialer.writes[1], "parent question", "child question")
			if search {
				require.Equal(t, "web_search", gjson.GetBytes(dialer.writes[1], "tools.0.type").String())
			}
			require.Empty(t, upstream.requests)
		})
	}
}

func TestHTTPWSRecoveryForwardAPIKeyScalarInputBecomesMessageHistory(t *testing.T) {
	for _, passthrough := range []bool{false, true} {
		t.Run(fmt.Sprintf("passthrough_%t", passthrough), func(t *testing.T) {
			svc, account, upstream := newHTTPWSPolicyTestService(t, "https://upstream.example")
			account.Extra["openai_passthrough"] = passthrough
			dialer := &httpWSRecoveryTestDialer{}
			svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
			first := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, "first", "", false))
			require.Equal(t, "resp_recovery_1", first.ResponseID)
			require.Equal(t, gjson.String, gjson.GetBytes(dialer.writes[0], "input").Type, "API Key forwarding must exercise the original scalar-input path")
			svc.getOpenAIWSConnPool().ClearAccount(account.ID)
			second := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, "next", first.ResponseID, false))
			require.True(t, second.OpenAIWSMode)
			require.Equal(t, "resp_recovery_2", second.ResponseID)
			require.Equal(t, 2, dialer.dials)
			require.Len(t, dialer.writes, 2, "each turn must generate only once")
			requireHTTPWSRecoveryHistory(t, dialer.writes[1], "first", "next")
			items := gjson.GetBytes(dialer.writes[1], "input").Array()
			for i, item := range items {
				require.True(t, item.IsObject(), "recovered input item %d must not be a bare JSON string", i)
				require.Equal(t, "message", item.Get("type").String())
			}
			require.Equal(t, "first", items[0].Get("content").String())
			require.Equal(t, "parent answer", items[1].Get("content.0.text").String())
			require.Equal(t, "next", items[2].Get("content").String())
			require.Empty(t, upstream.requests)
		})
	}
}

func TestHTTPWSRecoveryForwardTurnStateChangeUsesFreshSocketWithHistory(t *testing.T) {
	svc, account, upstream := newHTTPWSPolicyTestService(t, "https://upstream.example")
	account.Extra["openai_passthrough"] = true
	dialer := &httpWSRecoveryTestDialer{}
	svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
	c, _ := newHTTPWSPolicyTestContext("/v1/responses")
	c.Request.Header.Set(openAIWSTurnStateHeader, "parent-turn-state")
	first, err := svc.Forward(context.Background(), c, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("parent question")}, "", false))
	require.NoError(t, err)
	require.Equal(t, "resp_recovery_1", first.ResponseID)

	// 同一授权及会话的 turn-state 更新不能跨旧握手复用连接，但仍可用完整父历史恢复。
	c, _ = newHTTPWSPolicyTestContext("/v1/responses")
	c.Request.Header.Set(openAIWSTurnStateHeader, "next-turn-state")
	second, err := svc.Forward(context.Background(), c, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("child question")}, first.ResponseID, false))
	require.NoError(t, err)
	require.True(t, second.OpenAIWSMode)
	require.Equal(t, 2, dialer.dials, "changed turn-state must not reuse the old handshake")
	require.Equal(t, "parent-turn-state", dialer.headers[0].Get(openAIWSTurnStateHeader))
	require.Equal(t, "next-turn-state", dialer.headers[1].Get(openAIWSTurnStateHeader))
	require.Len(t, dialer.writes, 2)
	requireHTTPWSRecoveryHistory(t, dialer.writes[1], "parent question", "child question")
	require.Empty(t, upstream.requests)
}

func TestHTTPWSRecoveryForwardCompletePrefixIsNotDuplicated(t *testing.T) {
	svc, account, upstream := newHTTPWSPolicyTestService(t, "https://upstream.example")
	account.Extra["openai_passthrough"] = true
	output := json.RawMessage(`[{"id":"msg_parent","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"parent answer","annotations":[]}]}]`)
	dialer := &httpWSRecoveryTestDialer{firstOutput: output}
	svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
	first := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("parent question")}, "", false))
	svc.getOpenAIWSConnPool().ClearAccount(account.ID)
	var completedOutput []json.RawMessage
	require.NoError(t, json.Unmarshal(output, &completedOutput))
	input := []any{httpWSRecoveryTestUser("parent question"), completedOutput[0], httpWSRecoveryTestUser("child question")}
	second := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, input, first.ResponseID, false))
	require.True(t, second.OpenAIWSMode)
	require.Len(t, dialer.writes, 2)
	requireHTTPWSRecoveryHistory(t, dialer.writes[1], "parent question", "child question")
	require.Empty(t, upstream.requests)
}

func TestHTTPWSRecoveryForwardPreservesToolCallAndOutput(t *testing.T) {
	for _, tc := range []struct {
		name           string
		oauth          bool
		upstreamCallID string
		wantCallID     string
	}{
		{"apikey_original", false, "call_parent", "call_parent"},
		{"oauth_native", true, "fc_parent", "fc_parent"},
		{"oauth_legacy_prefix", true, "call_parent", "fc_parent"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			svc, account, upstream := newHTTPWSPolicyTestService(t, "https://upstream.example")
			dialer := &httpWSRecoveryTestDialer{}
			svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
			if tc.oauth {
				svc, account, upstream, dialer = newHTTPWSRecoveryTestService(t)
			}
			// 2026-11-19：保留上游 legacy call_→子轮 OAuth fc_ 的真实转换差异，
			// 同时验证原生 fc_ 与 API Key 原 call_；不能通过放宽工具配对使测试通过。
			dialer.firstOutput = json.RawMessage(fmt.Sprintf(`[{"id":"fc_parent","type":"function_call","status":"completed","call_id":%q,"name":"lookup","arguments":"{\"value\":1}"}]`, tc.upstreamCallID))
			first := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("look up a value")}, "", false))
			svc.getOpenAIWSConnPool().ClearAccount(account.ID)
			child := []any{map[string]any{"type": "function_call_output", "call_id": tc.upstreamCallID, "output": `{"value":42}`}, httpWSRecoveryTestUser("explain that result")}
			second := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, child, first.ResponseID, false))
			require.True(t, second.OpenAIWSMode)
			require.Len(t, dialer.writes, 2)
			items := gjson.GetBytes(dialer.writes[1], "input").Array()
			require.Len(t, items, 4)
			require.Equal(t, "look up a value", items[0].Get("content").String())
			require.Equal(t, "function_call", items[1].Get("type").String())
			require.Equal(t, tc.wantCallID, items[1].Get("call_id").String())
			require.Equal(t, "lookup", items[1].Get("name").String())
			require.JSONEq(t, `{"value":1}`, items[1].Get("arguments").String())
			require.Equal(t, "function_call_output", items[2].Get("type").String())
			require.Equal(t, items[1].Get("call_id").String(), items[2].Get("call_id").String())
			require.JSONEq(t, `{"value":42}`, items[2].Get("output").String())
			require.Equal(t, "explain that result", items[3].Get("content").String())
			require.False(t, gjson.GetBytes(dialer.writes[1], "previous_response_id").Exists())
			require.Empty(t, upstream.requests)
		})
	}
}

func TestHTTPWSRecoveryForwardHTTPFallbackCarriesCompleteHistory(t *testing.T) {
	for _, capacity := range []bool{false, true} {
		t.Run(fmt.Sprintf("capacity_%t", capacity), func(t *testing.T) {
			svc, account, upstream, dialer := newHTTPWSRecoveryTestService(t)
			parent, child := "parent question", "child question"
			if capacity {
				parent, child = strings.Repeat("p", 8<<20), strings.Repeat("c", 9<<20)
			}
			first := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser(parent)}, "", false))
			svc.getOpenAIWSConnPool().ClearAccount(account.ID)
			if !capacity {
				dialer.status = http.StatusForbidden
			}
			second := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser(child)}, first.ResponseID, false))
			require.False(t, second.OpenAIWSMode)
			require.Len(t, upstream.requests, 1)
			require.Len(t, dialer.writes, 1, "the child must never be generated over WS before HTTP fallback")
			if capacity {
				require.Equal(t, 1, dialer.dials, "oversized restored payload must be detected before a new handshake")
			} else {
				require.Equal(t, 5, dialer.dials, "initial successful parent, then four failed handshakes")
			}
			requireHTTPWSRecoveryHistory(t, upstream.lastBody, parent, child)
			require.False(t, gjson.GetBytes(upstream.lastBody, "type").Exists())
			require.True(t, gjson.GetBytes(upstream.lastBody, "stream").Bool(), "OAuth's prepared HTTP streaming behavior must survive reconstruction")
		})
	}
}

func TestHTTPWSRecoveryForwardHTTPFallbackCompletionCanContinueOnWebSocket(t *testing.T) {
	svc, account, upstream, dialer := newHTTPWSRecoveryTestService(t)
	first := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("parent question")}, "", false))
	svc.getOpenAIWSConnPool().ClearAccount(account.ID)
	dialer.status = http.StatusForbidden
	second := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("child question")}, first.ResponseID, false))
	require.False(t, second.OpenAIWSMode)
	require.Len(t, upstream.requests, 1)
	requireHTTPWSRecoveryHistory(t, upstream.lastBody, "parent question", "child question")
	require.Len(t, dialer.writes, 1)

	// 2026-11-19：HTTP 兜底成功的子轮也必须记录完整已完成输出，下一轮才能
	// 回到 WS 并恢复整段历史；否则切换一次传输就会使后续 previous_response_id 失效。
	dialer.status = 0
	third := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("grandchild question")}, "resp_http_ws_policy", false))
	require.True(t, third.OpenAIWSMode)
	require.Len(t, upstream.requests, 1)
	require.Equal(t, 6, dialer.dials, "parent handshake, four refused child handshakes, then one grandchild handshake")
	require.Len(t, dialer.writes, 2, "the parent and grandchild each generate once over WS; the child generates once over HTTP")
	payload := dialer.writes[1]
	require.False(t, gjson.GetBytes(payload, "previous_response_id").Exists())
	items := gjson.GetBytes(payload, "input").Array()
	require.Len(t, items, 5)
	require.Equal(t, "user", items[0].Get("role").String())
	require.Equal(t, "parent question", items[0].Get("content").String())
	require.Equal(t, "assistant", items[1].Get("role").String())
	require.Equal(t, "parent answer", items[1].Get("content.0.text").String())
	require.Equal(t, "user", items[2].Get("role").String())
	require.Equal(t, "child question", items[2].Get("content").String())
	require.Equal(t, "assistant", items[3].Get("role").String())
	require.Equal(t, "hello from pool", items[3].Get("content.0.text").String())
	require.Equal(t, "user", items[4].Get("role").String())
	require.Equal(t, "grandchild question", items[4].Get("content").String())
}

func TestHTTPWSRecoveryForwardNeverRestoresAcrossScopeOrCredential(t *testing.T) {
	for _, change := range []string{"api_key", "session", "authorization"} {
		t.Run(change, func(t *testing.T) {
			svc, account, upstream, dialer := newHTTPWSRecoveryTestService(t)
			first := forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("private parent history")}, "", false))
			svc.getOpenAIWSConnPool().ClearAccount(account.ID)
			c, _ := newHTTPWSPolicyTestContext("/v1/responses")
			switch change {
			case "api_key":
				key, exists := c.Get("api_key")
				require.True(t, exists)
				key.(*APIKey).ID++
			case "session":
				c.Request.Header.Set("session-id", "different-customer-session")
			case "authorization":
				account.Credentials["access_token"] = "rotated-or-unrelated-token"
			}
			_, err := svc.Forward(context.Background(), c, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("child")}, first.ResponseID, false))
			require.Error(t, err)
			var incomplete *openAIHTTPWSContinuationError
			require.ErrorAs(t, err, &incomplete)
			require.Len(t, dialer.writes, 1)
			require.Empty(t, upstream.requests, "another scope must never receive cached history through HTTP")
		})
	}
}

func TestHTTPWSRecoveryForwardUnknownOrUnsuccessfulParentNeverInventsHistory(t *testing.T) {
	for _, status := range []string{"unknown", "failed", "incomplete"} {
		t.Run(status, func(t *testing.T) {
			svc, account, upstream, dialer := newHTTPWSRecoveryTestService(t)
			previous, initialWrites := "resp_unknown_parent", 0
			if status != "unknown" {
				dialer.firstStatus = status
				c, _ := newHTTPWSPolicyTestContext("/v1/responses")
				_, _ = svc.Forward(context.Background(), c, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("unconfirmed parent")}, "", false))
				previous, initialWrites = "resp_recovery_1", 1
				require.Len(t, dialer.writes, initialWrites)
			}
			svc.getOpenAIWSConnPool().ClearAccount(account.ID)
			// 未知的上游 response ID 可能属于 store=true；保留原始 prev 交由
			// 上游验证，禁止把未知或失败父轮伪造成已恢复的完整本地历史。
			forwardHTTPWSRecoveryTest(t, svc, account, httpWSRecoveryTestBody(t, []any{httpWSRecoveryTestUser("child")}, previous, false))
			require.Len(t, dialer.writes, initialWrites+1)
			sent := dialer.writes[initialWrites]
			require.Equal(t, previous, gjson.GetBytes(sent, "previous_response_id").String())
			items := gjson.GetBytes(sent, "input").Array()
			require.Len(t, items, 1)
			require.Equal(t, "child", items[0].Get("content").String())
			require.Empty(t, upstream.requests)
		})
	}
}
