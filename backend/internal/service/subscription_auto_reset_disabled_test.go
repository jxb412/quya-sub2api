package service

import (
	"context"
	"testing"
	"time"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/Wei-Shaw/sub2api/internal/pkg/pagination"
	"github.com/stretchr/testify/require"
)

func newSubscriptionServiceWithAutoReset(repo UserSubscriptionRepository) *SubscriptionService {
	cfg := &config.Config{
		SubscriptionMaintenance: config.SubscriptionMaintenanceConfig{
			AutoResetEnabled: true,
		},
	}
	return NewSubscriptionService(groupRepoNoop{}, repo, nil, nil, cfg)
}

type disabledAutoResetRepo struct {
	userSubRepoNoop
	subscriptions []UserSubscription
	resetCalls    int
	activateCalls int
}

func (r *disabledAutoResetRepo) ResetDailyUsage(context.Context, int64, *time.Time, time.Time) error {
	r.resetCalls++
	return nil
}

func (r *disabledAutoResetRepo) ResetWeeklyUsage(context.Context, int64, *time.Time, time.Time) error {
	r.resetCalls++
	return nil
}

func (r *disabledAutoResetRepo) ResetMonthlyUsage(context.Context, int64, *time.Time, time.Time) error {
	r.resetCalls++
	return nil
}

func (r *disabledAutoResetRepo) ActivateWindows(context.Context, int64, time.Time, time.Time) error {
	r.activateCalls++
	return nil
}

func (r *disabledAutoResetRepo) ListByUserID(context.Context, int64) ([]UserSubscription, error) {
	return append([]UserSubscription(nil), r.subscriptions...), nil
}

func (r *disabledAutoResetRepo) ListActiveByUserID(context.Context, int64) ([]UserSubscription, error) {
	return append([]UserSubscription(nil), r.subscriptions...), nil
}

func (r *disabledAutoResetRepo) ListByGroupID(context.Context, int64, pagination.PaginationParams) ([]UserSubscription, *pagination.PaginationResult, error) {
	return append([]UserSubscription(nil), r.subscriptions...), &pagination.PaginationResult{}, nil
}

func (r *disabledAutoResetRepo) List(context.Context, pagination.PaginationParams, *int64, *int64, string, string, string, string) ([]UserSubscription, *pagination.PaginationResult, error) {
	return append([]UserSubscription(nil), r.subscriptions...), &pagination.PaginationResult{}, nil
}

func TestSubscriptionAutoResetDisabledRetainsExpiredWindowUsage(t *testing.T) {
	now := time.Date(2026, 9, 23, 12, 0, 0, 0, time.UTC)
	stale := now.Add(-45 * 24 * time.Hour)
	repo := &disabledAutoResetRepo{}
	svc := NewSubscriptionService(groupRepoNoop{}, repo, nil, nil, &config.Config{})
	svc.now = func() time.Time { return now }
	sub := &UserSubscription{
		ID:                 1,
		UserID:             2,
		GroupID:            3,
		Status:             SubscriptionStatusActive,
		StartsAt:           stale,
		ExpiresAt:          now.Add(30 * 24 * time.Hour),
		DailyWindowStart:   &stale,
		WeeklyWindowStart:  &stale,
		MonthlyWindowStart: &stale,
		DailyUsageUSD:      11,
		WeeklyUsageUSD:     22,
		MonthlyUsageUSD:    33,
	}

	require.NoError(t, svc.CheckAndResetWindows(context.Background(), sub))
	require.Equal(t, 0, repo.resetCalls)
	require.Equal(t, 11.0, sub.DailyUsageUSD)
	require.Equal(t, 22.0, sub.WeeklyUsageUSD)
	require.Equal(t, 33.0, sub.MonthlyUsageUSD)
	require.Equal(t, stale, *sub.DailyWindowStart)
	require.Equal(t, stale, *sub.WeeklyWindowStart)
	require.Equal(t, stale, *sub.MonthlyWindowStart)
}

func TestSubscriptionAutoResetDisabledKeepsExhaustedSubscriptionBlocked(t *testing.T) {
	now := time.Date(2026, 9, 23, 12, 0, 0, 0, time.UTC)
	stale := now.Add(-45 * 24 * time.Hour)
	limit := 10.0
	svc := NewSubscriptionService(groupRepoNoop{}, &disabledAutoResetRepo{}, nil, nil, &config.Config{})
	svc.now = func() time.Time { return now }
	sub := &UserSubscription{
		Status:             SubscriptionStatusActive,
		StartsAt:           stale,
		ExpiresAt:          now.Add(30 * 24 * time.Hour),
		DailyWindowStart:   &stale,
		WeeklyWindowStart:  &stale,
		MonthlyWindowStart: &stale,
		DailyUsageUSD:      11,
		WeeklyUsageUSD:     22,
		MonthlyUsageUSD:    33,
	}

	needsMaintenance, err := svc.ValidateAndCheckLimits(sub, &Group{
		DailyLimitUSD:   &limit,
		WeeklyLimitUSD:  &limit,
		MonthlyLimitUSD: &limit,
	})

	require.ErrorIs(t, err, ErrDailyLimitExceeded)
	require.False(t, needsMaintenance)
	require.Equal(t, 11.0, sub.DailyUsageUSD)
	require.Equal(t, 22.0, sub.WeeklyUsageUSD)
	require.Equal(t, 33.0, sub.MonthlyUsageUSD)
}

func TestSubscriptionAutoResetDisabledSkipsAllWindowMaintenance(t *testing.T) {
	repo := &disabledAutoResetRepo{}
	svc := NewSubscriptionService(groupRepoNoop{}, repo, nil, nil, &config.Config{})
	sub := &UserSubscription{ID: 1, UserID: 2, GroupID: 3}

	refreshed, err := svc.EnsureWindowMaintenance(context.Background(), sub)
	require.NoError(t, err)
	require.Same(t, sub, refreshed)
	svc.DoWindowMaintenance(sub)

	require.Equal(t, 0, repo.activateCalls)
	require.Equal(t, 0, repo.resetCalls)
}

func TestSubscriptionAutoResetDisabledHidesCountdownWithoutClearingUsage(t *testing.T) {
	now := time.Date(2026, 9, 23, 12, 0, 0, 0, time.UTC)
	stale := now.Add(-45 * 24 * time.Hour)
	repo := &disabledAutoResetRepo{subscriptions: []UserSubscription{{
		ID:                 1,
		Status:             SubscriptionStatusActive,
		ExpiresAt:          now.Add(30 * 24 * time.Hour),
		DailyWindowStart:   &stale,
		WeeklyWindowStart:  &stale,
		MonthlyWindowStart: &stale,
		DailyUsageUSD:      11,
		WeeklyUsageUSD:     22,
		MonthlyUsageUSD:    33,
	}}}
	svc := NewSubscriptionService(groupRepoNoop{}, repo, nil, nil, &config.Config{})

	subs, err := svc.ListUserSubscriptions(context.Background(), 1)
	require.NoError(t, err)
	require.Len(t, subs, 1)
	require.Nil(t, subs[0].DailyWindowStart)
	require.Nil(t, subs[0].WeeklyWindowStart)
	require.Nil(t, subs[0].MonthlyWindowStart)
	require.Equal(t, 11.0, subs[0].DailyUsageUSD)
	require.Equal(t, 22.0, subs[0].WeeklyUsageUSD)
	require.Equal(t, 33.0, subs[0].MonthlyUsageUSD)
}
