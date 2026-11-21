package service

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"testing"

	"github.com/stretchr/testify/require"
)

// Exercise the public forwarding paths: an early default-instructions patch or
// alias mapping must not undo the model-specific transform farther downstream.
func forwardStableInstructionsPayload(t *testing.T, model, transport, endpoint, body, template string) map[string]any {
	t.Helper()
	svc, account, upstream := newHTTPWSPolicyTestService(t, "https://unused.example")
	account.Type = AccountTypeOAuth
	account.Credentials = map[string]any{
		"access_token": "oauth-test-token", "chatgpt_account_id": "chatgpt-test-account",
		"model_mapping": map[string]any{"stable-alias": model},
	}
	account.Extra = map[string]any{
		"openai_oauth_responses_websockets_v2_mode": "off",
		"openai_passthrough":                        false,
	}
	svc.cfg.Gateway.ForcedCodexInstructionsTemplate = template
	response := strings.ReplaceAll(httpWSPolicyResponse, "gpt-6.1-sol", model)
	upstream.resp = &http.Response{
		StatusCode: http.StatusOK,
		Header:     http.Header{"Content-Type": {"text/event-stream"}},
		Body:       io.NopCloser(strings.NewReader("data: {\"type\":\"response.completed\",\"response\":" + response + "}\n\ndata: [DONE]\n\n")),
	}
	var dialer *httpWSTestDialer
	if transport == "ws" {
		account.Extra["openai_oauth_responses_websockets_v2_mode"] = OpenAIWSIngressModeCtxPool
		dialer = &httpWSTestDialer{events: []string{
			`{"type":"response.created","response":{"id":"resp_http_ws_policy","status":"in_progress","model":"` + model + `"}}`,
			`{"type":"response.output_text.delta","response_id":"resp_http_ws_policy","output_index":0,"content_index":0,"delta":"hello from pool"}`,
			`{"type":"response.completed","response":` + response + `}`,
		}}
		svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
	}
	c, rec := newHTTPWSPolicyTestContext(endpoint)
	var result *OpenAIForwardResult
	var err error
	switch endpoint {
	case "/v1/chat/completions":
		result, err = svc.ForwardAsChatCompletions(context.Background(), c, account, []byte(body), "", "")
	case "/v1/messages":
		result, err = svc.ForwardAsAnthropic(context.Background(), c, account, []byte(body), "", "")
	default:
		result, err = svc.Forward(context.Background(), c, account, []byte(body))
	}
	require.NoError(t, err)
	require.NotNil(t, result)
	require.Equal(t, transport == "ws", result.OpenAIWSMode)
	require.Contains(t, rec.Body.String(), "hello from pool")
	var payload []byte
	if transport == "ws" {
		require.Empty(t, upstream.requests)
		dialer.mu.Lock()
		conns := append([]*httpWSTestConn(nil), dialer.conns...)
		dialer.mu.Unlock()
		require.Len(t, conns, 1)
		conns[0].mu.Lock()
		writes := append([]map[string]json.RawMessage(nil), conns[0].writes...)
		conns[0].mu.Unlock()
		require.Len(t, writes, 1)
		payload, err = json.Marshal(writes[0])
		require.NoError(t, err)
	} else {
		require.Len(t, upstream.requests, 1)
		payload = upstream.lastBody
	}
	var decoded map[string]any
	require.NoError(t, json.Unmarshal(payload, &decoded))
	require.Equal(t, model, decoded["model"])
	return decoded
}

func stableInstructionsInput(t *testing.T, payload map[string]any, index int, role, text string) {
	t.Helper()
	input, ok := payload["input"].([]any)
	require.True(t, ok)
	require.Greater(t, len(input), index)
	message, ok := input[index].(map[string]any)
	require.True(t, ok)
	require.Equal(t, role, message["role"])
	actual, lossless := extractLosslessTextFromContent(message["content"])
	require.True(t, lossless)
	require.Equal(t, text, actual)
}

func TestCodexStableInstructionsForwardResponses(t *testing.T) {
	for _, model := range []string{"gpt-6.2-sol", "gpt-6.2-astra"} {
		for _, transport := range []string{"http", "ws"} {
			t.Run(model+"/"+transport+"/explicit_turn", func(t *testing.T) {
				payload := forwardStableInstructionsPayload(t, model, transport, "/v1/responses",
					`{"model":"stable-alias","stream":false,"instructions":"  This turn only.\n","input":[{"role":"system","content":"Project rules."},{"role":"user","content":"hello"}]}`, "")
				require.Equal(t, "  This turn only.\n", payload["instructions"])
				require.Len(t, payload["input"], 2)
				stableInstructionsInput(t, payload, 0, "developer", "Project rules.")
				stableInstructionsInput(t, payload, 1, "user", "hello")
			})
			t.Run(model+"/"+transport+"/default", func(t *testing.T) {
				payload := forwardStableInstructionsPayload(t, model, transport, "/v1/responses",
					`{"model":"stable-alias","stream":false,"input":"hello"}`, "")
				require.Equal(t, "", payload["instructions"])
				require.Len(t, payload["input"], 2)
				stableInstructionsInput(t, payload, 0, "developer", defaultCodexSynthInstructions(model))
				stableInstructionsInput(t, payload, 1, "user", "hello")
			})
		}
	}
}

