package service

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

// 2026-11-28: a compatible API-key upstream can close its SSE body without a
// final Responses object. Preserve usable output as incomplete JSON, with
// upstream usage or an authorized local estimate; reject content-free replies.
// Fixtures are synthetic;
// no production payloads, account IDs, request IDs, or credentials are stored.
const nonstreamSyntheticRequest = `{"model":"gpt-5.4","input":"synthetic probe","stream":false}`

const nonstreamMissingTerminalDelta = "event: response.created\n" +
	`data: {"type":"response.created","response":{"id":"resp_synthetic","status":"in_progress","output":[]}}` + "\n\n" +
	"event: response.output_text.delta\n" +
	`data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"partial"}` + "\n\n"

type nonstreamTerminalTestResult struct {
	context   *gin.Context
	recorder  *httptest.ResponseRecorder
	usage     *OpenAIUsage
	hasResult bool
	err       error
}

func replayNonstreamTerminalFixture(t *testing.T, passthrough bool, contentType, body string, before ...func(*gin.Context)) nonstreamTerminalTestResult {
	t.Helper()
	gin.SetMode(gin.TestMode)
	recorder := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(recorder)
	c.Request = httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(nonstreamSyntheticRequest))
	c.Request.Header.Set("Accept", "application/json")
	for _, seed := range before {
		seed(c)
	}
	header := make(http.Header)
	if contentType != "" {
		header.Set("Content-Type", contentType)
	}
	response := &http.Response{StatusCode: http.StatusOK, Header: header, Body: io.NopCloser(strings.NewReader(body))}
	return dispatchNonstreamTerminalFixture(c, recorder, response, passthrough)
}

func dispatchNonstreamTerminalFixture(c *gin.Context, recorder *httptest.ResponseRecorder, response *http.Response, passthrough bool) nonstreamTerminalTestResult {
	cfg := &config.Config{}
	svc := &OpenAIGatewayService{cfg: cfg, responseHeaderFilter: compileResponseHeaderFilter(cfg)}
	account := &Account{
		ID:       1,
		Platform: PlatformOpenAI,
		Type:     AccountTypeAPIKey,
		Extra:    map[string]any{"openai_passthrough": passthrough},
	}
	result := nonstreamTerminalTestResult{context: c, recorder: recorder}
	if passthrough {
		parsed, err := svc.handleNonStreamingResponsePassthrough(context.Background(), response, c, account, "gpt-5.4", "gpt-5.4", []byte(nonstreamSyntheticRequest))
		result.err = err
		if parsed != nil {
			result.hasResult = true
			result.usage = parsed.usage
		}
	} else {
		parsed, err := svc.handleNonStreamingResponse(context.Background(), response, c, account, "gpt-5.4", "gpt-5.4", []byte(nonstreamSyntheticRequest))
		result.err = err
		if parsed != nil {
			result.hasResult = true
			result.usage = parsed.usage
		}
	}
	return result
}

func requireNonstreamIncompleteJSONError(t *testing.T, result nonstreamTerminalTestResult) {
	t.Helper()
	require.Error(t, result.err)
	require.False(t, result.hasResult, "an unrecoverable reply must not produce a billable result")
	var failoverErr *UpstreamFailoverError
	require.False(t, errors.As(result.err, &failoverErr), "an incomplete response must not automatically replay generation")
	require.Equal(t, http.StatusBadGateway, result.recorder.Code)
	require.True(t, json.Valid(result.recorder.Body.Bytes()), "non-streaming failure must contain a single JSON value")
	require.Contains(t, result.recorder.Header().Get("Content-Type"), "application/json")
	require.Equal(t, "upstream_incomplete_response", gjson.GetBytes(result.recorder.Body.Bytes(), "error.code").String())
	require.NotEmpty(t, gjson.GetBytes(result.recorder.Body.Bytes(), "error.message").String())
	require.NotContains(t, result.recorder.Body.String(), "event: response.")
	require.NotContains(t, result.recorder.Body.String(), "data: [DONE]")
}

