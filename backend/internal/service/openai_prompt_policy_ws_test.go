package service

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/Wei-Shaw/sub2api/internal/config"
	coderws "github.com/coder/websocket"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

// 2026-11-26: exercise real WS ingress and inspect captured upstream frames so
// a future bridge refactor cannot silently reintroduce gateway-authored text.
func TestOpenAIZeroPromptInjection_WSIngressImageBridge(t *testing.T) {
	gin.SetMode(gin.TestMode)
	for _, accountType := range []string{AccountTypeOAuth, AccountTypeAPIKey} {
		for _, enabled := range []bool{false, true} {
			name := accountType + "/default"
			if enabled {
				name = accountType + "/zero_added_prompts"
			}
			t.Run(name, func(t *testing.T) {
				cfg := &config.Config{}
				cfg.Security.URLAllowlist.Enabled = false
				cfg.Security.URLAllowlist.AllowInsecureHTTP = true
				cfg.Gateway.OpenAIWS.Enabled = true
				cfg.Gateway.OpenAIWS.OAuthEnabled = true
				cfg.Gateway.OpenAIWS.APIKeyEnabled = true
				cfg.Gateway.OpenAIWS.ResponsesWebsocketsV2 = true
				cfg.Gateway.OpenAIWS.MaxConnsPerAccount = 1
				cfg.Gateway.OpenAIWS.MaxIdlePerAccount = 1
				cfg.Gateway.OpenAIWS.QueueLimitPerConn = 8
				cfg.Gateway.OpenAIWS.DialTimeoutSeconds = 3
				cfg.Gateway.OpenAIWS.ReadTimeoutSeconds = 3
				cfg.Gateway.OpenAIWS.WriteTimeoutSeconds = 3

				capture := &openAIWSCaptureConn{events: [][]byte{
					[]byte(`{"type":"response.completed","response":{"id":"resp_zero_prompt_1","model":"gpt-5.5","usage":{"input_tokens":1,"output_tokens":1}}}`),
					[]byte(`{"type":"response.completed","response":{"id":"resp_zero_prompt_2","model":"gpt-5.5","usage":{"input_tokens":1,"output_tokens":1}}}`),
				}}
				pool := newOpenAIWSConnPool(cfg)
				defer pool.Close()
				pool.setClientDialerForTest(&openAIWSCaptureDialer{conn: capture})
				svc := &OpenAIGatewayService{
					cfg: cfg, httpUpstream: &httpUpstreamRecorder{}, cache: &stubGatewayCache{},
					openaiWSResolver: NewOpenAIWSProtocolResolver(cfg),
					toolCorrector:    NewCodexToolCorrector(), openaiWSPool: pool,
				}
				groupID := int64(901)
				apiKey := &APIKey{ID: 902, UserID: 903, GroupID: &groupID, Group: &Group{
					ID: groupID, Platform: PlatformOpenAI, AllowImageGeneration: true,
					OpenAIZeroPromptInjection: enabled,
				}}
				account := &Account{
					ID: 904, Platform: PlatformOpenAI, Type: accountType, Status: StatusActive,
					Schedulable: true, Concurrency: 1,
					Credentials: map[string]any{"access_token": "test-token", "api_key": "test-token"},
					Extra: map[string]any{
						"openai_oauth_responses_websockets_v2_enabled": true,
						"responses_websockets_v2_enabled":              true,
						"codex_image_generation_bridge":                true,
					},
				}
				serverErr := make(chan error, 1)
				server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
					conn, err := coderws.Accept(w, r, nil)
					if err != nil {
						serverErr <- err
						return
					}
					defer func() { _ = conn.CloseNow() }()
					ginCtx, _ := gin.CreateTestContext(httptest.NewRecorder())
					ginCtx.Request = r.Clone(r.Context())
					ginCtx.Request.Header = r.Header.Clone()
					ginCtx.Request.Header.Set("User-Agent", "codex_cli_rs/0.98.0")
					ginCtx.Set("api_key", apiKey)
					readCtx, cancel := context.WithTimeout(r.Context(), 3*time.Second)
					_, first, err := conn.Read(readCtx)
					cancel()
					if err != nil {
						serverErr <- err
						return
					}
					serverErr <- svc.ProxyResponsesWebSocketFromClient(r.Context(), ginCtx, conn, account, "test-token", first, nil)
				}))
				defer server.Close()
				ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
				defer cancel()
				client, _, err := coderws.Dial(ctx, "ws"+strings.TrimPrefix(server.URL, "http"), nil)
				require.NoError(t, err)
				defer func() { _ = client.CloseNow() }()
				frames := []string{
					`{"type":"response.create","model":"gpt-5.5","instructions":"client-only-guidance","input":"draw a cat"}`,
					`{"type":"response.create","model":"gpt-5.5","previous_response_id":"resp_zero_prompt_1","instructions":"client-only-guidance","input":"draw a dog","tools":[{"type":"image_generation","output_format":"png"}],"tool_choice":{"type":"image_generation"}}`,
				}
				for _, frame := range frames {
					require.NoError(t, client.Write(ctx, coderws.MessageText, []byte(frame)))
					_, response, err := client.Read(ctx)
					require.NoError(t, err)
					require.Equal(t, "response.completed", gjson.GetBytes(response, "type").String())
				}
				require.NoError(t, client.Close(coderws.StatusNormalClosure, "done"))
				select {
				case err := <-serverErr:
					require.NoError(t, err)
				case <-ctx.Done():
					t.Fatal("websocket proxy did not finish")
				}
				require.Len(t, capture.writes, 2)
				for index, sent := range capture.writes {
					encoded, err := json.Marshal(sent)
					require.NoError(t, err)
					instructions := gjson.GetBytes(encoded, "instructions").String()
					hasImageTool := gjson.GetBytes(encoded, `tools.#(type=="image_generation")`).Exists()
					if enabled {
						require.Equal(t, "client-only-guidance", instructions)
						require.Equal(t, index == 1, hasImageTool, "only the client-provided image tool may remain")
						require.NotContains(t, string(encoded), codexImageGenerationBridgeText)
						if index == 0 {
							require.False(t, gjson.GetBytes(encoded, "tool_choice").Exists())
						}
					} else {
						require.Contains(t, instructions, "client-only-guidance")
						require.Contains(t, instructions, codexImageGenerationBridgeText)
						require.True(t, hasImageTool)
					}
					if index == 1 && accountType == AccountTypeOAuth {
						// Changing tools requires a full create on OAuth; replay may
						// contain both client turns, but no newly authored prompt.
						require.JSONEq(t, `["draw a cat","draw a dog"]`, gjson.GetBytes(encoded, "input").Raw)
					} else {
						require.Equal(t, gjson.Get(frames[index], "input").String(), gjson.GetBytes(encoded, "input").String())
					}
				}
			})
		}
	}
}

