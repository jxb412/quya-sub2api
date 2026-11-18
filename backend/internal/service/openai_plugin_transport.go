package service

import "net/http"

func (s *OpenAIGatewayService) SetPluginManager(manager *PluginManager) {
	s.pluginManager = manager
}

// doOpenAIUpstream applies the explicit HTTP ctx_pool policy before dispatch.
// Otherwise, enabled OAuth plugins and the normal HTTP transport retain their
// existing routing. Every transport returns the same response parsing contract.
func (s *OpenAIGatewayService) doOpenAIUpstream(request *http.Request, proxyURL string, account *Account) (*http.Response, error) {
	if attempt := openAIHTTPWSAttemptFromContext(request.Context()); attempt != nil {
		response, handled, err := s.connectOpenAIHTTPWSPool(request, proxyURL, account, attempt)
		if handled {
			return response, err
		}
		// 2026-11-19：WS 恢复后若握手仍失败，HTTP 必须收到完整历史，
		// 不能把原 previous_response_id 的增量体送往另一条传输链路。
		request, err = cloneOpenAIHTTPWSFallbackRequest(request, proxyURL, account, attempt)
		if err != nil {
			return nil, err
		}
		// 2026-11-19：以真正发送的完整 body 建立有界恢复记录，兼容重试会替换旧记录。
		request = s.prepareOpenAIHTTPWSHTTPHistory(request, proxyURL, account, attempt)
	}
	if s.pluginManager != nil {
		// 插件需要客户端自报身份（例如 BPS 降温通道只放行官方 Codex 客户端）：出站身份
		// 收口已把 UA/originator 改写为网关规范身份，这里补私有头透传，未接管即撤回。
		restoreClientIdentity := attachPluginClientIdentity(request)
		response, handled, err := s.pluginManager.RoundTripOpenAIOAuth(request.Context(), request, proxyURL, account)
		restoreClientIdentity()
		if handled {
			return response, err
		}
	}
	response, err := s.httpUpstream.Do(request, proxyURL, account.ID, account.Concurrency)
	// 2026-11-19：部分 HTTPUpstream 未回填 Request 或丢失上下文；仅为 HTTP
	// 兜底补齐本次请求的恢复记录，普通 HTTP 不受影响。
	if replay, _ := request.Context().Value(openAIHTTPWSHTTPReplayContextKey{}).(*openAIHTTPWSHTTPReplay); response != nil && replay != nil {
		var returnedReplay *openAIHTTPWSHTTPReplay
		if response.Request != nil {
			returnedReplay, _ = response.Request.Context().Value(openAIHTTPWSHTTPReplayContextKey{}).(*openAIHTTPWSHTTPReplay)
		}
		if returnedReplay != replay {
			response.Request = request
		}
	}
	return response, err
}

// doOpenAIAccountTestUpstream 让 OpenAI OAuth 账号测试与真实转发使用同一插件路径。
// API Key 和未命中插件的账号保持各自原有的 HTTPUpstream 行为。
func (s *AccountTestService) doOpenAIAccountTestUpstream(
	request *http.Request,
	proxyURL string,
	account *Account,
	useTLSFallback bool,
) (*http.Response, error) {
	if s.pluginManager != nil {
		restoreClientIdentity := attachPluginClientIdentity(request)
		response, handled, err := s.pluginManager.RoundTripOpenAIOAuth(request.Context(), request, proxyURL, account)
		restoreClientIdentity()
		if handled {
			return response, err
		}
	}
	if useTLSFallback {
		return s.httpUpstream.DoWithTLS(
			request,
			proxyURL,
			account.ID,
			account.Concurrency,
			s.tlsFPProfileService.ResolveTLSProfile(account),
		)
	}
	return s.httpUpstream.Do(request, proxyURL, account.ID, account.Concurrency)
}
