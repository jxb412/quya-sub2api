package service

import (
	"context"
	"net/http"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/pkg/ctxkey"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
)

func pluginConversationTestContext(t *testing.T, apiKeyID int64) *gin.Context {
	t.Helper()
	gin.SetMode(gin.TestMode)
	c, _ := gin.CreateTestContext(nil)
	c.Request, _ = http.NewRequest(http.MethodPost, "/v1/responses", nil)
	c.Set("api_key", &APIKey{ID: apiKeyID})
	return c
}

func TestWithPluginClientConversationContextExplicitIsStableAndAPIKeyScoped(t *testing.T) {
	body := []byte(`{"prompt_cache_key":"shared-session","input":"hi"}`)
	c1 := pluginConversationTestContext(t, 101)
	ctx1 := withPluginClientConversationContext(context.Background(), c1, body)
	require.Equal(t, pluginClientConversationExplicit, ctx1.Value(ctxkey.ClientConversationSource))
	key1, _ := ctx1.Value(ctxkey.ClientConversationKey).(string)
	require.NotEmpty(t, key1)

	c2 := pluginConversationTestContext(t, 101)
	ctx2 := withPluginClientConversationContext(context.Background(), c2, body)
	require.Equal(t, key1, ctx2.Value(ctxkey.ClientConversationKey))

	c3 := pluginConversationTestContext(t, 202)
	ctx3 := withPluginClientConversationContext(context.Background(), c3, body)
	require.NotEqual(t, key1, ctx3.Value(ctxkey.ClientConversationKey))
}

func TestWithPluginClientConversationContextNoneIgnoresMissingSession(t *testing.T) {
	c := pluginConversationTestContext(t, 101)
	ctx := withPluginClientConversationContext(context.Background(), c, []byte(`{"input":"hi"}`))
	require.Equal(t, pluginClientConversationNone, ctx.Value(ctxkey.ClientConversationSource))
	require.Equal(t, "", ctx.Value(ctxkey.ClientConversationKey))
}

func TestEnsurePluginClientConversationContextPreservesFirstSnapshot(t *testing.T) {
	c := pluginConversationTestContext(t, 101)
	ctx := ensurePluginClientConversationContext(context.Background(), c, []byte(`{"input":"hi"}`))
	require.Equal(t, pluginClientConversationNone, ctx.Value(ctxkey.ClientConversationSource))

	// 模拟第一次调度后账号身份层注入了固定 prompt_cache_key。failover 再进入
	// Forward 时必须保留 none，而不是把账号级 key 误认成客户端显式会话。
	again := ensurePluginClientConversationContext(ctx, c, []byte(`{"prompt_cache_key":"account-fixed"}`))
	require.Equal(t, pluginClientConversationNone, again.Value(ctxkey.ClientConversationSource))
	require.Equal(t, "", again.Value(ctxkey.ClientConversationKey))
}

func TestResolvePluginClientConversationKeyPrecedence(t *testing.T) {
	c := pluginConversationTestContext(t, 1)
	c.Request.Header.Set("session-id", "header-session")
	body := []byte(`{"prompt_cache_key":"body-cache","client_metadata":{"session_id":"body-session"}}`)
	require.Equal(t, "body-cache", resolvePluginClientConversationKey(c, body))

	body = []byte(`{"client_metadata":{"x-codex-turn-metadata":"{\"thread_id\":\"nested-thread\"}"}}`)
	require.Equal(t, "nested-thread", resolvePluginClientConversationKey(c, body))

	require.Equal(t, "header-session", resolvePluginClientConversationKey(c, []byte(`{"input":"hi"}`)))
}

func TestAttachPluginClientIdentityAddsAndRemovesPrivateConversationHeaders(t *testing.T) {
	ctx := context.WithValue(context.Background(), ctxkey.ClientConversationSource, pluginClientConversationExplicit)
	ctx = context.WithValue(ctx, ctxkey.ClientConversationKey, "isolated-member-key")
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, "https://example.com", nil)
	require.NoError(t, err)

	restore := attachPluginClientIdentity(req)
	require.Equal(t, pluginClientConversationExplicit, req.Header.Get(PluginClientConversationSourceHeader))
	require.Equal(t, "isolated-member-key", req.Header.Get(PluginClientConversationKeyHeader))
	restore()
	require.Empty(t, req.Header.Get(PluginClientConversationSourceHeader))
	require.Empty(t, req.Header.Get(PluginClientConversationKeyHeader))
}
