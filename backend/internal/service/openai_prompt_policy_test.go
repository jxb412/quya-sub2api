package service

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http/httptest"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/pkg/ctxkey"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

func enableZeroPromptForTest(_ *OpenAIGatewayService, _ *Account, c *gin.Context) {
	key := getAPIKeyFromContext(c)
	key.Group.Platform = PlatformOpenAI
	key.Group.OpenAIZeroPromptInjection = true
}

func TestZeroPromptPolicyAuthenticatedGroupAndRetry(t *testing.T) {
	for _, tc := range []struct {
		platform string
		enabled  bool
		want     bool
	}{
		{PlatformOpenAI, true, true}, {PlatformOpenAI, false, false}, {PlatformAnthropic, true, false},
	} {
		t.Run(fmt.Sprintf("%s/%t", tc.platform, tc.enabled), func(t *testing.T) {
			c, _ := gin.CreateTestContext(httptest.NewRecorder())
			group := &Group{ID: 1, Platform: tc.platform, OpenAIZeroPromptInjection: tc.enabled}
			c.Set("api_key", &APIKey{Group: group})
			require.Equal(t, tc.want, openAIZeroPromptInjection(c))
			// A retry/bridge cannot reverse the original authenticated policy.
			c.Set("api_key", &APIKey{Group: &Group{Platform: PlatformOpenAI, OpenAIZeroPromptInjection: !tc.want}})
			require.Equal(t, tc.want, openAIZeroPromptInjection(c))
		})
	}
	require.False(t, openAIZeroPromptInjection(nil))
	c, _ := gin.CreateTestContext(httptest.NewRecorder())
	c.Request = httptest.NewRequest("POST", "/v1/responses", nil)
	c.Request = c.Request.WithContext(context.WithValue(c.Request.Context(), ctxkey.Group, &Group{Platform: PlatformOpenAI, OpenAIZeroPromptInjection: true}))
	require.True(t, openAIZeroPromptInjection(c))
}

func TestZeroPromptTransformPreservesCallerGuidance(t *testing.T) {
	for _, model := range []string{"gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-sol", "gpt-5.4", "gpt-5.3-codex-spark"} {
		for _, compact := range []bool{false, true} {
			t.Run(fmt.Sprintf("%s/compact=%t", model, compact), func(t *testing.T) {
				var body map[string]any
				require.NoError(t, json.Unmarshal([]byte(`{"instructions":"  Client turn.\n","input":[{"role":"system","content":"Client system."},{"role":"user","content":"Hello"},{"role":"developer","content":"Later client guidance."}]}`), &body))
				body["model"] = model
				result := applyCodexOAuthTransformWithOptions(body, codexOAuthTransformOptions{ZeroPromptInjection: true, IsCompact: compact})
				require.NoError(t, result.Error)
				require.Equal(t, "  Client turn.\n", body["instructions"])
				require.Len(t, body["input"], 3)
				stableInstructionsInput(t, body, 0, "developer", "Client system.")
				stableInstructionsInput(t, body, 1, "user", "Hello")
				stableInstructionsInput(t, body, 2, "developer", "Later client guidance.")
			})
		}
	}
}

func TestZeroPromptForwardNoDefaultOnHTTPOrWSPool(t *testing.T) {
	for _, model := range []string{"gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-sol", "gpt-5.4", "gpt-5.3-codex-spark"} {
		for _, transport := range []string{"http", "ws"} {
			t.Run(model+"/"+transport, func(t *testing.T) {
				payload := forwardStableInstructionsPayload(t, model, transport, "/v1/responses",
					`{"model":"stable-alias","stream":false,"input":"hello"}`, "", enableZeroPromptForTest)
				require.Equal(t, "", payload["instructions"])
				require.Len(t, payload["input"], 1)
				stableInstructionsInput(t, payload, 0, "user", "hello")
			})
		}
	}
}

func TestZeroPromptForwardCompatibilitySkipsTemplateAndTodo(t *testing.T) {
	for _, model := range []string{"gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-sol", "gpt-5.4"} {
		for _, transport := range []string{"http", "ws"} {
			for _, endpoint := range []string{"/v1/chat/completions", "/v1/messages"} {
				t.Run(model+"/"+transport+endpoint, func(t *testing.T) {
					body := `{"model":"stable-alias","stream":false,"messages":[{"role":"system","content":"Client rules."},{"role":"user","content":"hello"}]}`
					if endpoint == "/v1/messages" {
						body = `{"model":"stable-alias","max_tokens":16,"stream":false,"system":"Client rules.","messages":[{"role":"user","content":"hello"}]}`
					}
					payload := forwardStableInstructionsPayload(t, model, transport, endpoint, body, "SERVER-INJECTED-TEMPLATE", enableZeroPromptForTest)
					require.Equal(t, "", payload["instructions"])
					require.Len(t, payload["input"], 2)
					stableInstructionsInput(t, payload, 0, "developer", "Client rules.")
					stableInstructionsInput(t, payload, 1, "user", "hello")
				})
			}
		}
	}
}

