//go:build unit

package repository

import (
	"context"
	"testing"
	"time"

	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/redis/go-redis/v9"
	"github.com/stretchr/testify/require"
)

// 2026-11-24: an in-flight request belongs to its original paid subscription,
// including after a renewal replaces the user/group's active subscription.
func TestSubscriptionCacheRenewalLateChargeStaysInOldPeriod(t *testing.T) {
	c, _ := newMiniRedisCache(t)
	ctx := context.Background()
	current := &service.SubscriptionCacheData{SubscriptionID: 200, Status: "active", ExpiresAt: time.Now().Add(time.Hour), DailyUsage: 1, WeeklyUsage: 2, MonthlyUsage: 3}
	require.NoError(t, c.SetSubscriptionCache(ctx, 10, 20, current))
	require.NoError(t, c.UpdateSubscriptionUsage(ctx, 10, 20, 100, 50))
	got, err := c.GetSubscriptionCache(ctx, 10, 20)
	require.NoError(t, err)
	require.Equal(t, int64(200), got.SubscriptionID)
	require.Equal(t, 1.0, got.DailyUsage)
	require.Equal(t, 2.0, got.WeeklyUsage)
	require.Equal(t, 3.0, got.MonthlyUsage)
	epoch, err := c.GetSubscriptionCacheEpoch(ctx, 10, 20)
	require.NoError(t, err)
	require.Zero(t, epoch, "old-period charges must not disturb the new-period cache")
	require.NoError(t, c.UpdateSubscriptionUsage(ctx, 10, 20, 200, 0.5))
	got, err = c.GetSubscriptionCache(ctx, 10, 20)
	require.NoError(t, err)
	require.Equal(t, 1.5, got.DailyUsage)
	require.Equal(t, 2.5, got.WeeklyUsage)
	require.Equal(t, 3.5, got.MonthlyUsage)
}

func TestSubscriptionCacheRenewalRejectsDelayedSnapshots(t *testing.T) {
	for _, newCachePresent := range []bool{false, true} {
		t.Run(map[bool]string{false: "invalidated cache remains empty", true: "replacement cache remains intact"}[newCachePresent], func(t *testing.T) {
			c, _ := newMiniRedisCache(t)
			ctx := context.Background()
			old := &service.SubscriptionCacheData{SubscriptionID: 100, Status: "active", ExpiresAt: time.Now().Add(time.Hour), DailyUsage: 90}
			require.NoError(t, c.SetSubscriptionCache(ctx, 10, 20, old))
			require.NoError(t, c.InvalidateSubscriptionCache(ctx, 10, 20))
			if newCachePresent {
				epoch, err := c.GetSubscriptionCacheEpoch(ctx, 10, 20)
				require.NoError(t, err)
				require.NoError(t, c.SetSubscriptionCache(ctx, 10, 20, &service.SubscriptionCacheData{SubscriptionID: 200, CacheEpoch: epoch, Status: "active", ExpiresAt: old.ExpiresAt}))
			}
			require.NoError(t, c.SetSubscriptionCache(ctx, 10, 20, old), "delayed async write is discarded")
			got, err := c.GetSubscriptionCache(ctx, 10, 20)
			if !newCachePresent {
				require.ErrorIs(t, err, redis.Nil)
				return
			}
			require.NoError(t, err)
			require.Equal(t, int64(200), got.SubscriptionID)
			require.Zero(t, got.DailyUsage)
		})
	}
}

func TestSubscriptionCacheChargeFencesEarlierDatabaseSnapshot(t *testing.T) {
	for _, cachePresent := range []bool{false, true} {
		t.Run(map[bool]string{false: "cache miss", true: "cache hit"}[cachePresent], func(t *testing.T) {
			c, _ := newMiniRedisCache(t)
			ctx := context.Background()
			old := &service.SubscriptionCacheData{SubscriptionID: 100, Status: "active", ExpiresAt: time.Now().Add(time.Hour), DailyUsage: 1}
			if cachePresent {
				require.NoError(t, c.SetSubscriptionCache(ctx, 10, 20, old))
			}
			require.NoError(t, c.UpdateSubscriptionUsage(ctx, 10, 20, 100, 2))
			require.NoError(t, c.SetSubscriptionCache(ctx, 10, 20, old))
			got, err := c.GetSubscriptionCache(ctx, 10, 20)
			if !cachePresent {
				require.ErrorIs(t, err, redis.Nil)
				return
			}
			require.NoError(t, err)
			require.Equal(t, 3.0, got.DailyUsage)
		})
	}
}

func TestSubscriptionCacheRejectsLegacySchemaWithoutPeriodID(t *testing.T) {
	c, _ := newMiniRedisCache(t)
	ctx := context.Background()
	require.NoError(t, c.rdb.HSet(ctx, billingSubKey(10, 20), "status", "active", "daily_usage", "90").Err())
	_, err := c.GetSubscriptionCache(ctx, 10, 20)
	require.ErrorContains(t, err, "missing subscription ID")
}