func requireNonstreamRecoveredJSON(t *testing.T, result nonstreamTerminalTestResult) {
	t.Helper()
	require.NoError(t, result.err)
	require.True(t, result.hasResult, "recoverable content must flow through the existing billing pipeline")
	require.NotNil(t, result.usage)
	require.Equal(t, http.StatusOK, result.recorder.Code)
	require.True(t, json.Valid(result.recorder.Body.Bytes()))
	require.Contains(t, result.recorder.Header().Get("Content-Type"), "application/json")
	require.Equal(t, "incomplete", gjson.GetBytes(result.recorder.Body.Bytes(), "status").String())
	require.NotEmpty(t, gjson.GetBytes(result.recorder.Body.Bytes(), "output").Array())
	require.False(t, gjson.GetBytes(result.recorder.Body.Bytes(), "error.code").Exists())
	require.NotContains(t, result.recorder.Body.String(), "usage_source")
	require.NotContains(t, result.recorder.Body.String(), "local_estimate")
	require.NotContains(t, result.recorder.Body.String(), `"estimated"`)
	require.Equal(t, int64(result.usage.InputTokens), gjson.GetBytes(result.recorder.Body.Bytes(), "usage.input_tokens").Int())
	require.Equal(t, int64(result.usage.OutputTokens), gjson.GetBytes(result.recorder.Body.Bytes(), "usage.output_tokens").Int())
}

func TestOpenAINonStreamingMissingTerminalAfterCompactKeepalive(t *testing.T) {
	for _, passthrough := range []bool{false, true} {
		path := "normal"
		if passthrough {
			path = "passthrough"
		}
		for _, recovered := range []bool{false, true} {
			name := "without_output"
			body := ": keepalive\n\n"
			if recovered {
				name = "with_output"
				body = nonstreamMissingTerminalDelta
			}
			t.Run(path+"/"+name, func(t *testing.T) {
				c, recorder := newCompactBridgeTestContext(t, true)
				stop := StartOpenAICompactSSEKeepalive(c, keepaliveTestInterval)
				defer stop()
				waitForKeepaliveBeats()
				require.True(t, StopOpenAICompactSSEKeepaliveCommitted(c))
				response := &http.Response{
					StatusCode: http.StatusOK,
					Header:     http.Header{"Content-Type": []string{"text/event-stream"}},
					Body:       io.NopCloser(strings.NewReader(body)),
				}
				result := dispatchNonstreamTerminalFixture(c, recorder, response, passthrough)
				require.Equal(t, http.StatusOK, recorder.Code, "committed keepalive headers cannot be changed to 502")
				require.Contains(t, recorder.Header().Get("Content-Type"), "text/event-stream")
				require.Contains(t, recorder.Body.String(), ": keepalive\n\n")
				events := parseCompactBridgeSSE(t, stripKeepaliveComments(recorder.Body.String()))
				if recovered {
					require.NoError(t, result.err)
					require.True(t, result.hasResult)
					require.NotNil(t, result.usage)
					require.NotEmpty(t, events)
					require.Equal(t, "response.incomplete", events[len(events)-1][0])
					require.Equal(t, "incomplete", gjson.Get(events[len(events)-1][1], "response.status").String())
					require.Equal(t, "partial", gjson.Get(events[len(events)-1][1], "response.output.0.content.0.text").String())
					require.NotContains(t, recorder.Body.String(), "event: response.completed")
					require.NotContains(t, recorder.Body.String(), "local_estimate")
					return
				}
				require.Error(t, result.err)
				require.False(t, result.hasResult)
				var failoverErr *UpstreamFailoverError
				require.False(t, errors.As(result.err, &failoverErr))
				require.Len(t, events, 1, "append exactly one failed event, without JSON or upstream partial deltas")
				require.Equal(t, "response.failed", events[0][0])
				require.Equal(t, "failed", gjson.Get(events[0][1], "response.status").String())
				require.Equal(t, "upstream_incomplete_response", gjson.Get(events[0][1], "response.error.code").String())
				require.NotContains(t, recorder.Body.String(), `"delta":"partial"`)
				streamErr, ok := GetOpsStreamError(c)
				require.True(t, ok)
				require.Equal(t, http.StatusBadGateway, streamErr.IntendedStatus)
			})
		}
	}
}

