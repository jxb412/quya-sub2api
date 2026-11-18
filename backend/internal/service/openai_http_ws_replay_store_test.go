package service

import (
	"encoding/json"
	"fmt"
	"sync"
	"testing"
	"time"

	"github.com/stretchr/testify/require"
)

// 2026-11-19：验证完整历史缓存的隔离、深拷贝、到期和容量边界；
// 缓存失效必须降级为 miss，不能返回其他身份或已经淘汰的正文。
func TestOpenAIHTTPWSReplayStoreIdentityAndDeepCopy(t *testing.T) {
	store := newOpenAIHTTPWSReplayStore()
	backing := make([]byte, 1<<20)
	copy(backing, `{"role":"user","content":"a"}`)
	raw := json.RawMessage(backing[:27])
	items := []json.RawMessage{raw}
	require.True(t, store.Put("identity-a", "resp-1", items, time.Minute))
	backing[0] = '['
	items[0] = json.RawMessage(`"replacement"`)
	got, ok := store.Get("identity-a", "resp-1")
	require.True(t, ok)
	require.Equal(t, byte('{'), got[0][0])
	require.Equal(t, len(raw), cap(got[0]), "returned items must own exact-sized backing arrays")
	entry, validEntry := store.entries[openAIHTTPWSReplayKey{identity: "identity-a", responseID: "resp-1"}].Value.(*openAIHTTPWSReplayEntry)
	require.True(t, validEntry)
	require.Equal(t, len(raw), cap(entry.items[0]), "stored items must not pin the large request backing array")
	got[0][0] = '['
	again, ok := store.Get("identity-a", "resp-1")
	require.True(t, ok)
	require.Equal(t, byte('{'), again[0][0])
	_, ok = store.Get("identity-b", "resp-1")
	require.False(t, ok)
	_, ok = store.Get("identity-a", "resp-2")
	require.False(t, ok)
}

func TestOpenAIHTTPWSReplayStoreTTLDoesNotSlideAndIsCapped(t *testing.T) {
	now := time.Unix(1_800_000_000, 0)
	store := newOpenAIHTTPWSReplayStore()
	store.now = func() time.Time { return now }
	require.True(t, store.Put("identity", "short", nil, time.Minute))
	require.True(t, store.Put("identity", "capped", nil, time.Hour))
	now = now.Add(time.Minute - time.Nanosecond)
	_, ok := store.Get("identity", "short")
	require.True(t, ok)
	now = now.Add(time.Nanosecond)
	_, ok = store.Get("identity", "short")
	require.False(t, ok, "a successful Get must not extend the expiry")
	now = now.Add(14*time.Minute - time.Nanosecond)
	_, ok = store.Get("identity", "capped")
	require.True(t, ok)
	now = now.Add(time.Nanosecond)
	_, ok = store.Get("identity", "capped")
	require.False(t, ok, "configured sticky TTL must not exceed the 15-minute history retention cap")
	require.Zero(t, store.bytes)
	require.Empty(t, store.entries)
	for _, ttl := range []time.Duration{0, -time.Second} {
		require.False(t, store.Put("identity", "invalid", nil, ttl))
	}
}

func TestOpenAIHTTPWSReplayStoreLRUEvictionAndReplacement(t *testing.T) {
	store := newOpenAIHTTPWSReplayStore()
	store.maxEntries = 2
	require.True(t, store.Put("id", "a", []json.RawMessage{json.RawMessage(`1`)}, time.Minute))
	require.True(t, store.Put("id", "b", []json.RawMessage{json.RawMessage(`2`)}, time.Minute))
	_, ok := store.Get("id", "a")
	require.True(t, ok)
	require.True(t, store.Put("id", "c", []json.RawMessage{json.RawMessage(`3`)}, time.Minute))
	_, ok = store.Get("id", "b")
	require.False(t, ok, "the least recently used history must be evicted")
	bytesBefore := store.bytes
	require.True(t, store.Put("id", "a", []json.RawMessage{json.RawMessage(`100`)}, time.Minute))
	require.Equal(t, bytesBefore+2, store.bytes, "replacement must subtract old bytes")
	require.Len(t, store.entries, 2)
	got, ok := store.Get("id", "a")
	require.True(t, ok)
	require.Equal(t, "100", string(got[0]))
}

func TestOpenAIHTTPWSReplayStoreCountsItemOverheadAndTotalBudget(t *testing.T) {
	store := newOpenAIHTTPWSReplayStore()
	oneItem := []json.RawMessage{json.RawMessage(`1`)}
	entryBytes := openAIHTTPWSReplayEntryOverhead + int64(len("id")+len("a")) + openAIHTTPWSReplayItemOverhead + 1
	store.maxBytes = entryBytes * 2
	store.maxEntryBytes = entryBytes
	require.True(t, store.Put("id", "a", oneItem, time.Minute))
	require.Equal(t, entryBytes, store.bytes)
	require.False(t, store.Put("id", "oversize", oneItem, time.Minute))
	require.False(t, store.Put("id", "a", []json.RawMessage{json.RawMessage(`1`), json.RawMessage(`2`)}, time.Minute))
	require.Equal(t, entryBytes, store.bytes, "rejected insertion must not delete a valid old entry")
	require.True(t, store.Put("id", "b", oneItem, time.Minute))
	require.True(t, store.Put("id", "c", oneItem, time.Minute))
	require.Equal(t, entryBytes*2, store.bytes)
	_, ok := store.Get("id", "a")
	require.False(t, ok)
	require.Len(t, store.entries, 2)
}

func TestOpenAIHTTPWSReplayStorePurgesExpiredBeforeValidLRU(t *testing.T) {
	now := time.Unix(1_800_000_000, 0)
	store := newOpenAIHTTPWSReplayStore()
	store.now = func() time.Time { return now }
	store.maxEntries = 2
	require.True(t, store.Put("id", "long", nil, time.Minute))
	require.True(t, store.Put("id", "short", nil, time.Second))
	now = now.Add(2 * time.Second)
	require.True(t, store.Put("id", "new", nil, time.Minute))
	_, ok := store.Get("id", "long")
	require.True(t, ok, "an expired recent entry must be removed before a valid older entry")
	_, ok = store.Get("id", "short")
	require.False(t, ok)
}

func TestOpenAIHTTPWSReplayStoreConcurrentAccessAndLazyInitialization(t *testing.T) {
	svc := &OpenAIGatewayService{}
	stores := make(chan *openAIHTTPWSReplayStore, 8)
	var workers sync.WaitGroup
	for worker := 0; worker < 8; worker++ {
		workers.Add(1)
		go func(worker int) {
			defer workers.Done()
			store := svc.getOpenAIHTTPWSReplayStore()
			stores <- store
			identity := fmt.Sprintf("identity-%d", worker)
			for i := 0; i < 20; i++ {
				responseID := fmt.Sprintf("resp-%d", i)
				if !store.Put(identity, responseID, []json.RawMessage{json.RawMessage(`{"safe":true}`)}, time.Minute) {
					t.Errorf("Put failed for %s/%s", identity, responseID)
					return
				}
				if _, ok := store.Get(identity, responseID); !ok {
					t.Errorf("Get missed %s/%s", identity, responseID)
					return
				}
			}
		}(worker)
	}
	workers.Wait()
	close(stores)
	for store := range stores {
		require.Same(t, svc.getOpenAIHTTPWSReplayStore(), store)
	}
	require.Len(t, svc.getOpenAIHTTPWSReplayStore().entries, 160)
	require.LessOrEqual(t, svc.getOpenAIHTTPWSReplayStore().bytes, openAIHTTPWSReplayMaxBytes)
}
