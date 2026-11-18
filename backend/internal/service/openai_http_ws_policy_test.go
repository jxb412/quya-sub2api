package service

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/gin-gonic/gin"
	"github.com/gorilla/websocket"
	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

const httpWSPolicyResponse = `{"id":"resp_http_ws_policy","object":"response","status":"completed","model":"gpt-6.1-sol","output":[{"id":"msg_policy","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello from pool","annotations":[]}]}],"usage":{"input_tokens":12,"output_tokens":7,"total_tokens":19}}`

type httpWSPolicyURLDialer struct {
	inner *httpWSTestDialer
	url   atomic.Value
}

func (d *httpWSPolicyURLDialer) Dial(ctx context.Context, url string, headers http.Header, proxy string) (openAIWSClientConn, int, http.Header, error) {
	d.url.Store(url)
	return d.inner.Dial(ctx, url, headers, proxy)
}

func newHTTPWSPolicyTestService(t *testing.T, baseURL string) (*OpenAIGatewayService, *Account, *httpUpstreamRecorder) {
	t.Helper()
	gin.SetMode(gin.TestMode)
	httpRecorder := &httpUpstreamRecorder{resp: &http.Response{
		StatusCode: http.StatusOK,
		Header:     http.Header{"Content-Type": {"application/json"}},
		Body:       io.NopCloser(strings.NewReader(httpWSPolicyResponse)),
	}}
	svc := newOpenAIImageGenerationControlTestService(httpRecorder)
	cfg := svc.cfg
	cfg.Security.URLAllowlist.Enabled = false
	cfg.Security.URLAllowlist.AllowInsecureHTTP = true
	cfg.Gateway.OpenAIWS.Enabled = true
	cfg.Gateway.OpenAIWS.OAuthEnabled = true
	cfg.Gateway.OpenAIWS.APIKeyEnabled = true
	cfg.Gateway.OpenAIWS.ModeRouterV2Enabled = true
	cfg.Gateway.OpenAIWS.ResponsesWebsocketsV2 = true
	cfg.Gateway.OpenAIWS.MaxConnsPerAccount = 2
	cfg.Gateway.OpenAIWS.MaxIdlePerAccount = 2
	cfg.Gateway.OpenAIWS.QueueLimitPerConn = 2
	cfg.Gateway.OpenAIWS.ReadTimeoutSeconds = 5
	cfg.Gateway.OpenAIWS.WriteTimeoutSeconds = 5
	cfg.Gateway.OpenAIWS.StickyResponseIDTTLSeconds = 60
	account := newOpenAIImageGenerationControlTestAccount()
	account.Concurrency = 2
	account.Credentials["base_url"] = baseURL
	account.Extra = map[string]any{
		"openai_apikey_responses_websockets_v2_mode": OpenAIWSIngressModeCtxPool,
		"openai_responses_mode":                      "force_responses",
	}
	t.Cleanup(func() { svc.getOpenAIWSConnPool().Close() })
	return svc, account, httpRecorder
}

func newHTTPWSPolicyTestContext(path string) (*gin.Context, *httptest.ResponseRecorder) {
	c, recorder := newOpenAIImageGenerationControlTestContext(true, "ctx-pool-test/1.0")
	c.Request = httptest.NewRequest(http.MethodPost, path, nil)
	c.Request.Header.Set("Content-Type", "application/json")
	c.Request.Header.Set("session-id", "http-ws-policy-session")
	SetOpenAIClientTransport(c, OpenAIClientTransportHTTP)
	return c, recorder
}