type nonstreamUnexpectedEOFReader struct {
	*strings.Reader
}

func (r nonstreamUnexpectedEOFReader) Read(p []byte) (int, error) {
	n, err := r.Reader.Read(p)
	if errors.Is(err, io.EOF) {
		return n, io.ErrUnexpectedEOF
	}
	return n, err
}

func TestOpenAINonStreamingUnexpectedEOFDoesNotReturnSuccess(t *testing.T) {
	for _, passthrough := range []bool{false, true} {
		path := "normal"
		if passthrough {
			path = "passthrough"
		}
		t.Run(path, func(t *testing.T) {
			gin.SetMode(gin.TestMode)
			recorder := httptest.NewRecorder()
			c, _ := gin.CreateTestContext(recorder)
			c.Request = httptest.NewRequest(http.MethodPost, "/v1/responses", nil)
			response := &http.Response{
				StatusCode: http.StatusOK,
				Header:     http.Header{"Content-Type": []string{"text/event-stream"}},
				Body:       io.NopCloser(nonstreamUnexpectedEOFReader{strings.NewReader(nonstreamMissingTerminalDelta)}),
			}
			result := dispatchNonstreamTerminalFixture(c, recorder, response, passthrough)
			require.ErrorIs(t, result.err, io.ErrUnexpectedEOF)
			require.False(t, result.hasResult)
			require.False(t, c.Writer.Written(), "the caller must remain able to report the read error")
			require.Empty(t, recorder.Body.String())
		})
	}
}

