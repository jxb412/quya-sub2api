package service

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/stretchr/testify/require"
)

type httpWSTestDialer struct {
	mu        sync.Mutex
	count     int
	conns     []*httpWSTestConn
	headers   []http.Header
	deadlines []time.Duration
	status    int
	errorBody string
	events    []string
	writeErr  error
	dialErr   error
}

func (d *httpWSTestDialer) Dial(ctx context.Context, _ string, headers http.Header, _ string) (openAIWSClientConn, int, http.Header, error) {
	d.mu.Lock()
	defer d.mu.Unlock()
	d.count++
	d.headers = append(d.headers, cloneHeader(headers))
	deadline, ok := ctx.Deadline()
	if ok {
		d.deadlines = append(d.deadlines, time.Until(deadline))
	} else {
		d.deadlines = append(d.deadlines, 0)
	}
	if d.dialErr != nil {
		return nil, 0, nil, d.dialErr
	}
	if d.status != 0 {
		return nil, d.status, http.Header{"Retry-After": {"12"}}, &openAIWSHandshakeError{Body: []byte(d.errorBody), Err: errors.New("handshake refused")}
	}
	c := &httpWSTestConn{events: d.events, writeErr: d.writeErr}
	d.conns = append(d.conns, c)
	return c, http.StatusSwitchingProtocols, http.Header{"X-Codex-Primary-Used-Percent": {"35"}, "Set-Cookie": {"session=isolated"}}, nil
}

type httpWSTestConn struct {
	mu       sync.Mutex
	writes   []map[string]json.RawMessage
	pending  []string
	events   []string
	closed   bool
	writeErr error
}

func (c *httpWSTestConn) WriteJSON(_ context.Context, value any) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return io.ErrClosedPipe
	}
	raw, err := json.Marshal(value)
	if err != nil {
		return err
	}
	var payload map[string]json.RawMessage
	if err := json.Unmarshal(raw, &payload); err != nil {
		return err
	}
	c.writes = append(c.writes, payload)
	if c.events != nil {
		c.pending = append([]string(nil), c.events...)
	} else {
		id := fmt.Sprintf("resp_%d", len(c.writes))
		c.pending = []string{
			fmt.Sprintf(`{"type":"response.created","response":{"id":%q,"status":"in_progress"}}`, id),
			fmt.Sprintf(`{"type":"response.output_text.delta","response_id":%q,"delta":"hello"}`, id),
			fmt.Sprintf(`{"type":"response.completed","response":{"id":%q,"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":7,"output_tokens":2}}}`, id),
		}
	}
	return c.writeErr
}

func (c *httpWSTestConn) ReadMessage(ctx context.Context) ([]byte, error) {
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return nil, io.ErrClosedPipe
	}
	if len(c.pending) == 0 {
		c.mu.Unlock()
		return nil, io.EOF
	}
	message := c.pending[0]
	c.pending = c.pending[1:]
	c.mu.Unlock()
	if message == "wait_for_cancel" {
		<-ctx.Done()
		return nil, ctx.Err()
	}
	return []byte(message), nil
}

func (c *httpWSTestConn) Ping(context.Context) error { return nil }
func (c *httpWSTestConn) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.closed = true
	return nil
}

func newHTTPWSTestService(t *testing.T, dialer *httpWSTestDialer) (*OpenAIGatewayService, *Account) {
	t.Helper()
	cfg := &config.Config{}
	cfg.Gateway.OpenAIWS.MaxConnsPerAccount = 1
	cfg.Gateway.OpenAIWS.MinIdlePerAccount = 0
	cfg.Gateway.OpenAIWS.MaxIdlePerAccount = 1
	cfg.Gateway.OpenAIWS.QueueLimitPerConn = 2
	cfg.Gateway.OpenAIWS.DialTimeoutSeconds = 5
	cfg.Gateway.OpenAIWS.ReadTimeoutSeconds = 2
	cfg.Gateway.OpenAIWS.WriteTimeoutSeconds = 2
	pool := newOpenAIWSConnPool(cfg)
	pool.setClientDialerForTest(dialer)
	t.Cleanup(pool.Close)
	return &OpenAIGatewayService{cfg: cfg, openaiWSPool: pool}, &Account{ID: 71, Platform: PlatformOpenAI, Type: AccountTypeOAuth, Concurrency: 1}
}