func writeHTTPWSPolicySuccess(conn *websocket.Conn) error {
	events := []string{
		`{"type":"response.created","response":{"id":"resp_http_ws_policy","status":"in_progress","model":"gpt-6.1-sol"}}`,
		`{"type":"response.output_item.added","output_index":0,"item":{"id":"msg_policy","type":"message","role":"assistant","status":"in_progress","content":[]}}`,
		`{"type":"response.output_text.delta","item_id":"msg_policy","output_index":0,"content_index":0,"delta":"hello from pool"}`,
		`{"type":"response.output_item.done","output_index":0,"item":{"id":"msg_policy","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello from pool","annotations":[]}]}}`,
		`{"type":"response.completed","response":` + httpWSPolicyResponse + `}`,
	}
	for _, event := range events {
		if err := conn.WriteMessage(websocket.TextMessage, []byte(event)); err != nil {
			return err
		}
	}
	return nil
}

func TestHTTPWSPolicyForwardResponses(t *testing.T) {
	for _, passthrough := range []bool{false, true} {
		for _, stream := range []bool{false, true} {
			t.Run(fmt.Sprintf("passthrough_%t_stream_%t", passthrough, stream), func(t *testing.T) {
				var generations atomic.Int32
				requests := make(chan []byte, 1)
				upgrader := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}
				server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
					conn, err := upgrader.Upgrade(w, r, nil)
					if err != nil {
						return
					}
					defer conn.Close()
					_, body, err := conn.ReadMessage()
					if err != nil {
						return
					}
					generations.Add(1)
					requests <- body
					_ = writeHTTPWSPolicySuccess(conn)
				}))
				defer server.Close()
				svc, account, upstream := newHTTPWSPolicyTestService(t, server.URL)
				account.Extra["openai_passthrough"] = passthrough
				c, rec := newHTTPWSPolicyTestContext("/v1/responses")
				body := []byte(fmt.Sprintf(`{"model":"gpt-6.1-sol","stream":%t,"input":"hello"}`, stream))
				result, err := svc.Forward(context.Background(), c, account, body)
				require.NoError(t, err)
				require.NotNil(t, result)
				require.True(t, result.OpenAIWSMode)
				require.Equal(t, "resp_http_ws_policy", result.RequestID)
				require.Equal(t, "resp_http_ws_policy", result.ResponseID)
				require.Equal(t, "response.completed", result.UpstreamTerminalEvent)
				require.Equal(t, 12, result.Usage.InputTokens)
				require.Equal(t, 7, result.Usage.OutputTokens)
				require.EqualValues(t, 1, generations.Load())
				require.Empty(t, upstream.requests, "ctx_pool must bypass upstream HTTP after a successful WS connection")
				require.Contains(t, rec.Body.String(), "hello from pool")
				payload := <-requests
				require.Equal(t, "response.create", gjson.GetBytes(payload, "type").String())
				if stream {
					require.Contains(t, rec.Header().Get("Content-Type"), "text/event-stream")
					require.Contains(t, rec.Body.String(), "response.completed")
				} else {
					require.Equal(t, "completed", gjson.GetBytes(rec.Body.Bytes(), "status").String())
				}
			})
		}
	}
}

