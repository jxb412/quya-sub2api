package service

import (
	"context"
	"errors"
	"sync/atomic"
	"testing"
	"time"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/stretchr/testify/require"
)

type renewalBillingCacheStub struct {
	billingCacheWorkerStub
	data      *SubscriptionCacheData
	epoch     int64
	epochErr  error
	epochRead atomic.Bool
	fills     chan *SubscriptionCacheData
}

func (c *renewalBillingCacheStub) GetSubscriptionCache(context.Context, int64, int64) (*SubscriptionCacheData, error) {
	return c.data, nil
}
func (c *renewalBillingCacheStub) GetSubscriptionCacheEpoch(context.Context, int64, int64) (int64, error) {
	c.epochRead.Store(true)
	return c.epoch, c.epochErr
}
func (c *renewalBillingCacheStub) SetSubscriptionCache(_ context.Context, _, _ int64, data *SubscriptionCacheData) error {
	c.fills <- data
	return nil
}

type renewalBillingSubRepo struct {
	UserSubscriptionRepository
	sub   *UserSubscription
	cache *renewalBillingCacheStub
	calls int
}

func (r *renewalBillingSubRepo) GetActiveByUserIDAndGroupID(context.Context, int64, int64) (*UserSubscription, error) {
	r.calls++
	if !r.cache.epochRead.Load() {
		return nil, errors.New("epoch must be captured before the database snapshot")
	}
	return r.sub, nil
}

func TestBillingCacheRenewalRefetchesMismatchedOrLegacyPeriod(t *testing.T) {
	for _, cacheID := range []int64{0, 100} {
		t.Run(map[int64]string{0: "legacy cache", 100: "old period"}[cacheID], func(t *testing.T) {
			cache := &renewalBillingCacheStub{data: &SubscriptionCacheData{SubscriptionID: cacheID, DailyUsage: 99}, epoch: 7, fills: make(chan *SubscriptionCacheData, 1)}
			repo := &renewalBillingSubRepo{cache: cache, sub: &UserSubscription{ID: 200, Status: SubscriptionStatusActive, ExpiresAt: time.Now().Add(time.Hour), DailyUsageUSD: 2}}
			svc := NewBillingCacheService(cache, nil, repo, nil, nil, nil, &config.Config{}, nil)
			t.Cleanup(svc.Stop)
			got, err := svc.getSubscriptionStatusForID(context.Background(), 10, 20, 200)
			require.NoError(t, err)
			require.Equal(t, int64(200), got.SubscriptionID)
			require.Equal(t, 2.0, got.DailyUsage)
			require.Equal(t, 1, repo.calls)
			select {
			case fill := <-cache.fills:
				require.Equal(t, int64(200), fill.SubscriptionID)
				require.Equal(t, int64(7), fill.CacheEpoch)
			case <-time.After(time.Second):
				t.Fatal("cache fill not queued")
			}
		})
	}
}

func TestBillingCacheRenewalRejectsOldRequestWithoutOpeningGlobalCircuit(t *testing.T) {
	cache := &renewalBillingCacheStub{data: &SubscriptionCacheData{SubscriptionID: 200}, fills: make(chan *SubscriptionCacheData, 1)}
	repo := &renewalBillingSubRepo{cache: cache, sub: &UserSubscription{ID: 200}}
	svc := NewBillingCacheService(cache, nil, repo, nil, nil, nil, &config.Config{}, nil)
	t.Cleanup(svc.Stop)
	svc.circuitBreaker = newBillingCircuitBreaker(config.CircuitBreakerConfig{Enabled: true, FailureThreshold: 2})
	for i := 0; i < 10; i++ {
		err := svc.checkSubscriptionEligibility(context.Background(), 10, &Group{ID: 20}, &UserSubscription{ID: 100})
		require.ErrorIs(t, err, ErrSubscriptionInvalid)
		require.True(t, svc.circuitBreaker.Allow(), "normal subscription replacement must not block other users")
	}
	require.Empty(t, cache.fills, "a stale request cannot fill new-period data under its old period")
}

func TestBillingCacheRenewalEpochFailureUsesDatabaseWithoutFilling(t *testing.T) {
	cache := &renewalBillingCacheStub{epochErr: errors.New("redis offline"), fills: make(chan *SubscriptionCacheData, 1)}
	repo := &renewalBillingSubRepo{cache: cache, sub: &UserSubscription{ID: 200}}
	svc := NewBillingCacheService(cache, nil, repo, nil, nil, nil, &config.Config{}, nil)
	got, err := svc.getSubscriptionStatusForID(context.Background(), 10, 20, 200)
	require.NoError(t, err)
	require.Equal(t, int64(200), got.SubscriptionID)
	svc.Stop()
	require.Empty(t, cache.fills, "unfenced snapshots cannot be asynchronously written")
}

func TestBillingCacheRenewalQueuedAndFallbackChargeCarryPeriodID(t *testing.T) {
	cache := &billingCacheWorkerStub{}
	svc := NewBillingCacheService(cache, nil, nil, nil, nil, nil, &config.Config{}, nil)
	svc.QueueUpdateSubscriptionUsage(10, 20, 100, 1)
	require.Eventually(t, func() bool { return atomic.LoadInt64(&cache.subscriptionID) == 100 }, time.Second, time.Millisecond)
	svc.Stop()
	svc.QueueUpdateSubscriptionUsage(10, 20, 200, 1)
	require.Equal(t, int64(200), atomic.LoadInt64(&cache.subscriptionID), "synchronous fallback keeps the original request period")
}

func TestBillingCacheFixedChargeRejectsReplacedPeriod(t *testing.T) {
	cache := &renewalBillingCacheStub{}
	cache.epochRead.Store(true)
	repo := &renewalBillingSubRepo{cache: cache, sub: &UserSubscription{ID: 200}}
	svc := &BillingCacheService{subRepo: repo}
	err := svc.checkFixedChargeSubscription(context.Background(), 10, &Group{ID: 20}, 100, 1)
	require.ErrorIs(t, err, ErrSubscriptionInvalid)
}

func TestFinalizeSubscriptionBillingCarriesOriginalPeriod(t *testing.T) {
	for _, result := range []*UsageBillingApplyResult{nil, {Applied: true}} {
		writes := make(chan cacheWriteTask, 1)
		groupID := int64(20)
		finalizePostUsageBilling(context.Background(), &postUsageBillingParams{
			Cost: &CostBreakdown{ActualCost: 2.5}, User: &User{ID: 10},
			APIKey: &APIKey{GroupID: &groupID}, Account: &Account{ID: 1},
			IsSubscriptionBill: true, Subscription: &UserSubscription{ID: 100},
		}, &billingDeps{
			billingCacheService: &BillingCacheService{cache: &billingCacheWorkerStub{}, cacheWriteChan: writes},
			deferredService:     &DeferredService{},
		}, result)
		require.Len(t, writes, 1)
		write := <-writes
		require.Equal(t, cacheWriteUpdateSubscriptionUsage, write.kind)
		require.EqualValues(t, 100, write.subscriptionID)
		require.EqualValues(t, 10, write.userID)
		require.EqualValues(t, 20, write.groupID)
		require.Equal(t, 2.5, write.amount)
	}
}
