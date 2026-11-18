package service

import (
	"container/list"
	"encoding/json"
	"strings"
	"sync"
	"time"
)

// 2026-11-19：HTTP 转 WS 的独立请求缺少原生 WS 会话保留的完整历史，
// 导致连接回收后只能拒绝续接。此进程内缓存只接收调用方已证明完整的
// input + output，供同一硬身份安全恢复；不修改 Redis/数据库及原 WS 状态接口。
const (
	openAIHTTPWSReplayMaxBytes      int64 = 64 << 20
	openAIHTTPWSReplayMaxEntryBytes int64 = 16 << 20
	openAIHTTPWSReplayMaxEntries          = 512
	openAIHTTPWSReplayMaxTTL              = 15 * time.Minute
	// 预算包括原始 JSON 字节、RawMessage 切片项及保守的条目/索引开销。
	openAIHTTPWSReplayItemOverhead  int64 = 32
	openAIHTTPWSReplayEntryOverhead int64 = 256
)

type openAIHTTPWSReplayKey struct {
	identity   string
	responseID string
}

type openAIHTTPWSReplayEntry struct {
	key       openAIHTTPWSReplayKey
	items     []json.RawMessage
	bytes     int64
	expiresAt time.Time
}

type openAIHTTPWSReplayStore struct {
	mu            sync.Mutex
	entries       map[openAIHTTPWSReplayKey]*list.Element
	lru           list.List
	bytes         int64
	maxBytes      int64
	maxEntryBytes int64
	maxEntries    int
	now           func() time.Time
}

func newOpenAIHTTPWSReplayStore() *openAIHTTPWSReplayStore {
	return &openAIHTTPWSReplayStore{
		entries:       make(map[openAIHTTPWSReplayKey]*list.Element),
		maxBytes:      openAIHTTPWSReplayMaxBytes,
		maxEntryBytes: openAIHTTPWSReplayMaxEntryBytes,
		maxEntries:    openAIHTTPWSReplayMaxEntries,
		now:           time.Now,
	}
}

func (s *OpenAIGatewayService) getOpenAIHTTPWSReplayStore() *openAIHTTPWSReplayStore {
	if s == nil {
		return nil
	}
	s.openaiHTTPWSReplayStoreOnce.Do(func() {
		if s.openaiHTTPWSReplayStore == nil {
			s.openaiHTTPWSReplayStore = newOpenAIHTTPWSReplayStore()
		}
	})
	return s.openaiHTTPWSReplayStore
}

// 2026-11-19：按调用方提供的硬身份和响应 ID 查找完整历史；过期不返回，
// 命中仅更新 LRU，不延长 TTL。返回深拷贝，避免重试的输入改写污染其他请求。
func (s *openAIHTTPWSReplayStore) Get(identity, responseID string) ([]json.RawMessage, bool) {
	if s == nil {
		return nil, false
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	element, ok := s.entries[openAIHTTPWSReplayKey{identity: identity, responseID: responseID}]
	if !ok {
		return nil, false
	}
	entry, _ := element.Value.(*openAIHTTPWSReplayEntry)
	if !s.now().Before(entry.expiresAt) {
		s.remove(element)
		return nil, false
	}
	s.lru.MoveToFront(element)
	return cloneOpenAIHTTPWSReplayItems(entry.items), true
}

// 2026-11-19：仅保存调用方确认完整且成功结束的历史。用实际字节及条目开销
// 执行单条/总量/数量三重上限，先清理过期项再淘汰最久未使用项。
// 入库深拷贝每个 item，禁止小切片长期引用整份大请求的 backing array。
func (s *openAIHTTPWSReplayStore) Put(identity, responseID string, items []json.RawMessage, ttl time.Duration) bool {
	if s == nil || strings.TrimSpace(identity) == "" || strings.TrimSpace(responseID) == "" || ttl <= 0 {
		return false
	}
	if ttl > openAIHTTPWSReplayMaxTTL {
		ttl = openAIHTTPWSReplayMaxTTL
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.maxEntries <= 0 || s.maxBytes <= 0 || s.maxEntryBytes <= 0 {
		return false
	}
	size := openAIHTTPWSReplayEntryOverhead + int64(len(identity)) + int64(len(responseID))
	for _, item := range items {
		itemBytes := int64(len(item)) + openAIHTTPWSReplayItemOverhead
		if size > s.maxEntryBytes || itemBytes > s.maxEntryBytes-size {
			return false
		}
		size += itemBytes
	}
	if size > s.maxEntryBytes || size > s.maxBytes {
		return false
	}
	now := s.now()
	for _, element := range s.entries {
		entry, _ := element.Value.(*openAIHTTPWSReplayEntry)
		if !now.Before(entry.expiresAt) {
			s.remove(element)
		}
	}
	lookupKey := openAIHTTPWSReplayKey{identity: identity, responseID: responseID}
	if existing := s.entries[lookupKey]; existing != nil {
		s.remove(existing)
	}
	for s.lru.Len() >= s.maxEntries || s.bytes > s.maxBytes-size {
		s.remove(s.lru.Back())
	}
	key := openAIHTTPWSReplayKey{identity: strings.Clone(identity), responseID: strings.Clone(responseID)}
	entry := &openAIHTTPWSReplayEntry{
		key: key, items: cloneOpenAIHTTPWSReplayItems(items), bytes: size, expiresAt: now.Add(ttl),
	}
	s.entries[key] = s.lru.PushFront(entry)
	s.bytes += size
	return true
}

// 调用方必须持有 mu；无需后台清理协程，读取及写入均在锁内执行到期判断。
func (s *openAIHTTPWSReplayStore) remove(element *list.Element) {
	entry, _ := element.Value.(*openAIHTTPWSReplayEntry)
	delete(s.entries, entry.key)
	s.lru.Remove(element)
	s.bytes -= entry.bytes
}

func cloneOpenAIHTTPWSReplayItems(items []json.RawMessage) []json.RawMessage {
	cloned := make([]json.RawMessage, len(items))
	for i, item := range items {
		cloned[i] = make(json.RawMessage, len(item))
		copy(cloned[i], item)
	}
	return cloned
}
