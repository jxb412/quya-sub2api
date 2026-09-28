package service

import (
	"context"
	"net/http"
	"strings"

	"github.com/Wei-Shaw/sub2api/internal/pkg/ctxkey"
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

// attachPluginClientIdentity 在把请求交给插件前写入客户端身份私有头，
// 返回恢复函数（插件未接管该请求时由调用方立即调用，避免私有头出站到上游）。
func attachPluginClientIdentity(request *http.Request) func() {
	noop := func() {}
	if request == nil {
		return noop
	}
	userAgent, originator := pluginClientIdentityFromContext(request.Context())
	if userAgent == "" && originator == "" {
		return noop
	}
	header := request.Header
	if header == nil {
		return noop
	}
	header.Set(PluginClientUserAgentHeader, userAgent)
	header.Set(PluginClientOriginatorHeader, originator)
	// 恢复即删除：这两个私有头只由本函数写入，入站白名单也不会透传同名客户端头。
	return func() {
		header.Del(PluginClientUserAgentHeader)
		header.Del(PluginClientOriginatorHeader)
	}
}
