// 插件验收 harness：以与 Sub2API 宿主完全相同的 go-plugin 客户端配置拉起插件，
// 验证握手、GetInfo/Health、配置校验/应用、以及一次完整的 Forward 流式转发。
//
// 用法: go run . <plugin-binary-path> [target-url]
// target-url 缺省为本地起的 HTTP 测试服务器（含 SSE 分块响应）。
package main

import (
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"strings"
	"time"

	pluginv1 "github.com/Wei-Shaw/sub2api/pkg/pluginapi/v1"
	hclog "github.com/hashicorp/go-hclog"
	hcplugin "github.com/hashicorp/go-plugin"
)

func main() {
	if len(os.Args) < 2 {
		log.Fatal("usage: harness <plugin-binary> [target-url]")
	}
	binaryPath := os.Args[1]

	binary, err := os.ReadFile(binaryPath)
	must(err, "read plugin binary")
	checksum := sha256.Sum256(binary)

	// 与 startPluginRuntime 相同的客户端配置。
	cmd := exec.Command(binaryPath)
	client := hcplugin.NewClient(&hcplugin.ClientConfig{
		HandshakeConfig:  pluginv1.HandshakeConfig,
		Plugins:          pluginv1.ClientPluginMap(),
		Cmd:              cmd,
		AllowedProtocols: []hcplugin.Protocol{hcplugin.ProtocolGRPC},
		StartTimeout:     15 * time.Second,
		SecureConfig: &hcplugin.SecureConfig{
			Checksum: checksum[:],
			Hash:     sha256.New(),
		},
		Logger:           hclog.NewNullLogger(),
		SyncStdout:       io.Discard,
		SyncStderr:       io.Discard,
		UnixSocketConfig: &hcplugin.UnixSocketConfig{TempDir: os.TempDir()},
		SkipHostEnv:      true,
	})
	defer client.Kill()

	rpcClient, err := client.Client()
	must(err, "handshake / start plugin")
	fmt.Println("✓ go-plugin 握手成功")

	dispensed, err := rpcClient.Dispense(pluginv1.TransportPluginName)
	must(err, "dispense transport plugin")
	api := dispensed.(pluginv1.TransportPluginClient)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	info, err := api.GetInfo(ctx, &pluginv1.GetInfoRequest{})
	must(err, "GetInfo")
	fmt.Printf("✓ GetInfo: id=%s version=%s protocol=%d transport_api=%d caps=%v\n",
		info.PluginId, info.PluginVersion, info.ProtocolVersion, info.TransportApiVersion, info.Capabilities)
	if info.ProtocolVersion != pluginv1.ProtocolVersion || info.TransportApiVersion != pluginv1.TransportAPIVersion {
		log.Fatal("协议版本不匹配")
	}

	health, err := api.Health(ctx, &pluginv1.HealthRequest{})
	must(err, "Health")
	fmt.Printf("✓ Health: healthy=%v\n", health.Healthy)

	// 配置校验：空配置 → 完整默认值；非法配置 → 拒绝。
	// 可通过 HARNESS_CONFIG 覆盖测试配置（默认空 = 插件默认值）。
	harnessConfig := os.Getenv("HARNESS_CONFIG")
	if harnessConfig == "" {
		harnessConfig = "{}"
	}
	validation, err := api.ValidateConfig(ctx, &pluginv1.ValidateConfigRequest{ConfigJson: []byte(harnessConfig)})
	must(err, "ValidateConfig")
	if !validation.Valid {
		log.Fatalf("空配置应通过校验: %s", validation.Message)
	}
	fmt.Printf("✓ ValidateConfig 规范化: %s\n", truncate(string(validation.NormalizedConfigJson), 120))

	invalid, err := api.ValidateConfig(ctx, &pluginv1.ValidateConfigRequest{ConfigJson: []byte(`{"bogus":1}`)})
	must(err, "ValidateConfig(bogus)")
	if invalid.Valid {
		log.Fatal("未知字段应被拒绝")
	}
	fmt.Println("✓ 未知字段被拒绝")

	applied, err := api.ApplyConfig(ctx, &pluginv1.ApplyConfigRequest{ConfigJson: validation.NormalizedConfigJson})
	must(err, "ApplyConfig")
	if !applied.Applied {
		log.Fatalf("配置应用失败: %s", applied.Message)
	}
	fmt.Println("✓ ApplyConfig")

	// Forward 全流程：本地 HTTP 服务器返回分块 SSE。
	targetURL := ""
	if len(os.Args) > 2 {
		targetURL = os.Args[2]
	} else {
		server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			body, _ := io.ReadAll(r.Body)
			fmt.Printf("  · 上游收到: %s %s UA=%q originator=%q body=%dB\n",
				r.Method, r.URL.Path, r.Header.Get("User-Agent"), r.Header.Get("Originator"), len(body))
			w.Header().Set("Content-Type", "text/event-stream")
			w.WriteHeader(200)
			flusher := w.(http.Flusher)
			for i := 0; i < 3; i++ {
				fmt.Fprintf(w, "data: chunk-%d\n\n", i)
				flusher.Flush()
				time.Sleep(30 * time.Millisecond)
			}
		}))
		defer server.Close()
		targetURL = server.URL + "/backend-api/codex/responses"
	}

	req, err := http.NewRequestWithContext(ctx, "POST", targetURL, strings.NewReader(
		`{"model":"gpt-5","instructions":"x","input":[],"stream":true,"client_metadata":{"x-codex-installation-id":"d322e342-3394-4ba6-abc2-a7df70b78af1","session_id":"s"}}`))
	must(err, "build request")
	// 模拟宿主转发的完整头集合（乱序 + 混入宿主注入的 version/accept-encoding/cookie，
	// 用于验证插件的重排与剥除是否与真实 codex 0.153.4 抓包一致）。
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("User-Agent", "codex_cli_rs/0.153.4 (Ubuntu 22.4.0; x86_64) xterm-256color")
	req.Header.Set("Originator", "codex_cli_rs")
	req.Header.Set("Accept", "text/event-stream")
	req.Header.Set("Authorization", "Bearer dummy")
	req.Header.Set("chatgpt-account-id", "acc-1")
	req.Header.Set("version", "0.50.0")
	req.Header.Set("Accept-Encoding", "gzip")
	req.Header.Set("Cookie", "__cf_bm=stale")
	req.Header.Set("session-id", "01a08a14-b6b3-7940-9615-8a8a984fbc76")
	req.Header.Set("thread-id", "01a08a14-b6b3-7940-9615-8a8a984fbc76")
	req.Header.Set("x-client-request-id", "01a08a14-b6b3-7940-9615-8a8a984fbc76")
	req.Header.Set("x-codex-beta-features", "remote_compaction_v2")
	req.Header.Set("x-codex-window-id", "01a08a14-b6b3-7940-9615-8a8a984fbc76:0")
	req.Header.Set("x-codex-turn-metadata", `{"installation_id":"d322e342-3394-4ba6-abc2-a7df70b78af1","session_id":"01a08a14-b6b3-7940-9615-8a8a984fbc76","window_id":"01a08a14-b6b3-7940-9615-8a8a984fbc76:0","request_kind":"turn"}`)
	req.Header.Set("x-codex-turn-state", "dirty-turn-state")

	resp, err := roundTripViaPlugin(ctx, api, req)
	must(err, "Forward round trip")
	defer resp.Body.Close()
	payload, err := io.ReadAll(resp.Body)
	must(err, "read response body")
	fmt.Printf("✓ Forward: status=%d proto=%s bytes=%d\n", resp.StatusCode, resp.Proto, len(payload))
	if !strings.Contains(string(payload), "chunk-2") {
		log.Fatalf("响应体不完整: %q", string(payload))
	}
	fmt.Println("✓ SSE 响应体完整")
	fmt.Println("ALL PASS")
}

