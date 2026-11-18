package service

import (
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
)

// 2026-11-19：Codex 上游可能因过大的生成帧关闭连接；请求一旦发送便不能安全重放。
// 因此将 16 MiB 作为该端点的保守发送预算，在发送前改走 HTTP，保留合法长请求。
// 此预算独立于接收事件上限，也不能套用到其他 Responses 兼容上游。
const openAIHTTPWSCodexRequestLimitBytes int64 = 16 << 20

// 2026-11-19：专用容量错误只在拨号及生成前产生，授权发送原 HTTP 请求；
// 读取失败、非法请求和已经发送的生成均不能借此触发回退重放。
type openAIHTTPWSCodexCapacityError struct{ wireBytes int64 }

func (e *openAIHTTPWSCodexCapacityError) Error() string {
	return "HTTP Responses payload exceeds Codex WebSocket capacity"
}

type openAIHTTPWSJSONByteCounter struct{ bytes int64 }

func (w *openAIHTTPWSJSONByteCounter) Write(p []byte) (int, error) {
	w.bytes += int64(len(p))
	return len(p), nil
}

func checkOpenAIHTTPWSCodexPayloadSize(req *http.Request, account *Account, payload map[string]json.RawMessage) error {
	if req == nil || req.URL == nil || account == nil || !account.IsOpenAIOAuthLike() ||
		req.URL.Scheme != "https" || !strings.EqualFold(req.URL.Hostname(), "chatgpt.com") ||
		(req.URL.Port() != "" && req.URL.Port() != "443") || req.URL.User != nil ||
		req.URL.Path != "/backend-api/codex/responses" {
		return nil
	}
	// 2026-11-19：必须与 coder/websocket/wsjson.Write 的编码一致，计入默认
	// HTML 转义和 Encode 末尾换行；检查对象已含 response.create 和当轮 metadata。
	// 仅统计原 HTTP body 会漏算转义膨胀及新增字段，导致判断通过后仍发送超大帧。
	var counter openAIHTTPWSJSONByteCounter
	if err := json.NewEncoder(&counter).Encode(payload); err != nil {
		return fmt.Errorf("encode HTTP Responses WebSocket payload: %w", err)
	}
	if counter.bytes > openAIHTTPWSCodexRequestLimitBytes {
		return &openAIHTTPWSCodexCapacityError{wireBytes: counter.bytes}
	}
	return nil
}
