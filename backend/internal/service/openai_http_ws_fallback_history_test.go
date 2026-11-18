package service

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"testing"

	"github.com/stretchr/testify/require"
)

// 2026-11-19：完整历史只允许来自本次成功 HTTP 兜底，失败、取消、缺失 output
// 及普通 HTTP 均不得产生伪造的可恢复父链。
func TestHTTPWSFallbackHistoryRequiresSuccessfulCompleteResponse(t *testing.T) {
	for _, tc := range []struct {
		name       string
		payload    string
		prefix     string
		suffix     string
		event      string
		fallback   bool
		sent       bool
		forwardErr bool
		cancel     bool
		want       bool
	}{
		{name: "json", payload: httpWSPolicyResponse, fallback: true, want: true},
		{name: "sse", payload: `{"type":"response.completed","response":` + httpWSPolicyResponse + `}`, event: "response.completed", fallback: true, want: true},
		{name: "done", payload: `{"type":"response.done","response":` + httpWSPolicyResponse + `}`, event: "response.done", fallback: true, want: true},
		{name: "done_sentinel_keeps_terminal", payload: httpWSPolicyResponse, suffix: "[DONE]", fallback: true, want: true},
		{name: "usage_tail_keeps_terminal", payload: httpWSPolicyResponse, suffix: `{"usage":{"input_tokens":12}}`, fallback: true, want: true},
		{name: "empty_terminal_after_delta", prefix: `{"type":"response.output_text.delta","delta":"actual content"}`, payload: `{"type":"response.completed","response":{"id":"resp_http_ws_policy","status":"completed","output":[]}}`, fallback: true},
		{name: "ordinary_http", payload: httpWSPolicyResponse},
		{name: "already_sent_ws", payload: httpWSPolicyResponse, fallback: true, sent: true},
		{name: "forward_error", payload: httpWSPolicyResponse, fallback: true, forwardErr: true},
		{name: "cancelled", payload: httpWSPolicyResponse, fallback: true, cancel: true},
		{name: "failed", payload: `{"id":"resp_http_ws_policy","status":"failed","output":[]}`, fallback: true},
		{name: "missing_output", payload: `{"id":"resp_http_ws_policy","status":"completed"}`, fallback: true},
		{name: "reference_output", payload: `{"id":"resp_http_ws_policy","status":"completed","output":[{"type":"item_reference","id":"external"}]}`, fallback: true},
		{name: "invalid_output", payload: `{"id":"resp_http_ws_policy","status":"completed","output":["bad"]}`, fallback: true},
		{name: "error_envelope", payload: `{"id":"resp_http_ws_policy","status":"completed","output":[],"error":{"code":"server_error"}}`, fallback: true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			svc, _, _ := newHTTPWSPolicyTestService(t, "https://unused.example")
			attempt := &openAIHTTPWSAttempt{}
			attempt.fallback.Store(tc.fallback)
			attempt.sent.Store(tc.sent)
			replay := &openAIHTTPWSHTTPReplay{plan: &openAIHTTPWSReplayPlan{identity: "test-identity", complete: true,
				input: []json.RawMessage{json.RawMessage(`{"type":"message","role":"user","content":"question"}`)}}}
			attempt.httpReplay.Store(replay)
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			ctx = context.WithValue(ctx, openAIHTTPWSAttemptContextKey{}, attempt)
			ctx = context.WithValue(ctx, openAIHTTPWSHTTPReplayContextKey{}, replay)
			req, err := http.NewRequestWithContext(ctx, http.MethodPost, "https://unused.example/v1/responses", nil)
			require.NoError(t, err)
			if tc.prefix != "" {
				observeOpenAIHTTPWSHTTPHistory(&http.Response{Request: req}, []byte(tc.prefix), "")
			}
			observeOpenAIHTTPWSHTTPHistory(&http.Response{Request: req}, []byte(tc.payload), tc.event)
			if tc.suffix != "" {
				observeOpenAIHTTPWSHTTPHistory(&http.Response{Request: req}, []byte(tc.suffix), "")
			}
			if tc.cancel {
				cancel()
			}
			var forwardErr error
			if tc.forwardErr {
				forwardErr = errors.New("interrupted after terminal")
			}
			svc.completeOpenAIHTTPWSHTTPHistory(ctx, attempt, &OpenAIForwardResult{ResponseID: "resp_http_ws_policy"}, forwardErr)
			items, found := svc.getOpenAIHTTPWSReplayStore().Get("test-identity", "resp_http_ws_policy")
			require.Equal(t, tc.want, found)
			if tc.want {
				require.Len(t, items, 2)
			}
		})
	}
}

// 2026-11-19：WS 转 SSE 的成功终态若漏掉先前 delta，不能登记为完整可恢复历史；
// 没有任何输出事件的合法空响应仍可保存，保持原有协议语义。
func TestOpenAIHTTPWSPoolOutputDeltasPreventEmptyHistoryCapture(t *testing.T) {
	for _, withDelta := range []bool{false, true} {
		name := "legitimate_empty"
		if withDelta {
			name = "missing_generated_output"
		}
		t.Run(name, func(t *testing.T) {
			events := []string{`{"type":"response.created","response":{"id":"resp_empty_terminal","status":"in_progress"}}`}
			if withDelta {
				events = append(events, `{"type":"response.output_text.delta","response_id":"resp_empty_terminal","delta":"actual content"}`)
			}
			events = append(events, `{"type":"response.completed","response":{"id":"resp_empty_terminal","status":"completed","output":[]}}`)
			svc, account := newHTTPWSTestService(t, &httpWSTestDialer{events: events})
			req, attempt := newHTTPWSTestRequest(t, "empty-output-history", `{"input":[{"type":"message","role":"user","content":"question"}]}`)
			resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
			require.NoError(t, err)
			_, err = io.ReadAll(resp.Body)
			require.NoError(t, err)
			require.NoError(t, resp.Body.Close())
			identity, err := openAIHTTPWSHistoryIdentity(req, "", account, attempt)
			require.NoError(t, err)
			_, found := svc.getOpenAIHTTPWSReplayStore().Get(identity, "resp_empty_terminal")
			require.Equal(t, !withDelta, found)
		})
	}
}