func newHTTPWSTestRequest(t *testing.T, scope, body string) (*http.Request, *openAIHTTPWSAttempt) {
	t.Helper()
	attempt := &openAIHTTPWSAttempt{scope: scope, groupID: 9}
	ctx := context.WithValue(context.Background(), openAIHTTPWSAttemptContextKey{}, attempt)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, "https://chatgpt.com/backend-api/codex/responses", strings.NewReader(body))
	require.NoError(t, err)
	req.Header.Set("Authorization", "Bearer test-token")
	req.Header.Set("Session_id", "thread-1")
	return req, attempt
}

func TestOpenAIHTTPWSPoolPreservesPayloadUsageAndReusesScopedConnection(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	for i := 0; i < 2; i++ {
		body := `{"model":"gpt-6.1-sol","stream":false,"input":[{"role":"user","content":"hi"}],"previous_response_id":"resp_previous","conversation":{"id":"conversation-1"},"reasoning":{"effort":"medium"}}`
		req, attempt := newHTTPWSTestRequest(t, "group:9/user:2/key:3/session:thread-1", body)
		resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
		require.NoError(t, err)
		require.Equal(t, http.StatusOK, resp.StatusCode)
		require.True(t, attempt.sent.Load())
		data, err := io.ReadAll(resp.Body)
		require.NoError(t, err)
		require.NoError(t, resp.Body.Close())
		require.Contains(t, string(data), `"output_tokens":2`)
		require.Contains(t, string(data), `"text":"hello"`)
		if i == 0 {
			require.Equal(t, "35", resp.Header.Get("X-Codex-Primary-Used-Percent"))
		} else {
			require.Empty(t, resp.Header.Get("X-Codex-Primary-Used-Percent"))
			require.Empty(t, resp.Header.Get("Set-Cookie"))
		}
	}
	require.Equal(t, 1, dialer.count)
	require.Len(t, dialer.conns[0].writes, 2)
	payload := dialer.conns[0].writes[0]
	require.NotContains(t, payload, "stream")
	require.JSONEq(t, `"response.create"`, string(payload["type"]))
	require.JSONEq(t, `"resp_previous"`, string(payload["previous_response_id"]))
	require.JSONEq(t, `{"id":"conversation-1"}`, string(payload["conversation"]))
}

func TestOpenAIHTTPWSPoolStrictIsolation(t *testing.T) {
	for _, change := range []string{"scope", "token", "cookie", "proxy", "url"} {
		t.Run(change, func(t *testing.T) {
			dialer := &httpWSTestDialer{}
			svc, account := newHTTPWSTestService(t, dialer)
			for i := 0; i < 2; i++ {
				scope, proxy := "key:1/session:1", "http://proxy-one.invalid:80"
				if i == 1 && change == "scope" {
					scope = "key:2/session:1"
				}
				req, _ := newHTTPWSTestRequest(t, scope, `{"model":"gpt-6.1-sol","input":[]}`)
				if i == 1 {
					switch change {
					case "token":
						req.Header.Set("Authorization", "Bearer changed-token")
					case "cookie":
						req.Header.Set("Cookie", "session=other")
					case "proxy":
						proxy = "http://proxy-two.invalid:80"
					case "url":
						req.URL.Host = "different.invalid"
					}
				}
				resp, err := svc.doOpenAIHTTPWSPool(req, proxy, account)
				require.NoError(t, err)
				_, err = io.ReadAll(resp.Body)
				require.NoError(t, err)
				require.NoError(t, resp.Body.Close())
			}
			require.Equal(t, 2, dialer.count)
		})
	}
}

func TestOpenAIHTTPWSPoolReturnsOriginal429BeforeDispatch(t *testing.T) {
	dialer := &httpWSTestDialer{status: http.StatusTooManyRequests, errorBody: `{"detail":"Rate limit exceeded"}`}
	svc, account := newHTTPWSTestService(t, dialer)
	req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
	resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
	require.NoError(t, err)
	require.Equal(t, http.StatusTooManyRequests, resp.StatusCode)
	require.Equal(t, "12", resp.Header.Get("Retry-After"))
	data, err := io.ReadAll(resp.Body)
	require.NoError(t, err)
	require.JSONEq(t, dialer.errorBody, string(data))
	require.False(t, attempt.sent.Load())
	require.Equal(t, 1, dialer.count)
}