func TestCodexStableInstructionsForwardCompatibility(t *testing.T) {
	for _, transport := range []string{"http", "ws"} {
		t.Run(transport+"/chat", func(t *testing.T) {
			payload := forwardStableInstructionsPayload(t, "gpt-6.2-sol", transport, "/v1/chat/completions",
				`{"model":"stable-alias","stream":false,"messages":[{"role":"system","content":"Project rules."},{"role":"user","content":"hello"}]}`, "")
			require.Equal(t, "", payload["instructions"])
			require.Len(t, payload["input"], 2)
			stableInstructionsInput(t, payload, 0, "developer", "Project rules.")
			stableInstructionsInput(t, payload, 1, "user", "hello")
		})
		for _, tc := range []struct {
			name     string
			template string
			prefixes []string
		}{
			{name: "plain", prefixes: []string{"client-system"}},
			{name: "forced_template", template: "server-prefix\n\n{{ .ExistingInstructions }}", prefixes: []string{"server-prefix\n\nclient-system"}},
			{name: "template_without_placeholder", template: "server-prefix", prefixes: []string{"server-prefix", "client-system"}},
		} {
			t.Run(transport+"/messages/"+tc.name, func(t *testing.T) {
				payload := forwardStableInstructionsPayload(t, "gpt-6.2-sol", transport, "/v1/messages",
					`{"model":"stable-alias","max_tokens":16,"system":"client-system","messages":[{"role":"user","content":"hello"}],"stream":false}`, tc.template)
				require.Equal(t, "", payload["instructions"])
				require.Len(t, payload["input"], len(tc.prefixes)+2)
				for i, prefix := range tc.prefixes {
					stableInstructionsInput(t, payload, i, "developer", prefix)
				}
				stableInstructionsInput(t, payload, len(tc.prefixes), "developer", openAICompatClaudeCodeTodoGuardText)
				stableInstructionsInput(t, payload, len(tc.prefixes)+1, "user", "hello")
			})
		}
	}
}

func TestCodexStableInstructionsForwardLegacyModelsUnchanged(t *testing.T) {
	for _, model := range []string{"gpt-6.1-sol", "gpt-5.4"} {
		for _, transport := range []string{"http", "ws"} {
			t.Run(model+"/"+transport+"/responses", func(t *testing.T) {
				payload := forwardStableInstructionsPayload(t, model, transport, "/v1/responses",
					`{"model":"stable-alias","stream":false,"instructions":"This turn only.","input":[{"role":"system","content":"Project rules."},{"role":"user","content":"hello"}]}`, "")
				require.Equal(t, "Project rules.\n\nThis turn only.", payload["instructions"])
			})
			t.Run(model+"/"+transport+"/default", func(t *testing.T) {
				payload := forwardStableInstructionsPayload(t, model, transport, "/v1/responses",
					`{"model":"stable-alias","stream":false,"input":"hello"}`, "")
				require.Equal(t, defaultCodexSynthInstructions(model), payload["instructions"])
				require.Len(t, payload["input"], 1)
				stableInstructionsInput(t, payload, 0, "user", "hello")
			})
			t.Run(model+"/"+transport+"/chat", func(t *testing.T) {
				payload := forwardStableInstructionsPayload(t, model, transport, "/v1/chat/completions",
					`{"model":"stable-alias","stream":false,"messages":[{"role":"system","content":"Project rules."},{"role":"user","content":"hello"}]}`, "")
				require.Equal(t, "Project rules.", payload["instructions"])
				require.Len(t, payload["input"], 1)
				stableInstructionsInput(t, payload, 0, "user", "hello")
			})
			t.Run(model+"/"+transport+"/messages_template", func(t *testing.T) {
				payload := forwardStableInstructionsPayload(t, model, transport, "/v1/messages",
					`{"model":"stable-alias","max_tokens":16,"system":"client-system","messages":[{"role":"user","content":"hello"}],"stream":false}`,
					"server-prefix\n\n{{ .ExistingInstructions }}")
				require.Equal(t, "server-prefix\n\nclient-system", payload["instructions"])
			})
		}
	}
}

func TestCodexStableInstructionsForcedTemplatePreservesTurnAndLaterDeveloper(t *testing.T) {
	body := map[string]any{
		"instructions": "  This turn only.\n",
		"input": []any{
			map[string]any{"role": "developer", "content": "Project rules."},
			map[string]any{"role": "developer", "content": "Additional rules."},
			map[string]any{"role": "user", "content": "hello"},
			map[string]any{"role": "developer", "content": "Later guidance."},
		},
	}
	modified, err := applyForcedCodexInstructionsTemplate(body, "server-prefix\n\n{{ .ExistingInstructions }}", forcedCodexInstructionsTemplateData{UpstreamModel: "gpt-6.2-sol"})
	require.NoError(t, err)
	require.True(t, modified)
	require.Equal(t, "  This turn only.\n", body["instructions"])
	require.Len(t, body["input"], 3)
	stableInstructionsInput(t, body, 0, "developer", "server-prefix\n\nProject rules.\n\nAdditional rules.")
	stableInstructionsInput(t, body, 1, "user", "hello")
	stableInstructionsInput(t, body, 2, "developer", "Later guidance.")
	before, err := json.Marshal(body)
	require.NoError(t, err)
	body["previous_response_id"] = "resp_previous"
	modified, err = applyForcedCodexInstructionsTemplate(body, "server-prefix\n\n{{ .ExistingInstructions }}", forcedCodexInstructionsTemplateData{UpstreamModel: "gpt-6.2-sol"})
	require.NoError(t, err)
	require.False(t, modified)
	delete(body, "previous_response_id")
	after, err := json.Marshal(body)
	require.NoError(t, err)
	require.JSONEq(t, string(before), string(after))
}