func TestOpenAINonStreamingMissingTerminalKeepsObservedUsageInOps(t *testing.T) {
	usageFrame := "event: response.in_progress\n" +
		`data: {"type":"response.in_progress","response":{"id":"resp_synthetic","status":"in_progress","usage":{"input_tokens":17,"output_tokens":3,"input_tokens_details":{"cached_tokens":11}}}}` + "\n\n"
	zeroUsageFrame := "event: response.in_progress\n" +
		`data: {"type":"response.in_progress","response":{"id":"resp_synthetic","status":"in_progress","usage":{"input_tokens":0,"output_tokens":0}}}` + "\n\n"
	emptyUsageFrame := "event: response.in_progress\n" + `data: {"type":"response.in_progress","response":{"usage":{}}}` + "\n\n"
	wrappedUsageFrame := "event: response.in_progress\n" + `data: {"type":"response.in_progress","data":{"usage":{"input_tokens":17,"output_tokens":3,"input_tokens_details":{"cached_tokens":11}}}}` + "\n\n"
	wrappedResponseUsageFrame := "event: response.in_progress\n" + `data: {"type":"response.in_progress","data":{"response":{"usage":{"input_tokens":17,"output_tokens":3,"input_tokens_details":{"cached_tokens":11}}}}}` + "\n\n"
	cases := []struct {
		name         string
		body         string
		observed     bool
		input        int
		output       int
		cached       int
		reason       string
		terminalType string
	}{
		{name: "no_usage", body: nonstreamMissingTerminalDelta, reason: "missing_terminal_response"},
		{name: "empty_usage_is_estimated", body: nonstreamMissingTerminalDelta + emptyUsageFrame, reason: "missing_terminal_response"},
		{name: "observed_usage", body: nonstreamMissingTerminalDelta + usageFrame, observed: true, input: 17, output: 3, cached: 11, reason: "missing_terminal_response"},
		{name: "wrapped_usage", body: nonstreamMissingTerminalDelta + wrappedUsageFrame, observed: true, input: 17, output: 3, cached: 11, reason: "missing_terminal_response"},
		{name: "wrapped_response_usage", body: nonstreamMissingTerminalDelta + wrappedResponseUsageFrame, observed: true, input: 17, output: 3, cached: 11, reason: "missing_terminal_response"},
		{name: "observed_zero_usage", body: nonstreamMissingTerminalDelta + zeroUsageFrame, observed: true, reason: "missing_terminal_response"},
		{name: "invalid_terminal_with_prior_usage", body: nonstreamMissingTerminalDelta + usageFrame + "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":null}\n\n", observed: true, input: 17, output: 3, cached: 11, reason: "invalid_terminal_response", terminalType: "response.completed"},
	}
	for _, passthrough := range []bool{false, true} {
		path := "normal"
		if passthrough {
			path = "passthrough"
		}
		for _, tc := range cases {
			t.Run(path+"/"+tc.name, func(t *testing.T) {
				result := replayNonstreamTerminalFixture(t, passthrough, "text/event-stream", tc.body, func(c *gin.Context) {
					setOpsUpstreamError(c, http.StatusTooManyRequests, "prior attempt", `{"reason":"prior_attempt"}`)
					appendOpsUpstreamError(c, OpsUpstreamErrorEvent{AccountID: 2, UpstreamStatusCode: http.StatusTooManyRequests, Reason: "prior_attempt", Detail: `{"reason":"prior_attempt"}`})
				})
				requireNonstreamRecoveredJSON(t, result)
				value, ok := result.context.Get(OpsUpstreamErrorDetailKey)
				require.True(t, ok)
				detail, ok := value.(string)
				require.True(t, ok)
				require.True(t, json.Valid([]byte(detail)))
				require.Equal(t, tc.reason, gjson.Get(detail, "reason").String())
				require.Equal(t, tc.terminalType, gjson.Get(detail, "terminal_type").String())
				require.True(t, gjson.Get(detail, "usage_observed").Exists(), "zero usage and absent usage must remain distinguishable")
				require.Equal(t, tc.observed, gjson.Get(detail, "usage_observed").Bool())
				require.True(t, gjson.Get(detail, "output_recovered").Bool())
				require.True(t, gjson.Get(detail, "billed_usage").IsObject())
				require.Equal(t, int64(result.usage.InputTokens), gjson.Get(detail, "billed_usage.input_tokens").Int())
				require.Equal(t, int64(result.usage.OutputTokens), gjson.Get(detail, "billed_usage.output_tokens").Int())
				if tc.observed {
					require.Equal(t, "upstream", gjson.Get(detail, "usage_source").String())
					require.Equal(t, tc.input, result.usage.InputTokens)
					require.Equal(t, tc.output, result.usage.OutputTokens)
					require.Equal(t, tc.cached, result.usage.CacheReadInputTokens)
					require.True(t, gjson.Get(detail, "observed_usage").IsObject())
					require.Equal(t, int64(tc.input), gjson.Get(detail, "observed_usage.input_tokens").Int())
					require.Equal(t, int64(tc.output), gjson.Get(detail, "observed_usage.output_tokens").Int())
					require.Equal(t, int64(tc.cached), gjson.Get(detail, "observed_usage.cache_read_input_tokens").Int())
				} else {
					require.Equal(t, "local_estimate", gjson.Get(detail, "usage_source").String())
					require.Positive(t, result.usage.InputTokens)
					require.Positive(t, result.usage.OutputTokens)
					require.Zero(t, result.usage.CacheReadInputTokens, "missing upstream usage cannot establish a cache hit")
					require.False(t, gjson.Get(detail, "observed_usage").Exists())
				}
				require.NotContains(t, detail, "synthetic probe")
				require.NotContains(t, detail, `"delta"`)
				value, ok = result.context.Get(OpsUpstreamErrorsKey)
				require.True(t, ok)
				events, ok := value.([]*OpsUpstreamErrorEvent)
				require.True(t, ok)
				require.Len(t, events, 2)
				require.Equal(t, "prior_attempt", events[0].Reason)
				require.Equal(t, `{"reason":"prior_attempt"}`, events[0].Detail)
				require.Equal(t, int64(1), events[1].AccountID)
				require.Equal(t, http.StatusOK, events[1].UpstreamStatusCode)
				require.Equal(t, passthrough, events[1].Passthrough)
				require.Equal(t, "upstream_incomplete_response", events[1].Reason)
				require.JSONEq(t, detail, events[1].Detail)
			})
		}
	}
}

