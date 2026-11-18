package service

import (
	"encoding/json"
	"net/http"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestOpenAIWSHandshakeDiagnosticsClassifiesWithoutLeakingUpstreamValues(t *testing.T) {
	for _, tt := range []struct {
		name, server, contentType, body, wantContent, wantBody string
		cloudflare                                             bool
	}{
		{"edge rejection", "cloudflare", "text/plain; charset=utf-8", "", "text", "empty", true},
		{"API rejection", "private-server-secret", "application/json", `{"error":{"code":"secret-code","message":"secret-prompt"}}`, "json", "json", false},
		{"HTML challenge", "cloudflare", "text/html", "<!DOCTYPE html><html>secret-cookie</html>", "html", "html", true},
		{"unknown", "private-server-secret", "application/secret", "secret-body", "other", "other", false},
	} {
		t.Run(tt.name, func(t *testing.T) {
			headers := make(http.Header)
			headers.Set("Server", tt.server)
			headers.Set("Content-Type", tt.contentType)
			headers.Set("Set-Cookie", "secret-cookie")
			d := classifyOpenAIWSHandshakeResponse(headers, []byte(tt.body))
			require.Equal(t, tt.cloudflare, d.ServerCloudflare)
			require.Equal(t, tt.wantContent, d.ContentKind)
			require.Equal(t, tt.wantBody, d.BodyKind)
			require.Equal(t, len(tt.body), d.BodyBytes)
			encoded, err := json.Marshal(d)
			require.NoError(t, err)
			require.NotContains(t, string(encoded), "secret")
		})
	}
}
