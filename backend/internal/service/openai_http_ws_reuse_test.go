package service

import (
	"encoding/json"
	"fmt"
	"io"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestOpenAIHTTPWSPoolReusesConnectionWithCurrentTurnMetadata(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	for i := 0; i < 2; i++ {
		body := `{"input":[],"client_metadata":{"preserve":{"number":9007199254740993},"x-codex-turn-metadata":"stale"}}`
		if i == 1 {
			body = `{"input":[],"previous_response_id":"resp_1","client_metadata":{"preserve":{"number":9007199254740993},"x-codex-turn-metadata":"stale"}}`
		}
		req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", body)
		metadata := fmt.Sprintf(`{"turn_id":"turn-%d","turn_started_at_unix_ms":%d}`, i, i+10)
		req.Header.Set(openAIWSTurnMetadataHeader, metadata)
		req.Header.Set(openAICodexRoutingHintHeader, fmt.Sprintf("model=gpt-6.%d-sol", i+1))
		resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
		require.NoError(t, err)
		_, err = io.ReadAll(resp.Body)
		require.NoError(t, err)
		require.NoError(t, resp.Body.Close())
		require.Equal(t, i > 0, attempt.reused.Load())
		require.Equal(t, 1, dialer.count, "turn metadata and soft routing hints must not force a new handshake")
		payload := dialer.conns[0].writes[i]
		var clientMetadata map[string]json.RawMessage
		require.NoError(t, json.Unmarshal(payload["client_metadata"], &clientMetadata))
		require.JSONEq(t, `{"number":9007199254740993}`, string(clientMetadata["preserve"]))
		var actualMetadata string
		require.NoError(t, json.Unmarshal(clientMetadata[openAIWSTurnMetadataHeader], &actualMetadata))
		require.Equal(t, metadata, actualMetadata)
		original, err := req.GetBody()
		require.NoError(t, err)
		originalBody, err := io.ReadAll(original)
		require.NoError(t, err)
		require.NoError(t, original.Close())
		require.Equal(t, body, string(originalBody), "the HTTP fallback body must remain untouched")
	}
	require.Len(t, dialer.conns[0].writes, 2)
}

func TestOpenAIHTTPWSPoolTurnMetadataRejectsNonObjectClientMetadataBeforeDispatch(t *testing.T) {
	for _, metadata := range []string{`[]`, `"secret-client-value"`, `123`, `true`} {
		t.Run(metadata, func(t *testing.T) {
			dialer := &httpWSTestDialer{}
			svc, account := newHTTPWSTestService(t, dialer)
			req, attempt := newHTTPWSTestRequest(t, "scope", `{"input":[],"client_metadata":`+metadata+`}`)
			req.Header.Set(openAIWSTurnMetadataHeader, `{"turn_id":"new-turn"}`)
			response, handled, err := svc.connectOpenAIHTTPWSPool(req, "", account, attempt)
			require.ErrorContains(t, err, "client_metadata must be an object or null")
			require.NotContains(t, err.Error(), metadata)
			require.Nil(t, response)
			require.True(t, handled)
			require.False(t, attempt.sent.Load())
			require.False(t, attempt.fallback.Load())
			require.Zero(t, dialer.count)
		})
	}
}

func TestOpenAIHTTPWSTurnMetadataMatchesNativeMerge(t *testing.T) {
	for _, existing := range []string{"", `null`, `{}`, `{"keep":"value"}`} {
		t.Run(existing, func(t *testing.T) {
			payload := map[string]json.RawMessage{"input": json.RawMessage(`[]`)}
			native := map[string]any{"input": []any{}}
			if existing != "" {
				payload["client_metadata"] = json.RawMessage(existing)
				var parsed any
				require.NoError(t, json.Unmarshal([]byte(existing), &parsed))
				native["client_metadata"] = parsed
			}
			setOpenAIWSTurnMetadata(native, ` {"turn_id":"turn-1"} `)
			require.NoError(t, setOpenAIHTTPWSTurnMetadata(payload, ` {"turn_id":"turn-1"} `))
			expected, err := json.Marshal(native)
			require.NoError(t, err)
			actual, err := json.Marshal(payload)
			require.NoError(t, err)
			require.JSONEq(t, string(expected), string(actual))
		})
	}
}