func TestOpenAINonStreamingMissingTerminalReturnsJSONError(t *testing.T) {
	cases := []struct {
		name string
		body string
	}{
		{name: "done_sentinel_only", body: "data: [DONE]\n\n"},
		{name: "created_only", body: "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_synthetic\",\"output\":[]}}\n\n"},
		{name: "heartbeat_only", body: ": keepalive\n\n: ping\n\n"},
		{name: "empty_body", body: ""},
		{name: "completed_missing_response", body: "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n"},
		{name: "completed_null_response", body: "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":null}\n\n"},
		{name: "completed_array_response", body: "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":[]}\n\n"},
		{name: "completed_string_response", body: "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":\"invalid\"}\n\n"},
		{name: "malformed_completed_payload", body: "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\n\n"},
	}
	for _, passthrough := range []bool{false, true} {
		path := "normal"
		if passthrough {
			path = "passthrough"
		}
		for _, tc := range cases {
			t.Run(path+"/"+tc.name, func(t *testing.T) {
				requireNonstreamIncompleteJSONError(t, replayNonstreamTerminalFixture(t, passthrough, "text/event-stream", tc.body))
			})
		}
	}
}

func TestOpenAINonStreamingSSEMissingTerminalWithWrongContentType(t *testing.T) {
	for _, passthrough := range []bool{false, true} {
		path := "normal"
		if passthrough {
			path = "passthrough"
		}
		for _, contentType := range []string{"", "application/json", "text/plain"} {
			t.Run(path+"/"+contentType, func(t *testing.T) {
				result := replayNonstreamTerminalFixture(t, passthrough, contentType, nonstreamMissingTerminalDelta)
				requireNonstreamRecoveredJSON(t, result)
				require.Equal(t, "partial", gjson.GetBytes(result.recorder.Body.Bytes(), "output.0.content.0.text").String())
			})
		}
	}
}

func TestOpenAINonStreamingMissingTerminalRecoversText(t *testing.T) {
	textDone := "event: response.output_text.done\n" + `data: {"type":"response.output_text.done","output_index":0,"content_index":0,"text":"partial"}` + "\n\n"
	itemDone := "event: response.output_item.done\n" + `data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg_synthetic","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"finished text","annotations":[]}]}}` + "\n\n"
	invalidTerminal := "event: response.completed\n" + `data: {"type":"response.completed","response":null}` + "\n\n"
	for _, passthrough := range []bool{false, true} {
		path := "normal"
		if passthrough {
			path = "passthrough"
		}
		for _, tc := range []struct{ name, body, text string }{
			{"delta_only", nonstreamMissingTerminalDelta, "partial"},
			{"delta_then_done_sentinel", nonstreamMissingTerminalDelta + "data: [DONE]\n\n", "partial"},
			{"text_done_only", textDone, "partial"},
			{"delta_then_text_done", nonstreamMissingTerminalDelta + textDone, "partial"},
			{"completed_message", itemDone, "finished text"},
			{"invalid_terminal_after_message", itemDone + invalidTerminal, "finished text"},
		} {
			t.Run(path+"/"+tc.name, func(t *testing.T) {
				result := replayNonstreamTerminalFixture(t, passthrough, "text/event-stream", tc.body)
				requireNonstreamRecoveredJSON(t, result)
				require.Len(t, gjson.GetBytes(result.recorder.Body.Bytes(), "output").Array(), 1)
				require.Equal(t, tc.text, gjson.GetBytes(result.recorder.Body.Bytes(), "output.0.content.0.text").String())
				require.Positive(t, result.usage.InputTokens)
				require.Positive(t, result.usage.OutputTokens)
				require.Zero(t, result.usage.CacheReadInputTokens)
			})
		}
	}
}

