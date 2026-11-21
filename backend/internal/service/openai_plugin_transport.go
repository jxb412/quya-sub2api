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
	return s.httpUpstream.Do(request, proxyURL, account.ID, account.Concurrency)
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
