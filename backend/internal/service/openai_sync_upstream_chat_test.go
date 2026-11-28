//go:build unit

package service

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

func TestSyncUpstreamChatForwardingPreservesClientJSON(t *testing.T) {
	gin.SetMode(gin.TestMode)
	for _, responsesClient := range []bool{false, true} {
		for _, enabled := range []bool{false, true} {
			t.Run(fmt.Sprintf("responses=%v/enabled=%v", responsesClient, enabled), func(t *testing.T) {
				body := []byte(`{"model":"gpt-5.4","messages":[{"role":"user","content":"Hello"}],"stream":false}`)
				path := "/v1/chat/completions"
				if responsesClient {
					path = "/v1/responses"
					body = []byte(`{"model":"gpt-5.4","input":"Hello","stream":false}`)
				}
				rec := httptest.NewRecorder()
				c, _ := gin.CreateTestContext(rec)
				c.Request = httptest.NewRequest(http.MethodPost, path, bytes.NewReader(body))
				SetOpenAIClientTransport(c, OpenAIClientTransportHTTP)
				upstreamBody := `{"id":"c1","object":"chat.completion","model":"gpt-5.4","choices":[{"index":0,"message":{"role":"assistant","content":"hello world"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":8}}}`
				contentType := "application/json"
				if enabled {
					upstreamBody = "data: " + `{"id":"c1","model":"gpt-5.4","choices":[{"index":0,"delta":{"content":"hello "}}]}` + "\n\ndata: " + `{"choices":[{"index":0,"delta":{"content":"world"},"finish_reason":"stop"}]}` + "\n\ndata: " + `{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":8}}}` + "\n\ndata: [DONE]\n\n"
					contentType = "text/event-stream"
				}
				upstream := &httpUpstreamRecorder{resp: &http.Response{StatusCode: http.StatusOK, Header: http.Header{"Content-Type": {contentType}}, Body: io.NopCloser(strings.NewReader(upstreamBody))}}
				svc := &OpenAIGatewayService{cfg: rawChatCompletionsTestConfig(), httpUpstream: upstream}
				account := rawChatCompletionsTestAccount()
				account.Extra = map[string]any{"openai_sync_upstream_stream": enabled}
				var result *OpenAIForwardResult
				var err error
				if responsesClient {
					result, err = svc.forwardResponsesViaRawChatCompletions(context.Background(), c, account, body, time.Now())
				} else {
					result, err = svc.forwardAsRawChatCompletions(context.Background(), c, account, body, "")
				}
				require.NoError(t, err)
				require.NotNil(t, result)
				require.False(t, result.Stream)
				require.Equal(t, enabled, gjson.GetBytes(upstream.lastBody, "stream").Bool())
				require.Equal(t, enabled, gjson.GetBytes(upstream.lastBody, "stream_options.include_usage").Bool())
				require.Equal(t, 12, result.Usage.InputTokens)
				require.Equal(t, 3, result.Usage.OutputTokens)
				require.Equal(t, 8, result.Usage.CacheReadInputTokens)
				require.Equal(t, http.StatusOK, rec.Code)
				require.Contains(t, rec.Header().Get("Content-Type"), "application/json")
				require.True(t, gjson.Valid(rec.Body.String()))
				if responsesClient {
					require.Equal(t, "hello world", gjson.Get(rec.Body.String(), "output.0.content.0.text").String())
				} else {
					require.Equal(t, "hello world", gjson.Get(rec.Body.String(), "choices.0.message.content").String())
				}
			})
		}
	}
}

func TestSyncUpstreamChatMissingTerminalAndUsage(t *testing.T) {
	req := []byte(`{"model":"gpt-5.4","messages":[{"role":"user","content":"Tell me a short story."}],"stream":true}`)
	for _, terminal := range []bool{false, true} {
		for _, explicitZero := range []bool{false, true} {
			t.Run(fmt.Sprintf("terminal=%v/zero=%v", terminal, explicitZero), func(t *testing.T) {
				stream := "data: " + `{"id":"c1","choices":[{"index":0,"delta":{"content":"A small fox found its way home."}}]}` + "\n\n"
				if terminal {
					stream += "data: " + `{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}` + "\n\n"
				}
				if explicitZero {
					stream += "data: " + `{"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}` + "\n\n"
				} else {
					stream += "data: " + `{"usage":{}}` + "\n\n"
				}
				body, incomplete, observed, err := aggregateSyncUpstreamChat([]byte(stream), req, "gpt-5.4")
				require.NoError(t, err)
				require.Equal(t, !terminal, incomplete)
				require.Equal(t, explicitZero, observed)
				require.Equal(t, "A small fox found its way home.", gjson.GetBytes(body, "choices.0.message.content").String())
				if terminal {
					require.Equal(t, "stop", gjson.GetBytes(body, "choices.0.finish_reason").String())
				} else {
					require.Equal(t, "length", gjson.GetBytes(body, "choices.0.finish_reason").String())
				}
				if explicitZero {
					require.Zero(t, gjson.GetBytes(body, "usage.total_tokens").Int())
				} else {
					require.Positive(t, gjson.GetBytes(body, "usage.prompt_tokens").Int())
					require.Positive(t, gjson.GetBytes(body, "usage.completion_tokens").Int())
				}
				require.NotContains(t, string(body), "estimate")
				require.Zero(t, gjson.GetBytes(body, "usage.prompt_tokens_details.cached_tokens").Int())
			})
		}
	}
}