func TestOpenAINonStreamingMissingTerminalPreservesCompletedToolsOnly(t *testing.T) {
	toolAdded := "event: response.output_item.added\n" + `data: {"type":"response.output_item.added","output_index":1,"item":{"id":"fc_synthetic","type":"function_call","status":"in_progress","call_id":"call_synthetic","name":"emit_probe","arguments":""}}` + "\n\n"
	partialArguments := "event: response.function_call_arguments.delta\n" + `data: {"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_synthetic","delta":"{\"value\":"}` + "\n\n"
	toolDone := "event: response.output_item.done\n" + `data: {"type":"response.output_item.done","output_index":1,"item":{"id":"fc_synthetic","type":"function_call","status":"completed","call_id":"call_synthetic","name":"emit_probe","arguments":"{\"value\":\"OK\"}"}}` + "\n\n"
	for _, passthrough := range []bool{false, true} {
		path := "normal"
		if passthrough {
			path = "passthrough"
		}
		for _, tc := range []struct {
			name, body         string
			wantText, wantTool bool
		}{
			{"partial_tool_only", toolAdded + partialArguments, false, false},
			{"partial_tool_with_text", nonstreamMissingTerminalDelta + toolAdded + partialArguments, true, false},
			{"completed_tool", toolAdded + partialArguments + toolDone, false, true},
			{"completed_tool_with_text", nonstreamMissingTerminalDelta + toolDone, true, true},
		} {
			t.Run(path+"/"+tc.name, func(t *testing.T) {
				result := replayNonstreamTerminalFixture(t, passthrough, "text/event-stream", tc.body)
				if !tc.wantText && !tc.wantTool {
					requireNonstreamIncompleteJSONError(t, result)
					return
				}
				requireNonstreamRecoveredJSON(t, result)
				var textCount, toolCount int
				for _, item := range gjson.GetBytes(result.recorder.Body.Bytes(), "output").Array() {
					switch item.Get("type").String() {
					case "message":
						textCount++
						require.Equal(t, "partial", item.Get("content.0.text").String())
					case "function_call":
						toolCount++
						require.Equal(t, "fc_synthetic", item.Get("id").String())
						require.Equal(t, "call_synthetic", item.Get("call_id").String())
						require.Equal(t, "emit_probe", item.Get("name").String())
						require.JSONEq(t, `{"value":"OK"}`, item.Get("arguments").String())
					}
				}
				require.Equal(t, tc.wantText, textCount == 1)
				require.Equal(t, tc.wantTool, toolCount == 1)
				if !tc.wantTool {
					require.Zero(t, toolCount, "partial tool arguments must not become executable calls")
				}
			})
		}
	}
}

func TestOpenAINonStreamingMissingTerminalWithoutContentDoesNotBillObservedUsage(t *testing.T) {
	body := "event: response.in_progress\n" + `data: {"type":"response.in_progress","response":{"id":"resp_synthetic","status":"in_progress","usage":{"input_tokens":17,"output_tokens":3}}}` + "\n\n"
	for _, passthrough := range []bool{false, true} {
		result := replayNonstreamTerminalFixture(t, passthrough, "text/event-stream", body)
		requireNonstreamIncompleteJSONError(t, result)
		value, ok := result.context.Get(OpsUpstreamErrorDetailKey)
		require.True(t, ok)
		detail, ok := value.(string)
		require.True(t, ok)
		require.False(t, gjson.Get(detail, "output_recovered").Bool())
		require.True(t, gjson.Get(detail, "usage_observed").Bool())
		require.Equal(t, int64(17), gjson.Get(detail, "observed_usage.input_tokens").Int())
		require.False(t, gjson.Get(detail, "billed_usage").Exists())
	}
}

