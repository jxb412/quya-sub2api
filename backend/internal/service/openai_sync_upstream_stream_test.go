package service

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"testing"

	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

// 2026-11-28: verify the actual outbound payload, downstream format and billable
// usage together; checking only the account flag would miss the original bug.
func TestOpenAISyncUpstreamStreamResponses(t *testing.T) {
	for _, passthrough := range []bool{false, true} {
		for _, enabled := range []bool{false, true} {
			for _, kind := range []string{"complete", "no_usage", "empty_final_usage", "explicit_zero", "missing_terminal", "no_output"} {
				t.Run(fmt.Sprintf("passthrough_%t/enabled_%t/%s", passthrough, enabled, kind), func(t *testing.T) {
					svc, account, upstream := newHTTPWSPolicyTestService(t, "https://upstream.example")
					account.Extra["openai_passthrough"] = passthrough
					account.Extra["openai_sync_upstream_stream"] = enabled
					if !enabled {
						account.Extra["openai_apikey_responses_websockets_v2_mode"] = "off"
					}
					final := httpWSPolicyResponse
					if kind == "no_usage" {
						final, _ = sjson.Delete(final, "usage")
					}
					responseBody := "data: {\"type\":\"response.completed\",\"response\":" + final + "}\n\ndata: [DONE]\n\n"
					if kind == "empty_final_usage" || kind == "explicit_zero" {
						final, _ = sjson.SetRaw(final, "usage", `{}`)
						if kind == "explicit_zero" {
							final, _ = sjson.SetRaw(final, "usage", `{"input_tokens":0,"output_tokens":0}`)
						}
						responseBody = "data: {\"type\":\"response.usage\",\"usage\":{\"input_tokens\":12,\"output_tokens\":7}}\n\ndata: {\"type\":\"response.completed\",\"response\":" + final + "}\n\n"
					}
					switch kind {
					case "missing_terminal":
						responseBody = nonstreamMissingTerminalDelta
					case "no_output":
						responseBody = ": keepalive\n\n"
					}
					upstream.resp = &http.Response{StatusCode: 200, Header: http.Header{"Content-Type": {"text/event-stream"}}, Body: io.NopCloser(strings.NewReader(responseBody))}
					c, rec := newHTTPWSPolicyTestContext("/v1/responses")
					body := []byte(`{"model":"gpt-6.1-sol","stream":false,"input":"hello"}`)
					result, err := svc.Forward(context.Background(), c, account, body)
					require.Len(t, upstream.requests, 1, "no replay after receiving generated content")
					require.Equal(t, enabled, gjson.GetBytes(upstream.lastBody, "stream").Bool())
					require.False(t, gjson.GetBytes(body, "stream").Bool(), "do not mutate the original client request")
					if enabled {
						require.Equal(t, "text/event-stream", upstream.requests[0].Header.Get("Accept"))
					}
					require.True(t, json.Valid(rec.Body.Bytes()))
					require.Contains(t, rec.Header().Get("Content-Type"), "application/json")
					if kind == "no_output" {
						require.Error(t, err)
						require.Nil(t, result)
						require.Equal(t, 502, rec.Code)
						return
					}
					require.NoError(t, err)
					require.NotNil(t, result)
					require.False(t, result.Stream)
					require.False(t, result.OpenAIWSMode, "explicit HTTP SSE overrides ctx_pool for sync clients")
					require.Equal(t, 200, rec.Code)
					if kind == "complete" || kind == "empty_final_usage" {
						require.Equal(t, 12, result.Usage.InputTokens)
						require.Equal(t, 7, result.Usage.OutputTokens)
						if enabled {
							require.EqualValues(t, 12, gjson.GetBytes(rec.Body.Bytes(), "usage.input_tokens").Int())
							require.EqualValues(t, 7, gjson.GetBytes(rec.Body.Bytes(), "usage.output_tokens").Int())
						}
					} else if kind == "explicit_zero" {
						require.Zero(t, result.Usage.InputTokens)
						require.Zero(t, result.Usage.OutputTokens)
					} else if enabled || kind == "missing_terminal" {
						require.Positive(t, result.Usage.InputTokens)
						require.Positive(t, result.Usage.OutputTokens)
					}
					if kind == "missing_terminal" {
						require.Equal(t, "incomplete", gjson.GetBytes(rec.Body.Bytes(), "status").String())
					} else {
						require.Equal(t, "completed", gjson.GetBytes(rec.Body.Bytes(), "status").String())
					}
					require.NotContains(t, rec.Body.String(), "local_estimate")
				})
			}
		}
	}
}

func TestOpenAISyncUpstreamStreamExclusions(t *testing.T) {
	for _, mode := range []string{"oauth", "compact", "background", "client_stream", "native_ws", "disabled_sync"} {
		t.Run(mode, func(t *testing.T) {
			_, account, _ := newHTTPWSPolicyTestService(t, "https://upstream.example")
			account.Extra["openai_sync_upstream_stream"] = true
			path, body := "/v1/responses", []byte(`{"model":"gpt-6.1-sol","stream":false,"input":"hello"}`)
			if mode == "compact" {
				path += "/compact"
			}
			c, _ := newHTTPWSPolicyTestContext(path)
			switch mode {
			case "oauth":
				account.Type = AccountTypeOAuth
			case "background":
				body, _ = sjson.SetBytes(body, "background", true)
			case "client_stream":
				body, _ = sjson.SetBytes(body, "stream", true)
			case "native_ws":
				SetOpenAIClientTransport(c, OpenAIClientTransportWS)
			case "disabled_sync":
				account.Extra["openai_disable_sync"] = true
				c.Request = c.Request.WithContext(WithOpenAIInboundRequestPolicy(c.Request.Context(), OpenAIInboundProtocolResponses, nil, RequestTypeSync))
				require.NotNil(t, openAIInboundProtocolFailover(c, account))
				return
			}
			patched, changed, err := prepareOpenAISyncUpstreamStream(c, account, body, false)
			require.NoError(t, err)
			require.False(t, changed)
			require.Equal(t, body, patched)
		})
	}
}