func TestSyncUpstreamChatIndexedToolsAndPartialSafety(t *testing.T) {
	req := []byte(`{"model":"gpt-5.4","messages":[{"role":"user","content":"Use tools"}]}`)
	stream := "data: " + `{"id":"c1","choices":[{"index":1,"delta":{"content":"second","reasoning_content":"think"}},{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"second","arguments":"{\"b\":"}},{"index":0,"id":"call_a","type":"function","function":{"name":"first","arguments":"{\"a\":"}}]}}]}` + "\n\ndata: " + `{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}},{"index":1,"function":{"arguments":"2}"}}]},"finish_reason":"tool_calls"},{"index":1,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":14,"completion_tokens":8}}` + "\n\n"
	body, incomplete, observed, err := aggregateSyncUpstreamChat([]byte(stream), req, "gpt-5.4")
	require.NoError(t, err)
	require.False(t, incomplete)
	require.True(t, observed)
	require.Equal(t, "first", gjson.GetBytes(body, "choices.0.message.tool_calls.0.function.name").String())
	require.Equal(t, `{"a":1}`, gjson.GetBytes(body, "choices.0.message.tool_calls.0.function.arguments").String())
	require.Equal(t, "second", gjson.GetBytes(body, "choices.0.message.tool_calls.1.function.name").String())
	require.Equal(t, "think", gjson.GetBytes(body, "choices.1.message.reasoning_content").String())
	require.False(t, gjson.GetBytes(body, "choices.0.message.tool_calls.0.index").Exists())
	partial := "data: " + `{"choices":[{"index":0,"delta":{"content":"I started a tool.","tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"unsafe","arguments":"{\"a\":"}}]}}]}` + "\n\n"
	body, incomplete, _, err = aggregateSyncUpstreamChat([]byte(partial), req, "gpt-5.4")
	require.NoError(t, err)
	require.True(t, incomplete)
	require.False(t, gjson.GetBytes(body, "choices.0.message.tool_calls").Exists())
	require.Equal(t, "length", gjson.GetBytes(body, "choices.0.finish_reason").String())
}

func TestSyncUpstreamChatRejectsEmptyAndErrors(t *testing.T) {
	for _, body := range []string{"data: [DONE]\n\n", "data: {}\n\n", "data: {bad}\n\n", "data: {\"error\":{\"message\":\"failed\"}}\n\n", `{"error":{"message":"failed"}}`, `{"choices":[]}`, "data: {\"choices\":[{\"index\":-1,\"delta\":{\"content\":\"bad\"}}]}\n\n"} {
		_, _, _, err := aggregateSyncUpstreamChat([]byte(body), []byte(`{"model":"gpt-5.4","messages":[]}`), "gpt-5.4")
		require.Error(t, err, body)
	}
}

func TestSyncUpstreamChatResponsesBridgeRecovery(t *testing.T) {
	gin.SetMode(gin.TestMode)
	for _, terminal := range []bool{false, true} {
		t.Run(fmt.Sprintf("terminal=%v", terminal), func(t *testing.T) {
			stream := "event: response.output_text.delta\ndata: " + `{"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"m1","delta":"A useful recovered answer."}` + "\n\n"
			if terminal {
				stream += "event: response.completed\ndata: " + `{"type":"response.completed","response":{"id":"r1","object":"response","model":"gpt-5.4","status":"completed","output":[{"type":"message","id":"m1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"A useful recovered answer."}]}]}}` + "\n\n"
			}
			rec := httptest.NewRecorder()
			c, _ := gin.CreateTestContext(rec)
			c.Request = httptest.NewRequest(http.MethodPost, "/v1/chat/completions", nil)
			SetOpenAIClientTransport(c, OpenAIClientTransportHTTP)
			account := rawChatCompletionsTestAccount()
			account.Extra = map[string]any{"openai_sync_upstream_stream": true}
			resp := &http.Response{StatusCode: http.StatusOK, Header: http.Header{"Content-Type": {"text/event-stream"}}, Body: io.NopCloser(strings.NewReader(stream))}
			result, err := (&OpenAIGatewayService{}).handleChatBufferedStreamingResponseWithRequest(resp, c, account, "gpt-5.4", "gpt-5.4", "gpt-5.4", time.Now(), []byte(`{"model":"gpt-5.4","input":"Tell me a story","stream":true}`))
			require.NoError(t, err)
			require.NotNil(t, result)
			require.False(t, result.Stream)
			require.Positive(t, result.Usage.InputTokens)
			require.Positive(t, result.Usage.OutputTokens)
			require.Equal(t, "A useful recovered answer.", gjson.Get(rec.Body.String(), "choices.0.message.content").String())
			require.NotContains(t, rec.Body.String(), "estimate")
			if terminal {
				require.Equal(t, "stop", gjson.Get(rec.Body.String(), "choices.0.finish_reason").String())
			} else {
				require.Equal(t, "length", gjson.Get(rec.Body.String(), "choices.0.finish_reason").String())
			}
		})
	}
}

