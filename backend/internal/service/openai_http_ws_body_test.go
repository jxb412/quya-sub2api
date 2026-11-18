package service

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"testing/iotest"

	"github.com/gorilla/websocket"
	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

func TestHTTPWSPolicyForwardLargePreparedBody(t *testing.T) {
	input := strings.Repeat("long conversation ", 1<<20)
	body, err := json.Marshal(map[string]any{
		"model": "gpt-6.1-sol", "stream": false, "input": input,
	})
	require.NoError(t, err)
	require.Greater(t, int64(len(body)), openAIWSMessageReadLimitBytes)
	require.Less(t, len(body), 32<<20, "the body fits the normal HTTP text request limit")

	var generations atomic.Int32
	requests := make(chan []byte, 1)
	upgrader := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		defer func() { _ = conn.Close() }()
		_, payload, err := conn.ReadMessage()
		if err != nil {
			return
		}
		generations.Add(1)
		requests <- payload
		_ = writeHTTPWSPolicySuccess(conn)
	}))
	defer server.Close()
	svc, account, upstream := newHTTPWSPolicyTestService(t, server.URL)
	svc.cfg.Gateway.MaxBodySize = 256 << 20
	svc.cfg.Gateway.TextMaxBodySize = 32 << 20
	c, recorder := newHTTPWSPolicyTestContext("/v1/responses")
	result, err := svc.Forward(context.Background(), c, account, body)
	require.NoError(t, err)
	require.NotNil(t, result)
	require.True(t, result.OpenAIWSMode)
	require.Equal(t, "response.completed", result.UpstreamTerminalEvent)
	require.Equal(t, 12, result.Usage.InputTokens)
	require.Equal(t, 7, result.Usage.OutputTokens)
	require.Equal(t, http.StatusOK, recorder.Code)
	require.Contains(t, recorder.Body.String(), "hello from pool")
	require.EqualValues(t, 1, generations.Load())
	require.Empty(t, upstream.requests, "a valid large request must use WS without a second HTTP generation")
	select {
	case payload := <-requests:
		require.Equal(t, "response.create", gjson.GetBytes(payload, "type").String())
		gotInput := gjson.GetBytes(payload, "input").String()
		require.Equal(t, len(input), len(gotInput))
		require.Equal(t, sha256.Sum256([]byte(input)), sha256.Sum256([]byte(gotInput)), "long input must arrive intact")
	default:
		t.Fatal("the upstream did not receive the large WS request")
	}
}

func TestOpenAIHTTPWSPoolBodyReadFailureNeverFallsBack(t *testing.T) {
	for _, failOpening := range []bool{false, true} {
		t.Run(map[bool]string{false: "read", true: "open"}[failOpening], func(t *testing.T) {
			dialer := &httpWSTestDialer{}
			svc, account := newHTTPWSTestService(t, dialer)
			req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
			readErr := errors.New("prepared body read failed")
			getBodyCalls := 0
			req.GetBody = func() (io.ReadCloser, error) {
				getBodyCalls++
				if failOpening {
					return nil, readErr
				}
				return io.NopCloser(io.MultiReader(strings.NewReader(`{"input":[]}`), iotest.ErrReader(readErr))), nil
			}
			resp, handled, err := svc.connectOpenAIHTTPWSPool(req, "", account, attempt)
			require.ErrorIs(t, err, readErr)
			require.Nil(t, resp)
			require.True(t, handled, "a body read error must stop dispatch, not request HTTP fallback")
			require.True(t, attempt.used.Load())
			require.False(t, attempt.sent.Load())
			require.False(t, attempt.fallback.Load())
			require.Equal(t, 1, getBodyCalls, "a read failure must not be retried as a connection failure")
			require.Zero(t, dialer.count)
		})
	}
}
