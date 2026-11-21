package repository

import (
	"context"
	"fmt"
	"testing"
	"time"

	"github.com/alicebob/miniredis/v2"
	"github.com/redis/go-redis/v9"
	"github.com/stretchr/testify/require"
)

// 2026-11-21: rolling startup on shared Redis must not release a peer's capacity.
func TestStartupCleanupPreservesOtherInstancesCapacity(t *testing.T) {
	server := miniredis.RunT(t)
	clientA := redis.NewClient(&redis.Options{Addr: server.Addr()})
	clientB := redis.NewClient(&redis.Options{Addr: server.Addr()})
	t.Cleanup(func() { _ = clientA.Close(); _ = clientB.Close() })
	instanceA := NewConcurrencyCache(clientA, 15, 90)
	instanceB := NewConcurrencyCache(clientB, 15, 90)
	ctx := context.Background()

	ok, err := instanceA.AcquireAccountSlot(ctx, 142, 1, "instance-a-account")
	require.NoError(t, err)
	require.True(t, ok)
	ok, err = instanceA.AcquireUserSlot(ctx, 107, 1, "instance-a-user")
	require.NoError(t, err)
	require.True(t, ok)
	ok, err = instanceA.IncrementAccountWaitCount(ctx, 142, 1)
	require.NoError(t, err)
	require.True(t, ok)
	ok, err = instanceA.IncrementWaitCount(ctx, 107, 1)
	require.NoError(t, err)
	require.True(t, ok)

	// Startup itself must not require co-locating slots, waits, and indexes in one hash slot.
	clientB.AddHook(startupSingleKeyHook{})
	require.NoError(t, instanceB.CleanupStaleProcessSlots(ctx, "instance-b-"))
	require.NoError(t, instanceB.CleanupStaleProcessSlots(ctx, "instance-b-"))
	for _, key := range []string{accountSlotKey(142), userSlotKey(107)} {
		count, err := clientA.ZCard(ctx, key).Result()
		require.NoError(t, err)
		require.EqualValues(t, 1, count)
	}
	for _, key := range []string{accountWaitKey(142), waitQueueKey(107)} {
		count, err := clientA.Get(ctx, key).Int()
		require.NoError(t, err)
		require.Equal(t, 1, count)
	}
	// An independent instance must still observe the occupied capacity and queue limits.
	instanceB = NewConcurrencyCache(clientA, 15, 90)
	ok, err = instanceB.AcquireAccountSlot(ctx, 142, 1, "instance-b-account")
	require.NoError(t, err)
	require.False(t, ok)
	ok, err = instanceB.AcquireUserSlot(ctx, 107, 1, "instance-b-user")
	require.NoError(t, err)
	require.False(t, ok)
	ok, err = instanceB.IncrementAccountWaitCount(ctx, 142, 1)
	require.NoError(t, err)
	require.False(t, ok)
	ok, err = instanceB.IncrementWaitCount(ctx, 107, 1)
	require.NoError(t, err)
	require.False(t, ok)
}

func TestStartupCleanupUsesExpiryRegardlessOfPrefix(t *testing.T) {
	for _, prefix := range []string{"instance-a-", "instance-b-", ""} {
		t.Run(fmt.Sprintf("prefix=%q", prefix), func(t *testing.T) {
			server := miniredis.RunT(t)
			now := time.Unix(1805760000, 0)
			server.SetTime(now)
			client := redis.NewClient(&redis.Options{Addr: server.Addr()})
			t.Cleanup(func() { _ = client.Close() })
			cache := NewConcurrencyCache(client, 15, 90)
			ctx := context.Background()
			for _, spec := range []slotIndexSpec{accountSlotIndex, userSlotIndex} {
				require.NoError(t, client.ZAdd(ctx, spec.slotKey(1),
					redis.Z{Score: float64(now.Unix()), Member: "instance-a-live"},
					redis.Z{Score: float64(now.Unix()), Member: "instance-b-live"},
					redis.Z{Score: float64(now.Unix() - 900), Member: "instance-a-expired"},
					redis.Z{Score: float64(now.Unix() - 901), Member: "instance-b-expired"},
				).Err())
				require.NoError(t, client.Expire(ctx, spec.slotKey(1), 30*time.Second).Err())
				require.NoError(t, client.ZAdd(ctx, spec.slotKey(2), redis.Z{
					Score: float64(now.Unix() - 900), Member: "instance-a-expired",
				}).Err())
				require.NoError(t, client.ZAdd(ctx, spec.indexKey,
					redis.Z{Score: float64(now.Unix() - 1), Member: "1"},
					redis.Z{Score: float64(now.Unix() - 1), Member: "2"},
				).Err())
			}
			require.NoError(t, cache.CleanupStaleProcessSlots(ctx, prefix))
			for _, spec := range []slotIndexSpec{accountSlotIndex, userSlotIndex} {
				members, err := client.ZRange(ctx, spec.slotKey(1), 0, -1).Result()
				require.NoError(t, err)
				require.ElementsMatch(t, []string{"instance-a-live", "instance-b-live"}, members)
				ttl, err := client.TTL(ctx, spec.slotKey(1)).Result()
				require.NoError(t, err)
				require.Equal(t, 30*time.Second, ttl, "startup must not extend a peer's key expiry")
				exists, err := client.Exists(ctx, spec.slotKey(2)).Result()
				require.NoError(t, err)
				require.Zero(t, exists)
				_, err = client.ZScore(ctx, spec.indexKey, "2").Result()
				require.ErrorIs(t, err, redis.Nil)
			}
		})
	}
}