func TestOpenAINonStreamingValidTerminalRemainsJSON(t *testing.T) {
	cases := []struct {
		event  string
		status string
	}{
		{event: "response.completed", status: "completed"},
		{event: "response.done", status: "completed"},
		{event: "response.incomplete", status: "incomplete"},
		{event: "response.cancelled", status: "cancelled"},
		{event: "response.canceled", status: "canceled"},
	}
	for _, passthrough := range []bool{false, true} {
		path := "normal"
		if passthrough {
			path = "passthrough"
		}
		for _, tc := range cases {
			for _, contentType := range []string{"text/event-stream", "application/json", ""} {
				t.Run(path+"/"+tc.event+"/"+contentType, func(t *testing.T) {
					response := map[string]any{
						"id": "resp_synthetic", "object": "response", "status": tc.status, "model": "gpt-5.4",
						"output": []any{map[string]any{"type": "message", "role": "assistant", "content": []any{map[string]any{"type": "output_text", "text": "kept"}}}},
						"usage":  map[string]any{"input_tokens": 17, "output_tokens": 3, "input_tokens_details": map[string]any{"cached_tokens": 11}},
					}
					if tc.status == "incomplete" {
						response["incomplete_details"] = map[string]any{"reason": "max_output_tokens"}
					}
					payload, err := json.Marshal(map[string]any{"type": tc.event, "response": response})
					require.NoError(t, err)
					body := "event: " + tc.event + "\ndata: " + string(payload) + "\n\ndata: [DONE]\n\n"
					result := replayNonstreamTerminalFixture(t, passthrough, contentType, body)
					require.NoError(t, result.err)
					require.Equal(t, http.StatusOK, result.recorder.Code)
					require.True(t, json.Valid(result.recorder.Body.Bytes()))
					require.Contains(t, result.recorder.Header().Get("Content-Type"), "application/json")
					require.Equal(t, tc.status, gjson.GetBytes(result.recorder.Body.Bytes(), "status").String())
					require.Equal(t, "kept", gjson.GetBytes(result.recorder.Body.Bytes(), "output.0.content.0.text").String())
					if tc.status == "incomplete" {
						require.Equal(t, "max_output_tokens", gjson.GetBytes(result.recorder.Body.Bytes(), "incomplete_details.reason").String())
					}
					require.NotNil(t, result.usage)
					require.Equal(t, 17, result.usage.InputTokens)
					require.Equal(t, 3, result.usage.OutputTokens)
					require.Equal(t, 11, result.usage.CacheReadInputTokens)
				})
			}
		}
	}
}

func TestOpenAINonStreamingJSONWithSSEWordsIsNotMisclassified(t *testing.T) {
	body := `{"id":"resp_synthetic","object":"response","status":"completed","model":"gpt-5.4","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Literal data: and event: are regular text.\ndata: inside an escaped JSON string is also text."}]}],"usage":{"input_tokens":17,"output_tokens":3}}`
	for _, passthrough := range []bool{false, true} {
		result := replayNonstreamTerminalFixture(t, passthrough, "application/json", body)
		require.NoError(t, result.err)
		require.Equal(t, http.StatusOK, result.recorder.Code)
		require.True(t, json.Valid(result.recorder.Body.Bytes()))
		require.Equal(t, gjson.Get(body, "output").Raw, gjson.GetBytes(result.recorder.Body.Bytes(), "output").Raw)
		require.NotNil(t, result.usage)
		require.Equal(t, 17, result.usage.InputTokens)
		require.Equal(t, 3, result.usage.OutputTokens)
	}
}