func TestHTTPWSPolicyOAuthForwardResponses(t *testing.T) {
	for _, passthrough := range []bool{false, true} {
		for _, stream := range []bool{false, true} {
			t.Run(fmt.Sprintf("passthrough_%t_stream_%t", passthrough, stream), func(t *testing.T) {
				svc, account, upstream := newHTTPWSPolicyTestService(t, "https://unused.example")
				account.Type = AccountTypeOAuth
				account.Credentials = map[string]any{"access_token": "oauth-test-token", "chatgpt_account_id": "chatgpt-test-account"}
				account.Extra = map[string]any{
					"openai_oauth_responses_websockets_v2_mode": OpenAIWSIngressModeCtxPool,
					"openai_passthrough":                        passthrough,
				}
				dialer := &httpWSPolicyURLDialer{inner: &httpWSTestDialer{events: []string{
					`{"type":"response.created","response":{"id":"resp_http_ws_policy","status":"in_progress","model":"gpt-6.1-sol"}}`,
					`{"type":"response.output_text.delta","response_id":"resp_http_ws_policy","output_index":0,"content_index":0,"delta":"hello from pool"}`,
					`{"type":"response.completed","response":` + httpWSPolicyResponse + `}`,
				}}}
				svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
				c, rec := newHTTPWSPolicyTestContext("/v1/responses")
				body := []byte(fmt.Sprintf(`{"model":"gpt-6.1-sol","stream":%t,"instructions":"Answer briefly.","input":"hello"}`, stream))
				result, err := svc.Forward(context.Background(), c, account, body)
				require.NoError(t, err)
				require.NotNil(t, result)
				require.True(t, result.OpenAIWSMode)
				require.Equal(t, "resp_http_ws_policy", result.RequestID)
				require.Equal(t, "response.completed", result.UpstreamTerminalEvent)
				require.Equal(t, 12, result.Usage.InputTokens)
				require.Equal(t, 7, result.Usage.OutputTokens)
				require.Empty(t, upstream.requests)
				require.Contains(t, rec.Body.String(), "hello from pool")
				require.Equal(t, "wss://chatgpt.com/backend-api/codex/responses", dialer.url.Load())
				dialer.inner.mu.Lock()
				count := dialer.inner.count
				headers := cloneHeader(dialer.inner.headers[0])
				dialer.inner.mu.Unlock()
				require.Equal(t, 1, count)
				require.Equal(t, "Bearer oauth-test-token", headers.Get("Authorization"))
				require.Equal(t, "chatgpt-test-account", headers.Get("Chatgpt-Account-Id"))
				if stream {
					require.Contains(t, rec.Header().Get("Content-Type"), "text/event-stream")
				} else {
					require.Equal(t, "completed", gjson.GetBytes(rec.Body.Bytes(), "status").String())
				}
			})
		}
	}
}

func TestHTTPWSPolicyCompatibilityEntrypoints(t *testing.T) {
	for _, entry := range []string{"chat", "messages"} {
		for _, stream := range []bool{false, true} {
			t.Run(fmt.Sprintf("%s_stream_%t", entry, stream), func(t *testing.T) {
				var generations atomic.Int32
				upgrader := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}
				server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
					conn, err := upgrader.Upgrade(w, r, nil)
					if err != nil {
						return
					}
					defer conn.Close()
					if _, _, err := conn.ReadMessage(); err != nil {
						return
					}
					generations.Add(1)
					_ = writeHTTPWSPolicySuccess(conn)
				}))
				defer server.Close()
				svc, account, upstream := newHTTPWSPolicyTestService(t, server.URL)
				path := "/v1/chat/completions"
				if entry == "messages" {
					path = "/v1/messages"
				}
				c, rec := newHTTPWSPolicyTestContext(path)
				body := []byte(fmt.Sprintf(`{"model":"gpt-6.1-sol","stream":%t,"max_tokens":64,"messages":[{"role":"user","content":"hello"}]}`, stream))
				var result *OpenAIForwardResult
				var err error
				if entry == "chat" {
					result, err = svc.ForwardAsChatCompletions(context.Background(), c, account, body, "", "")
				} else {
					result, err = svc.ForwardAsAnthropic(context.Background(), c, account, body, "", "")
				}
				require.NoError(t, err)
				require.NotNil(t, result)
				require.True(t, result.OpenAIWSMode)
				require.Equal(t, "resp_http_ws_policy", result.RequestID)
				require.Equal(t, "response.completed", result.UpstreamTerminalEvent)
				require.Equal(t, 7, result.Usage.OutputTokens)
				require.EqualValues(t, 1, generations.Load())
				require.Empty(t, upstream.requests)
				require.Contains(t, rec.Body.String(), "hello from pool")
			})
		}
	}
}