func TestOpenAIZeroPromptInjection_WSHTTPBridgeInheritedTools(t *testing.T) {
	gin.SetMode(gin.TestMode)
	upstream := &httpUpstreamRecorder{responses: []*http.Response{}}
	for _, id := range []string{"resp_tools_first", "resp_tools_second"} {
		sse := "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"" + id + "\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
		upstream.responses = append(upstream.responses, &http.Response{
			StatusCode: http.StatusOK, Header: http.Header{"Content-Type": []string{"text/event-stream"}},
			Body: io.NopCloser(strings.NewReader(sse)),
		})
	}
	svc := &OpenAIGatewayService{
		cfg:          &config.Config{Gateway: config.GatewayConfig{MaxLineSize: defaultMaxLineSize}},
		httpUpstream: upstream,
	}
	account := &Account{ID: 905, Platform: PlatformOpenAI, Type: AccountTypeAPIKey, Concurrency: 1}
	c, _ := gin.CreateTestContext(httptest.NewRecorder())
	c.Request = httptest.NewRequest(http.MethodGet, "/v1/responses", nil)
	c.Set("api_key", &APIKey{ID: 906, Group: &Group{Platform: PlatformOpenAI, OpenAIZeroPromptInjection: true}})
	frames := []string{
		`{"type":"response.create","model":"gpt-5.5","instructions":"client-only","tools":[{"type":"custom","name":"exec","description":"client custom tool"},{"type":"tool_search","description":"client search tool"}],"input":"find tools"}`,
		`{"type":"response.create","model":"gpt-5.5","instructions":"client-only","input":[{"type":"tool_search_output","call_id":"call_search","status":"completed","tools":[{"type":"function","name":"read_file","description":"client discovered tool","parameters":{"type":"object"}}]}]}`,
	}
	for index, frame := range frames {
		_, err := svc.proxyOpenAIWSHTTPBridgeTurn(
			context.Background(), c, account, "test-token", []byte(frame), len(frame),
			"gpt-5.5", "", "", "", "", index+1, func([]byte) error { return nil },
		)
		require.NoError(t, err)
	}
	require.Len(t, upstream.bodies, 2)
	for _, sent := range upstream.bodies {
		require.Equal(t, "client-only", gjson.GetBytes(sent, "instructions").String())
		require.Equal(t, "client custom tool", gjson.GetBytes(sent, `tools.#(name=="exec").description`).String())
		require.Equal(t, "client search tool", gjson.GetBytes(sent, `tools.#(name=="tool_search").description`).String())
		require.False(t, gjson.GetBytes(sent, `tools.#(name=="exec").parameters.properties.input.description`).Exists())
	}
	require.Equal(t, "client discovered tool", gjson.GetBytes(upstream.bodies[1], `tools.#(name=="read_file").description`).String())
	require.Equal(t, "function_call_output", gjson.GetBytes(upstream.bodies[1], "input.0.type").String())
}