func TestSyncUpstreamChatResponsesBridgeRetainsReportedUsage(t *testing.T) {
	for _, finalUsage := range []string{`{}`, `{"input_tokens":0,"output_tokens":0,"total_tokens":0}`} {
		stream := "event: response.created\ndata: " + `{"type":"response.created","response":{"id":"r1","usage":{"input_tokens":12,"output_tokens":7,"total_tokens":19,"input_tokens_details":{"cached_tokens":5}}}}` + "\n\nevent: response.completed\ndata: " + `{"type":"response.completed","response":{"id":"r1","model":"gpt-5.4","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Useful answer"}]}],"usage":` + finalUsage + "}}\n\n"
		rec := httptest.NewRecorder()
		c, _ := gin.CreateTestContext(rec)
		c.Request = httptest.NewRequest(http.MethodPost, "/v1/chat/completions", nil)
		SetOpenAIClientTransport(c, OpenAIClientTransportHTTP)
		account := rawChatCompletionsTestAccount()
		account.Extra = map[string]any{"openai_sync_upstream_stream": true}
		resp := &http.Response{StatusCode: http.StatusOK, Header: http.Header{"Content-Type": {"text/event-stream"}}, Body: io.NopCloser(strings.NewReader(stream))}
		result, err := (&OpenAIGatewayService{}).handleChatBufferedStreamingResponseWithRequest(resp, c, account, "gpt-5.4", "gpt-5.4", "gpt-5.4", time.Now(), []byte(`{"model":"gpt-5.4","input":"Hi"}`))
		require.NoError(t, err)
		require.NotNil(t, result)
		if finalUsage == `{}` {
			require.Equal(t, 12, result.Usage.InputTokens)
			require.Equal(t, 7, result.Usage.OutputTokens)
			require.Equal(t, 5, result.Usage.CacheReadInputTokens)
			require.EqualValues(t, 12, gjson.Get(rec.Body.String(), "usage.prompt_tokens").Int())
		} else {
			require.Zero(t, result.Usage.InputTokens)
			require.Zero(t, result.Usage.OutputTokens)
			require.Zero(t, result.Usage.CacheReadInputTokens)
		}
	}
}

func TestSyncUpstreamChatEmptyDoesNotReplay(t *testing.T) {
	body := []byte(`{"model":"gpt-5.4","messages":[{"role":"user","content":"Hi"}],"stream":false}`)
	rec := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(rec)
	c.Request = httptest.NewRequest(http.MethodPost, "/v1/chat/completions", bytes.NewReader(body))
	SetOpenAIClientTransport(c, OpenAIClientTransportHTTP)
	upstream := &httpUpstreamRecorder{resp: &http.Response{StatusCode: http.StatusOK, Header: http.Header{"Content-Type": {"text/event-stream"}}, Body: io.NopCloser(strings.NewReader("data: [DONE]\n\n"))}}
	account := rawChatCompletionsTestAccount()
	account.Extra = map[string]any{"openai_sync_upstream_stream": true}
	svc := &OpenAIGatewayService{cfg: rawChatCompletionsTestConfig(), httpUpstream: upstream}
	result, err := svc.forwardAsRawChatCompletions(context.Background(), c, account, body, "")
	require.Error(t, err)
	require.Nil(t, result)
	var failover *UpstreamFailoverError
	require.NotErrorAs(t, err, &failover)
	require.Len(t, upstream.requests, 1)
	require.Equal(t, http.StatusBadGateway, rec.Code)
	require.Equal(t, openAINonStreamingIncompleteCode, gjson.Get(rec.Body.String(), "error.code").String())
	require.NotContains(t, rec.Body.String(), "data:")
}

func TestSyncUpstreamChatRespectsBodyLimit(t *testing.T) {
	rec := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(rec)
	c.Request = httptest.NewRequest(http.MethodPost, "/v1/chat/completions", nil)
	response := &http.Response{StatusCode: http.StatusOK, Header: http.Header{}, Body: io.NopCloser(strings.NewReader(strings.Repeat("x", 65)))}
	cfg := rawChatCompletionsTestConfig()
	cfg.Gateway.UpstreamResponseReadMaxBytes = 64
	err := (&OpenAIGatewayService{cfg: cfg}).bufferSyncUpstreamChatJSON(c, response, rawChatCompletionsTestAccount(), []byte(`{}`), "gpt-5.4")
	require.ErrorIs(t, err, ErrUpstreamResponseBodyTooLarge)
	require.Equal(t, http.StatusBadGateway, rec.Code)
}