func TestOpenAIHTTPWSPoolNetworkConnectRetriesBoundEachDial(t *testing.T) {
	dialer := &httpWSTestDialer{dialErr: errors.New("network connection refused")}
	svc, account := newHTTPWSTestService(t, dialer)
	// A longer native WS timeout must not override the HTTP adapter's budget.
	svc.cfg.Gateway.OpenAIWS.DialTimeoutSeconds = 60
	req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
	resp, handled, err := svc.connectOpenAIHTTPWSPool(req, "", account, attempt)
	require.NoError(t, err)
	require.Nil(t, resp)
	require.False(t, handled)
	require.True(t, attempt.fallback.Load())
	require.False(t, attempt.sent.Load())
	require.Equal(t, 4, dialer.count)
	require.Len(t, dialer.deadlines, 4)
	for _, remaining := range dialer.deadlines {
		require.Positive(t, remaining)
		require.LessOrEqual(t, remaining, 5*time.Second)
	}
}

func TestOpenAIHTTPWSPoolGenerationOutlivesConnectionDeadline(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	req, _ := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
	resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
	require.NoError(t, err)
	body, ok := resp.Body.(*openAIHTTPWSSSEBody)
	require.True(t, ok)
	_, hasDeadline := body.ctx.Deadline()
	require.False(t, hasDeadline, "connection deadline must not become the generation deadline")
	require.NoError(t, body.ctx.Err())
	_, err = io.ReadAll(resp.Body)
	require.NoError(t, err)
	require.NoError(t, resp.Body.Close())
}

func TestOpenAIHTTPWSPoolContinuationKeepsConnectionAcrossTraceIDs(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	for i := 0; i < 2; i++ {
		payload := `{"input":[]}`
		if i == 1 {
			payload = `{"input":[],"previous_response_id":"resp_1"}`
		}
		req, _ := newHTTPWSTestRequest(t, "key:1/session:1", payload)
		req.Header.Set("X-Client-Request-Id", fmt.Sprintf("trace-%d", i))
		resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
		require.NoError(t, err)
		_, err = io.ReadAll(resp.Body)
		require.NoError(t, err)
		require.NoError(t, resp.Body.Close())
	}
	require.Equal(t, 1, dialer.count)
	require.Len(t, dialer.conns[0].writes, 2)
	require.JSONEq(t, `"resp_1"`, string(dialer.conns[0].writes[1]["previous_response_id"]))
}

func TestOpenAIHTTPWSPoolEarlyCloseAndFailedTerminalAreNotReusable(t *testing.T) {
	for _, terminalFailure := range []bool{false, true} {
		t.Run(fmt.Sprintf("terminal_failure_%t", terminalFailure), func(t *testing.T) {
			dialer := &httpWSTestDialer{}
			if terminalFailure {
				dialer.events = []string{`{"type":"response.failed","response":{"id":"resp_error","status":"failed","error":{"code":"rate_limit_exceeded","message":"retry later"}}}`}
			}
			svc, account := newHTTPWSTestService(t, dialer)
			req, _ := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
			resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
			require.NoError(t, err)
			if terminalFailure {
				data, err := io.ReadAll(resp.Body)
				require.NoError(t, err)
				require.Contains(t, string(data), `"response.failed"`)
			}
			require.NoError(t, resp.Body.Close())
			require.True(t, dialer.conns[0].closed)
		})
	}
}

