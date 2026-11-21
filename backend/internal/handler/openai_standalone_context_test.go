package handler

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/pkg/apicompat"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
	"go.uber.org/zap"
)

// 2026-11-21: issue #8999; standalone browser/terminal context has no tool call.
func TestNormalizeCodexStandaloneContextHTTPValidation(t *testing.T) {
	for _, name := range []string{"browser_tabs", "terminal_info"} {
		for _, escaped := range []bool{false, true} {
			body := pageContextRequest(t, pageContextItem(name, "context\n第二行"))
			if escaped {
				replacement := `\u0062rowser_tabs`
				if name == "terminal_info" {
					replacement = `\u0074erminal_info`
				}
				body = []byte(strings.Replace(string(body), name, replacement, 1))
			}
			h := &OpenAIGatewayHandler{}
			before := httptest.NewRecorder()
			c, _ := gin.CreateTestContext(before)
			require.False(t, h.validateFunctionCallOutputRequest(c, body, zap.NewNop()))
			require.Equal(t, http.StatusBadRequest, before.Code)
			require.Contains(t, before.Body.String(), "function_call_output requires call_id")
			got, changed := normalizeCodexUntrustedContext(body)
			require.True(t, changed, "name=%s escaped=%v", name, escaped)
			c, _ = gin.CreateTestContext(httptest.NewRecorder())
			require.True(t, h.validateFunctionCallOutputRequest(c, got, zap.NewNop()))
			require.False(t, c.Writer.Written())
			require.Equal(t, "<external_"+name+">context\n第二行</external_"+name+">", gjson.GetBytes(got, "input.0.content.0.text").String())
		}
	}
}

func TestNormalizeCodexStandaloneContextExactNamesOnly(t *testing.T) {
	for _, name := range []string{"browser_tabs", "terminal_info"} {
		for _, invalid := range []string{"prefix_" + name, name + "_extra", strings.ToUpper(name), " " + name, name + " "} {
			body := pageContextRequest(t, pageContextItem(invalid, "context"))
			got, changed := normalizeCodexUntrustedContext(body)
			require.False(t, changed, "name=%s", invalid)
			require.Equal(t, body, got)
		}
	}
}

func TestNormalizeCodexStandaloneContextPreservesMixedHistory(t *testing.T) {
	body := []byte(`{"model":"gpt-5","previous_response_id":"resp_history","metadata":{"large":9007199254740993},"input":[{"type":"function_call","namespace":"agent","name":"browser_tabs","call_id":"call_real","arguments":"{}"},{"type":"function_call_output","namespace":"agent","name":"browser_tabs","call_id":"call_real","output":"real result"},{"type":"function_call_output","namespace":"agent","name":"browser_tabs","output":"{\"tabs\":[]}"},{"type":"function_call_output","namespace":"agent","name":"terminal_info","output":"cwd: /workspace\n终端: bash"},{"role":"user","content":"continue"}]}`)
	got, changed := normalizeCodexUntrustedContext(body)
	require.True(t, changed)
	require.Equal(t, "resp_history", gjson.GetBytes(got, "previous_response_id").String())
	require.Equal(t, "9007199254740993", gjson.GetBytes(got, "metadata.large").Raw)
	require.Len(t, gjson.GetBytes(got, "input").Array(), 5)
	for _, path := range []string{"input.0", "input.1", "input.4"} {
		require.JSONEq(t, gjson.GetBytes(body, path).Raw, gjson.GetBytes(got, path).Raw)
	}
	require.Equal(t, `<external_browser_tabs>{"tabs":[]}</external_browser_tabs>`, gjson.GetBytes(got, "input.2.content.0.text").String())
	require.Equal(t, "<external_terminal_info>cwd: /workspace\n终端: bash</external_terminal_info>", gjson.GetBytes(got, "input.3.content.0.text").String())
	again, changed := normalizeCodexUntrustedContext(got)
	require.False(t, changed)
	require.Equal(t, got, again)
}

func TestNormalizeCodexStandaloneContextPreservesAmbiguousToolHistory(t *testing.T) {
	for _, name := range []string{"browser_tabs", "terminal_info"} {
		for _, tc := range []struct {
			label   string
			callID  any
			outputs int
			calls   int
		}{
			{"missing_id", nil, 0, 1},
			{"blank_id", " \t", 0, 1},
			{"invalid_id", 42, 0, 1},
			{"unpaired_call", "call_real", 0, 1},
			{"duplicate_outputs", "call_real", 2, 1},
			{"duplicate_calls", "call_real", 1, 2},
		} {
			t.Run(name+"/"+tc.label, func(t *testing.T) {
				call := map[string]any{"type": "function_call", "namespace": "agent", "name": name, "arguments": "{}"}
				if tc.callID != nil {
					call["call_id"] = tc.callID
				}
				items := []any{call}
				for i := 1; i < tc.calls; i++ {
					items = append(items, call)
				}
				for i := 0; i < tc.outputs; i++ {
					items = append(items, map[string]any{"type": "function_call_output", "call_id": tc.callID, "output": "tool result"})
				}
				items = append(items, pageContextItem(name, "ambiguous result"))
				body := pageContextRequest(t, items...)
				got, changed := normalizeCodexUntrustedContext(body)
				require.False(t, changed)
				require.Equal(t, body, got)
			})
		}
	}
}

func TestNormalizeCodexStandaloneContextSurvivesProtocolConversion(t *testing.T) {
	body := pageContextRequest(t,
		pageContextItem("browser_tabs", `{"tabs":[{"title":"资料","url":"https://example.com/?a=1&b=2"}]}`),
		pageContextItem("terminal_info", "cwd: /workspace\n$ printf '<example>'\n"),
		map[string]any{"role": "user", "content": "continue"})
	got, changed := normalizeCodexUntrustedContext(body)
	require.True(t, changed)
	var request apicompat.ResponsesRequest
	require.NoError(t, json.Unmarshal(got, &request))
	chat, err := apicompat.ResponsesToChatCompletionsRequest(&request)
	require.NoError(t, err)
	anthropic, err := apicompat.ResponsesToAnthropicRequest(&request)
	require.NoError(t, err)
	want := []string{gjson.GetBytes(got, "input.0.content.0.text").String(), gjson.GetBytes(got, "input.1.content.0.text").String(), "continue"}
	for name, converted := range map[string]any{"chat": chat, "anthropic": anthropic} {
		t.Run(name, func(t *testing.T) {
			encoded, err := json.Marshal(converted)
			require.NoError(t, err)
			var texts []string
			for _, message := range gjson.GetBytes(encoded, "messages").Array() {
				require.Equal(t, "user", message.Get("role").String())
				content := message.Get("content")
				if content.Type == gjson.String {
					texts = append(texts, content.String())
					continue
				}
				for _, part := range content.Array() {
					require.Equal(t, "text", part.Get("type").String())
					texts = append(texts, part.Get("text").String())
				}
			}
			require.Equal(t, want, texts)
		})
	}
}