func TestZeroPromptPassthroughAndThirdPartyAPIKey(t *testing.T) {
	for _, accountType := range []string{AccountTypeOAuth, AccountTypeAPIKey} {
		for _, passthrough := range []bool{false, true} {
			for _, endpoint := range []string{"/v1/responses", "/v1/messages"} {
				t.Run(fmt.Sprintf("%s/passthrough=%t%s", accountType, passthrough, endpoint), func(t *testing.T) {
					body := `{"model":"gpt-6.1-sol","stream":false,"instructions":"Client rules.","input":[{"role":"user","content":"hello"}]}`
					if endpoint == "/v1/messages" {
						body = `{"model":"gpt-6.1-sol","stream":false,"system":"Client rules.","max_tokens":16,"messages":[{"role":"user","content":"hello"}]}`
					}
					payload := forwardStableInstructionsPayload(t, "gpt-6.1-sol", "http", endpoint, body, "SERVER-INJECTED-TEMPLATE", enableZeroPromptForTest, func(_ *OpenAIGatewayService, account *Account, _ *gin.Context) {
						account.Type = accountType
						account.Extra["openai_passthrough"] = passthrough
						if accountType == AccountTypeAPIKey {
							account.Credentials = map[string]any{"api_key": "synthetic-api-key", "base_url": "https://provider.example"}
						}
					})
					encoded, err := json.Marshal(payload)
					require.NoError(t, err)
					require.NotContains(t, string(encoded), "SERVER-INJECTED-TEMPLATE")
					require.NotContains(t, string(encoded), openAICompatClaudeCodeTodoGuardMarker)
					require.Contains(t, string(encoded), "Client rules.")
					require.Contains(t, string(encoded), "hello")
				})
			}
		}
	}
}

func TestZeroPromptOAuthPassthroughMissingInstructions(t *testing.T) {
	for _, model := range []string{"gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-sol", "gpt-5.3-codex"} {
		for _, transport := range []string{"http", "ws"} {
			t.Run(model+"/"+transport, func(t *testing.T) {
				body := fmt.Sprintf(`{"model":%q,"stream":false,"input":[{"role":"user","content":"hello"}]}`, model)
				payload := forwardStableInstructionsPayload(t, model, transport, "/v1/responses", body, "", enableZeroPromptForTest, func(_ *OpenAIGatewayService, account *Account, _ *gin.Context) {
					account.Extra["openai_passthrough"] = true
				})
				require.Empty(t, payload["instructions"])
				require.Len(t, payload["input"], 1)
				stableInstructionsInput(t, payload, 0, "user", "hello")
			})
		}
	}
}

func TestZeroPromptEmptyToolOutputsAcrossChatAndLegacyIngress(t *testing.T) {
	for _, endpoint := range []string{"/v1/chat/completions", "/v1/responses"} {
		for _, transport := range []string{"http", "ws"} {
			t.Run(endpoint+"/"+transport, func(t *testing.T) {
				payload := forwardStableInstructionsPayload(t, "gpt-6.2-sol", transport, endpoint,
					`{"model":"stable-alias","stream":false,"messages":[{"role":"user","content":"hello"},{"role":"assistant","tool_calls":[{"id":"call_empty","type":"function","function":{"name":"ping","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_empty","content":""}]}`, "", enableZeroPromptForTest)
				encoded, err := json.Marshal(payload)
				require.NoError(t, err)
				output := gjson.GetBytes(encoded, `input.#(type=="function_call_output").output`)
				require.True(t, output.Exists())
				require.Equal(t, "", output.String())
				require.NotContains(t, string(encoded), "(empty)")
			})
		}
	}
	account := &Account{Platform: PlatformOpenAI, Type: AccountTypeOAuth}
	body := []byte(`{"type":"response.create","model":"gpt-6.2-sol","messages":[{"role":"assistant","tool_calls":[{"id":"call_empty","type":"function","function":{"name":"ping","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_empty","content":""}]}`)
	normalized, _, err := normalizeOpenAIResponsesWebSocketCompatibilityBodyWithPromptPolicy(nil, body, account, false, true)
	require.NoError(t, err)
	require.NotContains(t, string(normalized), "(empty)")
	require.True(t, gjson.GetBytes(normalized, `input.#(type=="function_call_output").output`).Exists())
}