func TestOpenAIHTTPWSPoolDonePreservesWireEventAndRecordsActualTerminalStatus(t *testing.T) {
	for _, status := range []string{"completed", "failed", "incomplete", "cancelled", "canceled"} {
		t.Run(status, func(t *testing.T) {
			event := fmt.Sprintf(`{"type":"response.done","response":{"id":"resp_done","status":%q}}`, status)
			dialer := &httpWSTestDialer{events: []string{event}}
			svc, account := newHTTPWSTestService(t, dialer)
			req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
			resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
			require.NoError(t, err)
			data, err := io.ReadAll(resp.Body)
			require.NoError(t, err)
			require.NoError(t, resp.Body.Close())
			require.Equal(t, "data: "+event+"\n\n", string(data))
			expectedTerminal := "response." + status
			if status == "completed" {
				expectedTerminal = "response.done"
			}
			require.Equal(t, expectedTerminal, attempt.terminalEvent.Load())
			require.Equal(t, "resp_done", attempt.responseID.Load())
			result := &OpenAIForwardResult{OpenAIWSMode: true, UpstreamTerminalEvent: expectedTerminal}
			require.Equal(t, status == "completed", result.SucceededForScheduling())
			require.Equal(t, status != "completed", dialer.conns[0].closed)
		})
	}
}

func TestOpenAIHTTPWSPoolNeverResendsAmbiguousWriteOrRead(t *testing.T) {
	for _, writeFailure := range []bool{false, true} {
		t.Run(fmt.Sprintf("write_failure_%t", writeFailure), func(t *testing.T) {
			dialer := &httpWSTestDialer{events: []string{}}
			if writeFailure {
				dialer.writeErr = errors.New("write may have reached upstream")
			}
			svc, account := newHTTPWSTestService(t, dialer)
			req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
			resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
			if writeFailure {
				require.Error(t, err)
			} else {
				require.NoError(t, err)
				_, err = io.ReadAll(resp.Body)
				require.Error(t, err)
				_ = resp.Body.Close()
			}
			require.True(t, attempt.sent.Load())
			_, err = svc.doOpenAIHTTPWSPool(req, "", account)
			require.ErrorContains(t, err, "already dispatched")
			require.Equal(t, 1, dialer.count)
			require.Len(t, dialer.conns[0].writes, 1)
			require.True(t, dialer.conns[0].closed)
		})
	}
}

func TestOpenAIHTTPWSPoolRejectsCorruptTurnAndClosesConnection(t *testing.T) {
	cases := map[string][]string{
		"mismatched_response": {`{"type":"response.created","response":{"id":"resp_a","status":"in_progress"}}`, `{"type":"response.completed","response":{"id":"resp_b","status":"completed"}}`},
		"false_completed":     {`{"type":"response.completed","response":{"id":"resp_a","status":"in_progress"}}`},
		"missing_response":    {`{"type":"response.completed"}`},
		"trailing_document":   {`{"type":"response.completed","response":{"id":"resp_a","status":"completed"}}{"type":"response.output_text.delta","delta":"late"}`},
	}
	for name, events := range cases {
		t.Run(name, func(t *testing.T) {
			dialer := &httpWSTestDialer{events: events}
			svc, account := newHTTPWSTestService(t, dialer)
			req, _ := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
			resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
			require.NoError(t, err)
			_, err = io.ReadAll(resp.Body)
			require.Error(t, err)
			require.True(t, dialer.conns[0].closed)
		})
	}
}

func TestOpenAIHTTPWSPoolCloseCancelsBlockedRead(t *testing.T) {
	dialer := &httpWSTestDialer{events: []string{"wait_for_cancel"}}
	svc, account := newHTTPWSTestService(t, dialer)
	req, _ := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[]}`)
	resp, err := svc.doOpenAIHTTPWSPool(req, "", account)
	require.NoError(t, err)
	done := make(chan error, 1)
	go func() { _, readErr := io.ReadAll(resp.Body); done <- readErr }()
	require.NoError(t, resp.Body.Close())
	select {
	case err := <-done:
		require.Error(t, err)
	case <-time.After(time.Second):
		t.Fatal("closing body did not cancel read")
	}
	require.True(t, dialer.conns[0].closed)
}

func TestOpenAIHTTPWSPoolRejectsBackgroundBeforeDispatch(t *testing.T) {
	dialer := &httpWSTestDialer{}
	svc, account := newHTTPWSTestService(t, dialer)
	req, attempt := newHTTPWSTestRequest(t, "key:1/session:1", `{"input":[],"background":true}`)
	_, err := svc.doOpenAIHTTPWSPool(req, "", account)
	require.ErrorContains(t, err, "background")
	require.False(t, attempt.sent.Load())
	require.Zero(t, dialer.count)
}
