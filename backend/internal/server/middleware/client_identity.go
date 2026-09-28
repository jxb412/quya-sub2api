package middleware

import (
	"context"
	"strings"

	"github.com/Wei-Shaw/sub2api/internal/pkg/ctxkey"
	"github.com/gin-gonic/gin"
)

// clientIdentityOriginatorHeader 是客户端自报中转方标识（codex-rs 用 clientInfo.name）。
const clientIdentityOriginatorHeader = "originator"

// ClientIdentityContext 全局中间件：在出站身份收口之前，把「客户端自报身份」
// （User-Agent / originator）抓进 request context。
//
// 为什么需要：宿主在构造 ChatGPT Codex 出站请求时会强制统一身份
// （enforceCodexIdentityHeaders：指纹收敛 / 规范 UA），请求头里的 User-Agent 与
// originator 都会变成网关规范 Codex 身份。依赖「客户端来源」的插件（例如
// codex-native-transport 的 BPS 降温通道只放行官方 Codex 客户端）如果只读请求头，
// 就会把每个客户端都当成官方 Codex 客户端。这里留下原始值，
// service.attachPluginClientIdentity 在把请求交给插件前写成 x-sub2api-client-* 私有头，
// 插件判定完必须剥离，绝不出站。
//
// 必须在 SessionBindingContext 之后注册（那里已经把 UA 归一化写回请求头）。
func ClientIdentityContext() gin.HandlerFunc {
	return func(c *gin.Context) {
		if c.Request == nil {
			c.Next()
			return
		}
		userAgent := normalizePersistentText(c.Request.Header.Get("User-Agent"), maxPersistentUserAgentBytes)
		originator := normalizePersistentText(c.Request.Header.Get(clientIdentityOriginatorHeader), maxPersistentUserAgentBytes)
		if strings.TrimSpace(userAgent) == "" && strings.TrimSpace(originator) == "" {
			c.Next()
			return
		}
		ctx := context.WithValue(c.Request.Context(), ctxkey.ClientUserAgent, userAgent)
		ctx = context.WithValue(ctx, ctxkey.ClientOriginator, originator)
		c.Request = c.Request.WithContext(ctx)
		c.Next()
	}
}
