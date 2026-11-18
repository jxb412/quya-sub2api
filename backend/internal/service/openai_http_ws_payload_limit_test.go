package service

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"strings"
	"testing"

	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

func TestHTTPWSPolicyOAuthLargePayloadFallsBackBeforeDispatch(t *testing.T) {
	for _, scenario := range []struct {
		name  string
		input string
	}{
		{name: "large_input", input: strings.Repeat("a", 17<<20)},
		{name: "html_escaping", input: strings.Repeat("<", 3<<20)},
	} {
		t.Run(scenario.name, func(t *testing.T) {
			svc, account, upstream := newHTTPWSPolicyTestService(t, "https://unused.example")
			account.Type = AccountTypeOAuth
			account.Credentials = map[string]any{"access_token": "oauth-test-token", "chatgpt_account_id": "chatgpt-test-account"}
			account.Extra = map[string]any{"openai_oauth_responses_websockets_v2_mode": OpenAIWSIngressModeCtxPool}
			dialer := &httpWSTestDialer{}
			svc.getOpenAIWSConnPool().setClientDialerForTest(dialer)
			upstream.resp.Header.Set("Content-Type", "text/event-stream")
			upstream.resp.Body = io.NopCloser(strings.NewReader("data: {\"type\":\"response.completed\",\"response\":" + httpWSPolicyResponse + "}\n\n"))
			body := []byte(`{"model":"gpt-6.1-sol","stream":false,"instructions":"Answer briefly.","input":"` + scenario.input + `"}`)
			if scenario.name == "html_escaping" {
				require.Less(t, int64(len(body)), openAIHTTPWSCodexRequestLimitBytes, "only the actual WS encoding exceeds capacity")
			}
			c, recorder := newHTTPWSPolicyTestContext("/v1/responses")
			result, err := svc.Forward(context.Background(), c, account, body)
			require.NoError(t, err)
			require.NotNil(t, result)
			require.False(t, result.OpenAIWSMode)
			require.Equal(t, 12, result.Usage.InputTokens)
			require.Equal(t, 7, result.Usage.OutputTokens)
			require.Equal(t, http.StatusOK, recorder.Code)
			require.Contains(t, recorder.Body.String(), "hello from pool")
			require.Zero(t, dialer.count, "capacity fallback must happen before any WS handshake or write")
			require.Len(t, upstream.requests, 1, "exactly one HTTP generation is allowed")
			messages := gjson.GetBytes(upstream.lastBody, "input")
			require.True(t, messages.IsArray(), "OAuth normalizes a string input to a message array")
			require.Len(t, messages.Array(), 1)
			message := messages.Array()[0]
			require.Equal(t, "message", message.Get("type").String())
			require.Equal(t, "user", message.Get("role").String())
			require.Equal(t, gjson.String, message.Get("content").Type)
			gotInput := message.Get("content").String()
			require.Equal(t, len(scenario.input), len(gotInput))
			require.Equal(t, sha256.Sum256([]byte(scenario.input)), sha256.Sum256([]byte(gotInput)))
			require.False(t, gjson.GetBytes(upstream.lastBody, "type").Exists(), "WS response.create must not modify HTTP fallback body")
		})
	}
}

func TestOpenAIHTTPWSPoolCodexCapacityCountsMetadataAndNewline(t *testing.T) {
	payload := map[string]json.RawMessage{"type": json.RawMessage(`"response.create"`), "input": json.RawMessage(`""`)}
	var empty bytes.Buffer
	require.NoError(t, json.NewEncoder(&empty).Encode(payload))
	input := strings.Repeat("a", int(openAIHTTPWSCodexRequestLimitBytes)-empty.Len())
	payload["input"] = json.RawMessage(`"` + input + `"`)
	req, _ := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
	account := &Account{Platform: PlatformOpenAI, Type: AccountTypeOAuth}
	encoded, err := json.Marshal(payload)
	require.NoError(t, err)
	require.Equal(t, openAIHTTPWSCodexRequestLimitBytes-1, int64(len(encoded)), "Encode adds the final byte as a newline")
	require.NoError(t, checkOpenAIHTTPWSCodexPayloadSize(req, account, payload), "exactly 16 MiB on the wire is allowed")
	payload["input"] = json.RawMessage(`"` + input + `a"`)
	var capacityErr *openAIHTTPWSCodexCapacityError
	require.ErrorAs(t, checkOpenAIHTTPWSCodexPayloadSize(req, account, payload), &capacityErr)
	require.Equal(t, openAIHTTPWSCodexRequestLimitBytes+1, capacityErr.wireBytes)

	// A body that fits exactly before metadata is added must still fall back.
	dialer := &httpWSTestDialer{}
	svc, liveAccount := newHTTPWSTestService(t, dialer)
	req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":"`+input+`"}`)
	req.Header.Set(openAIWSTurnMetadataHeader, `{"turn_id":"current-turn"}`)
	resp, handled, err := svc.connectOpenAIHTTPWSPool(req, "", liveAccount, attempt)
	require.NoError(t, err)
	require.Nil(t, resp)
	require.False(t, handled)
	require.True(t, attempt.fallback.Load())
	require.False(t, attempt.sent.Load())
	require.Zero(t, dialer.count)
}

func TestOpenAIHTTPWSPoolCodexCapacityDoesNotFallbackForInvalidPayload(t *testing.T) {
	for name, body := range map[string]string{
		"bad_json":   `{"input":"` + strings.Repeat("a", 17<<20),
		"background": `{"input":"` + strings.Repeat("a", 17<<20) + `","background":true}`,
	} {
		t.Run(name, func(t *testing.T) {
			dialer := &httpWSTestDialer{}
			svc, account := newHTTPWSTestService(t, dialer)
			req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", body)
			_, handled, err := svc.connectOpenAIHTTPWSPool(req, "", account, attempt)
			require.Error(t, err)
			var capacityErr *openAIHTTPWSCodexCapacityError
			require.False(t, errors.As(err, &capacityErr))
			require.True(t, handled)
			require.False(t, attempt.fallback.Load())
			require.False(t, attempt.sent.Load())
			require.Zero(t, dialer.count)
		})
	}
}

func TestOpenAIHTTPWSPoolCodexCapacityIsEndpointSpecific(t *testing.T) {
	payload := map[string]json.RawMessage{"input": json.RawMessage(`"` + strings.Repeat("a", 17<<20) + `"`)}
	for _, target := range []string{"https://api.openai.com/v1/responses", "https://chatgpt.com.invalid/backend-api/codex/responses", "https://chatgpt.com:444/backend-api/codex/responses"} {
		req, err := http.NewRequest(http.MethodPost, target, nil)
		require.NoError(t, err)
		require.NoError(t, checkOpenAIHTTPWSCodexPayloadSize(req, &Account{Platform: PlatformOpenAI, Type: AccountTypeOAuth}, payload))
	}
	req, _ := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
	require.NoError(t, checkOpenAIHTTPWSCodexPayloadSize(req, &Account{Platform: PlatformOpenAI, Type: AccountTypeAPIKey}, payload))
}