// roundTripViaPlugin 复刻宿主 pluginRuntime.roundTrip 的帧序列。
func roundTripViaPlugin(ctx context.Context, api pluginv1.TransportPluginClient, request *http.Request) (*http.Response, error) {
	streamCtx, cancel := context.WithCancel(ctx)
	stream, err := api.Forward(streamCtx)
	if err != nil {
		cancel()
		return nil, err
	}
	headers := map[string]*pluginv1.HeaderValues{}
	for key, values := range request.Header {
		headers[key] = &pluginv1.HeaderValues{Values: values}
	}
	if err := stream.Send(&pluginv1.ForwardRequest{Frame: &pluginv1.ForwardRequest_Start{Start: &pluginv1.ForwardRequestStart{
		RequestId:   "harness-1",
		Method:      request.Method,
		Url:         request.URL.String(),
		Host:        request.Host,
		Headers:     headers,
		AccountId:   42,
		Platform:    "openai",
		AccountType: "oauth",
		HasBody:     request.Body != nil,
	}}}); err != nil {
		cancel()
		return nil, err
	}
	if request.Body != nil {
		payload, err := io.ReadAll(request.Body)
		if err != nil {
			cancel()
			return nil, err
		}
		if err := stream.Send(&pluginv1.ForwardRequest{Frame: &pluginv1.ForwardRequest_BodyChunk{BodyChunk: payload}}); err != nil {
			cancel()
			return nil, err
		}
	}
	if err := stream.Send(&pluginv1.ForwardRequest{Frame: &pluginv1.ForwardRequest_BodyEnd{BodyEnd: true}}); err != nil {
		cancel()
		return nil, err
	}
	if err := stream.CloseSend(); err != nil {
		cancel()
		return nil, err
	}

	first, err := stream.Recv()
	if err != nil {
		cancel()
		return nil, err
	}
	if frameError := first.GetError(); frameError != nil {
		cancel()
		return nil, fmt.Errorf("plugin error [%s] request_sent=%v: %s", frameError.Code, frameError.RequestSent, frameError.Message)
	}
	start := first.GetStart()
	if start == nil {
		cancel()
		return nil, fmt.Errorf("expected start frame")
	}
	reader, writer := io.Pipe()
	go func() {
		defer cancel()
		defer writer.Close()
		for {
			frame, err := stream.Recv()
			if err == io.EOF {
				return
			}
			if err != nil {
				_ = writer.CloseWithError(err)
				return
			}
			if chunk := frame.GetBodyChunk(); len(chunk) > 0 {
				if _, err := writer.Write(chunk); err != nil {
					return
				}
			}
			if frame.GetEnd() != nil {
				return
			}
			if frameError := frame.GetError(); frameError != nil {
				_ = writer.CloseWithError(fmt.Errorf("plugin error [%s]: %s", frameError.Code, frameError.Message))
				return
			}
		}
	}()
	header := http.Header{}
	for key, values := range start.Headers {
		if values != nil {
			header[key] = values.Values
		}
	}
	return &http.Response{
		StatusCode:    int(start.StatusCode),
		Status:        start.Status,
		Proto:         start.Protocol,
		ProtoMajor:    int(start.ProtocolMajor),
		ProtoMinor:    int(start.ProtocolMinor),
		Header:        header,
		Body:          reader,
		ContentLength: start.ContentLength,
	}, nil
}

func must(err error, what string) {
	if err != nil {
		log.Fatalf("FAIL %s: %v", what, err)
	}
}

func truncate(value string, limit int) string {
	if len(value) <= limit {
		return value
	}
	return value[:limit] + "…"
}
