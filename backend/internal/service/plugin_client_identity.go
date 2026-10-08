package service

import (
	"context"
	"net/http"
	"strings"

	"github.com/Wei-Shaw/sub2api/internal/pkg/ctxkey"
	"github.com/gin-gonic/gin"
	"github.com/tidwall/gjson"
)

// 插件私有透传头：把「客户端自报身份」交给 OAuth 出站传输插件。
//
// 背景：宿主在构造 ChatGPT Codex 出站请求时会强制统一身份
// （enforceCodexIdentityHeaders），User-Agent / originator 一律改写成网关规范 Codex
// 身份。插件因此无法从请求头判断真实客户端来源，而这是某些能力的前置条件 ——
// 例如 codex-native-transport 的 BPS 降温通道只允许官方 Codex 客户端命中（第三方
// 聊天客户端会被 BPS 上游注入的 Excel/Work 插件上下文盖掉客户自己的提问）。
//
// 这里只在「请求真的要交给插件」时补两个私有头，插件消费后必须剥离（插件侧
// ordered_headers 按 `x-sub2api-` 前缀整段剥离，BPS 通道与正常通道都不出站）：
//   - 非插件路径（插件未绑定该账号 / 插件不可用）会在回退前删掉，不会带着私有头出站；
//   - 头名会被 Go 规范化为 X-Sub2api-Client-*，插件侧按大小写不敏感读取。
const (
	// PluginClientUserAgentHeader 携带客户端自报 User-Agent。
	PluginClientUserAgentHeader = "x-sub2api-client-user-agent"
	// PluginClientOriginatorHeader 携带客户端自报 originator。
	PluginClientOriginatorHeader = "x-sub2api-client-originator"
	// PluginClientConversationSourceHeader 标识客户端是否显式携带稳定会话键。
	PluginClientConversationSourceHeader = "x-sub2api-client-conversation-source"
	// PluginClientConversationKeyHeader 携带按 API Key 隔离、但不含上游账号的稳定会话键。
	PluginClientConversationKeyHeader = "x-sub2api-client-conversation-key"
)

const (
	pluginClientConversationExplicit = "explicit"
	pluginClientConversationNone     = "none"
)

// pluginClientIdentityFromContext 读取中间件快照的客户端自报身份。
// 缺失（例如服务层单测、非 HTTP 入站）时返回空串，调用方跳过注入。
func pluginClientIdentityFromContext(ctx context.Context) (string, string) {
	if ctx == nil {
		return "", ""
	}
	userAgent, _ := ctx.Value(ctxkey.ClientUserAgent).(string)
	originator, _ := ctx.Value(ctxkey.ClientOriginator).(string)
	return strings.TrimSpace(userAgent), strings.TrimSpace(originator)
}

func pluginClientConversationFromContext(ctx context.Context) (string, string) {
	if ctx == nil {
		return "", ""
	}
	source, _ := ctx.Value(ctxkey.ClientConversationSource).(string)
	key, _ := ctx.Value(ctxkey.ClientConversationKey).(string)
	return strings.TrimSpace(source), strings.TrimSpace(key)
}

// withPluginClientConversationContext 在任何账号级身份/指纹改写前抓取客户端原始会话键。
// key 只混入 API Key ID，不混入上游账号；因此同一会员会话在调度换号后仍能命中
// previous_response_id 的正常通道 pin，同时不同 API Key 的相同原始 session 不碰撞。
func withPluginClientConversationContext(ctx context.Context, c *gin.Context, body []byte) context.Context {
	if ctx == nil {
		ctx = context.Background()
	}
	raw := resolvePluginClientConversationKey(c, body)
	source := pluginClientConversationNone
	key := ""
	if raw != "" {
		source = pluginClientConversationExplicit
		key = isolateOpenAISessionID(getAPIKeyIDFromContext(c), raw)
	}
	ctx = context.WithValue(ctx, ctxkey.ClientConversationSource, source)
	ctx = context.WithValue(ctx, ctxkey.ClientConversationKey, key)
	return ctx
}

// ensurePluginClientConversationContext 只在尚未抓取时写入会话快照。调度 failover
// 会重复调用 Forward，必须始终保留第一次看到的客户端原始请求，不能把后续账号
// 已经改写过的 prompt_cache_key 当成真实客户端会话键。
func ensurePluginClientConversationContext(ctx context.Context, c *gin.Context, body []byte) context.Context {
	if ctx != nil {
		if source, ok := ctx.Value(ctxkey.ClientConversationSource).(string); ok && strings.TrimSpace(source) != "" {
			return ctx
		}
	}
	return withPluginClientConversationContext(ctx, c, body)
}

func resolvePluginClientConversationKey(c *gin.Context, body []byte) string {
	if value := pluginConversationJSONKey(body, "prompt_cache_key"); value != "" {
		return value
	}
	for _, path := range []string{
		"client_metadata.session_id",
		"client_metadata.session-id",
		"client_metadata.thread_id",
		"client_metadata.thread-id",
		"client_metadata.x-codex-window-id",
	} {
		if value := pluginConversationJSONKey(body, path); value != "" {
			return value
		}
	}
	if raw := pluginConversationJSONKey(body, "client_metadata.x-codex-turn-metadata"); raw != "" {
		metadata := gjson.Parse(raw)
		for _, name := range []string{"session_id", "session-id", "thread_id", "thread-id", "window_id"} {
			if value := pluginConversationResultString(metadata.Get(name)); value != "" {
				return value
			}
		}
	}
	if c == nil || c.Request == nil {
		return ""
	}
	for _, name := range []string{
		"session-id",
		"session_id",
		"thread-id",
		"thread_id",
		"conversation-id",
		"conversation_id",
		"x-codex-window-id",
	} {
		if value := strings.TrimSpace(c.Request.Header.Get(name)); value != "" {
			return value
		}
	}
	return ""
}

func pluginConversationJSONKey(body []byte, path string) string {
	return pluginConversationResultString(gjson.GetBytes(body, path))
}

func pluginConversationResultString(result gjson.Result) string {
	if !result.Exists() || result.Type != gjson.String {
		return ""
	}
	return strings.TrimSpace(result.String())
}

// attachPluginClientIdentity 在把请求交给插件前写入客户端身份私有头，
// 返回恢复函数（插件未接管该请求时由调用方立即调用，避免私有头出站到上游）。
func attachPluginClientIdentity(request *http.Request) func() {
	noop := func() {}
	if request == nil {
		return noop
	}
	userAgent, originator := pluginClientIdentityFromContext(request.Context())
	conversationSource, conversationKey := pluginClientConversationFromContext(request.Context())
	if userAgent == "" && originator == "" && conversationSource == "" && conversationKey == "" {
		return noop
	}
	header := request.Header
	if header == nil {
		return noop
	}
	if userAgent != "" {
		header.Set(PluginClientUserAgentHeader, userAgent)
	}
	if originator != "" {
		header.Set(PluginClientOriginatorHeader, originator)
	}
	if conversationSource != "" {
		header.Set(PluginClientConversationSourceHeader, conversationSource)
	}
	if conversationKey != "" {
		header.Set(PluginClientConversationKeyHeader, conversationKey)
	}
	// 恢复即删除：这些私有头只由本函数写入，入站白名单也不会透传同名客户端头。
	return func() {
		header.Del(PluginClientUserAgentHeader)
		header.Del(PluginClientOriginatorHeader)
		header.Del(PluginClientConversationSourceHeader)
		header.Del(PluginClientConversationKeyHeader)
	}
}