func TestStartupCleanupPreservesWaitOnlyAndUnindexedCounters(t *testing.T) {
	for _, markerExists := range []bool{false, true} {
		t.Run(fmt.Sprintf("legacy-marker=%v", markerExists), func(t *testing.T) {
			server := miniredis.RunT(t)
			client := redis.NewClient(&redis.Options{Addr: server.Addr()})
			t.Cleanup(func() { _ = client.Close() })
			cache := NewConcurrencyCache(client, 15, 1200)
			ctx := context.Background()
			if markerExists {
				require.NoError(t, client.Set(ctx, "concurrency:startup:legacy_wait_sweep:v1", "1", 0).Err())
			}
			for _, spec := range []slotIndexSpec{accountSlotIndex, userSlotIndex} {
				require.NoError(t, client.Set(ctx, spec.waitKey(1), 3, time.Minute).Err())
				require.NoError(t, client.Set(ctx, spec.waitKey(2), 5, time.Minute).Err())
				require.NoError(t, client.ZAdd(ctx, spec.indexKey, redis.Z{Score: 1, Member: "1"}).Err())
			}
			require.NoError(t, cache.CleanupStaleProcessSlots(ctx, "new-process-"))
			for _, spec := range []slotIndexSpec{accountSlotIndex, userSlotIndex} {
				for id, want := range map[int64]int{1: 3, 2: 5} {
					count, err := client.Get(ctx, spec.waitKey(id)).Int()
					require.NoError(t, err)
					require.Equal(t, want, count)
					ttl, err := client.TTL(ctx, spec.waitKey(id)).Result()
					require.NoError(t, err)
					require.Equal(t, time.Minute, ttl)
				}
				score, err := client.ZScore(ctx, spec.indexKey, "1").Result()
				require.NoError(t, err, "wait-only traffic must keep its active index entry")
				require.Greater(t, score, float64(1))
			}
			server.FastForward(time.Minute)
			for _, spec := range []slotIndexSpec{accountSlotIndex, userSlotIndex} {
				exists, err := client.Exists(ctx, spec.waitKey(1), spec.waitKey(2)).Result()
				require.NoError(t, err)
				require.Zero(t, exists, "abandoned wait counters still expire naturally")
			}
		})
	}
}

// Check the startup path's Cluster constraint without requiring a Redis Cluster test service.
type startupSingleKeyHook struct{}

func (startupSingleKeyHook) DialHook(next redis.DialHook) redis.DialHook { return next }
func (startupSingleKeyHook) ProcessPipelineHook(next redis.ProcessPipelineHook) redis.ProcessPipelineHook {
	return next
}
func (startupSingleKeyHook) ProcessHook(next redis.ProcessHook) redis.ProcessHook {
	return func(ctx context.Context, cmd redis.Cmder) error {
		switch cmd.Name() {
		case "eval", "evalsha":
			if fmt.Sprint(cmd.Args()[2]) != "1" {
				return fmt.Errorf("startup script requires more than one Redis hash slot: %v", cmd.Args()[2])
			}
		case "del", "unlink":
			if len(cmd.Args()) != 2 {
				return fmt.Errorf("startup deletion spans multiple Redis keys")
			}
		case "scan":
			return fmt.Errorf("startup must not scan unrelated keys")
		}
		return next(ctx, cmd)
	}
}