func TestHTTPWSPolicyOtherModesStayHTTP(t *testing.T) {
	for _, mode := range []string{"", OpenAIWSIngressModeOff, OpenAIWSIngressModePassthrough, OpenAIWSIngressModeHTTPBridge, OpenAIWSIngressModeShared, OpenAIWSIngressModeDedicated} {
		t.Run("mode_"+mode, func(t *testing.T) {
			var connections atomic.Int32
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				connections.Add(1)
				http.Error(w, "unexpected WS", http.StatusBadGateway)
			}))
			defer server.Close()
			svc, account, upstream := newHTTPWSPolicyTestService(t, server.URL)
			account.Extra["openai_apikey_responses_websockets_v2_mode"] = mode
			account.Extra["responses_websockets_v2_enabled"] = true
			c, rec := newHTTPWSPolicyTestContext("/v1/responses")
			result, err := svc.Forward(context.Background(), c, account, []byte(`{"model":"gpt-6.1-sol","stream":false,"input":"hello"}`))
			require.NoError(t, err)
			require.False(t, result.OpenAIWSMode)
			require.Len(t, upstream.requests, 1)
			require.Zero(t, connections.Load())
			require.Contains(t, rec.Body.String(), "hello from pool")
		})
	}
}

func TestHTTPWSPolicyConfigurationFailureDoesNotDispatch(t *testing.T) {
	svc, account, upstream := newHTTPWSPolicyTestService(t, "http://127.0.0.1:1")
	svc.cfg.Gateway.OpenAIWS.Enabled = false
	c, rec := newHTTPWSPolicyTestContext("/v1/responses")
	_, err := svc.Forward(context.Background(), c, account, []byte(`{"model":"gpt-6.1-sol","stream":false,"input":"hello"}`))
	require.Error(t, err)
	require.Equal(t, http.StatusServiceUnavailable, rec.Code)
	require.Contains(t, rec.Body.String(), "ctx_pool_configuration_error")
	require.Empty(t, upstream.requests)
}

func TestHTTPWSPolicyConnectionRetriesThenHTTPFallback(t *testing.T) {
	var connections atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		connections.Add(1)
		http.Error(w, "WS unavailable", http.StatusServiceUnavailable)
	}))
	defer server.Close()
	svc, account, upstream := newHTTPWSPolicyTestService(t, server.URL)
	c, rec := newHTTPWSPolicyTestContext("/v1/responses")
	result, err := svc.Forward(context.Background(), c, account, []byte(`{"model":"gpt-6.1-sol","stream":false,"input":"hello"}`))
	require.NoError(t, err)
	require.NotNil(t, result)
	require.False(t, result.OpenAIWSMode)
	require.EqualValues(t, 4, connections.Load(), "initial connection plus exactly three retries")
	require.Len(t, upstream.requests, 1)
	require.Contains(t, rec.Body.String(), "hello from pool")
}

func TestHTTPWSPolicyDoesNotReplayDispatchedGeneration(t *testing.T) {
	for _, failure := range []string{"disconnect", "response_failed"} {
		t.Run(failure, func(t *testing.T) {
			var generations atomic.Int32
			upgrader := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				conn, err := upgrader.Upgrade(w, r, nil)
				if err != nil {
					return
				}
				defer conn.Close()
				if _, _, err := conn.ReadMessage(); err != nil {
					return
				}
				generations.Add(1)
				if failure == "response_failed" {
					_ = conn.WriteMessage(websocket.TextMessage, []byte(`{"type":"response.failed","response":{"id":"resp_failed_policy","status":"failed","error":{"code":"server_error","message":"temporarily unavailable"}}}`))
				}
			}))
			defer server.Close()
			svc, account, upstream := newHTTPWSPolicyTestService(t, server.URL)
			c, _ := newHTTPWSPolicyTestContext("/v1/responses")
			_, err := svc.Forward(context.Background(), c, account, []byte(`{"model":"gpt-6.1-sol","stream":true,"input":"hello"}`))
			require.Error(t, err)
			var failover *UpstreamFailoverError
			if errors.As(err, &failover) {
				require.False(t, failover.ShouldRetryNextAccount())
				require.False(t, failover.RetryableOnSameAccount)
			}
			require.EqualValues(t, 1, generations.Load())
			require.Empty(t, upstream.requests, "an already dispatched generation must never be retried through HTTP")
		})
	}
}
