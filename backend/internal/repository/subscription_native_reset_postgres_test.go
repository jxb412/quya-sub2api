package repository

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/stretchr/testify/require"
)

func TestNativeResetPostgresConcurrentReplayAndPurchaseQueue(t *testing.T) {
	db, svc, _ := renewalPostgres(t)
	ctx := context.Background()
	_, err := svc.PreviewSubscriptionReset(ctx, 11, 1)
	require.ErrorIs(t, err, service.ErrSubscriptionNotFound, "another member cannot inspect these orders")
	preview, err := svc.PreviewSubscriptionReset(ctx, 10, 1)
	require.NoError(t, err)
	require.True(t, preview.Eligible)
	require.Equal(t, 2, preview.PendingCount)
	const count = 6
	results := make([]*service.SubscriptionUserResetResult, count)
	errs := make([]error, count)
	var wg sync.WaitGroup
	start := make(chan struct{})
	for i := range results {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			<-start
			results[i], errs[i] = svc.ResetUserSubscription(ctx, 10, 1, preview.PreviewToken, true)
		}(i)
	}
	close(start)
	wg.Wait()
	var newID int64
	fresh := 0
	for i, err := range errs {
		require.NoError(t, err)
		if newID == 0 {
			newID = results[i].NewSubscriptionID
		}
		require.Equal(t, newID, results[i].NewSubscriptionID)
		if !results[i].Replayed {
			fresh++
		}
	}
	require.Equal(t, 1, fresh)
	var rows int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM subscription_renewal_activations`).Scan(&rows))
	require.Equal(t, 1, rows)
	var previous string
	require.NoError(t, db.QueryRow(`SELECT receipt->>'previous_code' FROM subscription_renewal_activations WHERE old_subscription_id=1`).Scan(&previous))
	require.Equal(t, "store_current", previous)
	second, err := svc.PreviewSubscriptionReset(ctx, 10, newID)
	require.NoError(t, err)
	require.True(t, second.Eligible)
	require.Equal(t, 1, second.PendingCount)
	require.Equal(t, 31, second.NextValidityDays)
	activated, err := svc.ResetUserSubscription(ctx, 10, newID, second.PreviewToken, true)
	require.NoError(t, err)
	require.Equal(t, 31, activated.TotalValidityDays)
	require.Zero(t, activated.PendingCount)
	last, err := svc.PreviewSubscriptionReset(ctx, 10, activated.NewSubscriptionID)
	require.NoError(t, err)
	require.False(t, last.Eligible)
}

func TestNativeResetPostgresRejectsStaleProofAndRechecksRefund(t *testing.T) {
	db, svc, _ := renewalPostgres(t)
	ctx := context.Background()
	p, err := svc.PreviewSubscriptionReset(ctx, 10, 1)
	require.NoError(t, err)
	require.True(t, p.Eligible)
	_, err = db.Exec(`INSERT INTO redeem_codes(code,type,status,used_by,group_id,validity_days,used_at,notes) VALUES('store_extra','subscription','used',10,20,30,NOW(),'store order o_extra'); UPDATE user_subscriptions SET expires_at=expires_at+interval '30 days',notes=notes||E'\n通过兑换码 store_extra 兑换' WHERE id=1`)
	require.NoError(t, err)
	_, err = svc.ResetUserSubscription(ctx, 10, 1, p.PreviewToken, true)
	require.Error(t, err)
	var active int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM user_subscriptions WHERE deleted_at IS NULL`).Scan(&active))
	require.Equal(t, 1, active)
	next, err := svc.PreviewSubscriptionReset(ctx, 10, 1)
	require.NoError(t, err)
	require.True(t, next.Eligible)
	require.NotEqual(t, p.PreviewToken, next.PreviewToken)
	require.Equal(t, 3, next.PendingCount)
	hash := sha256.Sum256([]byte("10:store_next"))
	refund := "sbr_" + hex.EncodeToString(hash[:])[:28]
	_, err = db.Exec(`INSERT INTO redeem_codes(code,type,status,used_by,validity_days,used_at,notes) VALUES($1,'balance','used',10,0,NOW(),'subscription refund / o_next')`, refund)
	require.NoError(t, err)
	_, err = svc.ResetUserSubscription(ctx, 10, 1, next.PreviewToken, true)
	require.Error(t, err)
	denied, err := svc.PreviewSubscriptionReset(ctx, 10, 1)
	require.NoError(t, err)
	require.False(t, denied.Eligible)
	require.Contains(t, denied.Reason, "退款")
	var receipts int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM subscription_renewal_activations`).Scan(&receipts))
	require.Zero(t, receipts)
}

func TestNativeResetPostgresUsageChangesDoNotInvalidateConfirmation(t *testing.T) {
	db, svc, _ := renewalPostgres(t)
	ctx := context.Background()
	p, err := svc.PreviewSubscriptionReset(ctx, 10, 1)
	require.NoError(t, err)
	_, err = db.Exec(`UPDATE user_subscriptions SET daily_usage_usd=10,weekly_usage_usd=10,monthly_usage_usd=10,updated_at=NOW() WHERE id=1`)
	require.NoError(t, err)
	r, err := svc.ResetUserSubscription(ctx, 10, 1, p.PreviewToken, true)
	require.NoError(t, err)
	require.Equal(t, 61*24*time.Hour, r.ExpiresAt.Sub(r.StartsAt), "the new period includes both full purchased terms")
	var daily float64
	require.NoError(t, db.QueryRow(`SELECT daily_usage_usd FROM user_subscriptions WHERE id=$1`, r.NewSubscriptionID).Scan(&daily))
	require.Zero(t, daily)
	_, err = svc.ResetUserSubscription(ctx, 11, 1, p.PreviewToken, true)
	require.ErrorIs(t, err, service.ErrSubscriptionNotFound)
	_, err = svc.ResetUserSubscription(ctx, 10, 1, strings.Repeat("f", 64), true)
	require.Error(t, err)
}

func TestNativeResetPostgresRejectsCompetingLiveRowWithoutUniqueIndex(t *testing.T) {
	db, svc, in := renewalPostgres(t)
	ctx := context.Background()
	_, err := db.Exec(`DROP INDEX renewal_active_sub`)
	require.NoError(t, err)
	p, err := svc.PreviewSubscriptionReset(ctx, 10, 1)
	require.NoError(t, err)
	require.True(t, p.Eligible)
	var otherID int64
	require.NoError(t, db.QueryRow(`INSERT INTO user_subscriptions(user_id,group_id,starts_at,expires_at,status,notes) VALUES(10,20,NOW(),NOW()+INTERVAL '7 days','active','管理员单独赠送') RETURNING id`).Scan(&otherID))
	denied, err := svc.PreviewSubscriptionReset(ctx, 10, 1)
	require.NoError(t, err)
	require.False(t, denied.Eligible)
	require.Contains(t, denied.Reason, "多个未撤销套餐")
	_, err = svc.ResetUserSubscription(ctx, 10, 1, p.PreviewToken, true)
	require.ErrorContains(t, err, "多个未撤销套餐")
	_, err = svc.ActivateSubscriptionRenewal(ctx, 1, in)
	require.ErrorContains(t, err, "多个未撤销套餐", "legacy full-admin activation must use the same guard")
	var live, total, receipts int
	require.NoError(t, db.QueryRow(`SELECT count(*) FILTER(WHERE deleted_at IS NULL),count(*) FROM user_subscriptions`).Scan(&live, &total))
	require.Equal(t, 2, live)
	require.Equal(t, 2, total)
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM subscription_renewal_activations`).Scan(&receipts))
	require.Zero(t, receipts)
	var expiry time.Time
	var notes string
	require.NoError(t, db.QueryRow(`SELECT expires_at,notes FROM user_subscriptions WHERE id=1`).Scan(&expiry, &notes))
	require.Equal(t, in.ExpectedExpiresAt, expiry)
	require.Equal(t, in.ExpectedNotes, notes)
	_, err = db.Exec(`UPDATE user_subscriptions SET deleted_at=NOW() WHERE id=$1`, otherID)
	require.NoError(t, err)
	recovered, err := svc.PreviewSubscriptionReset(ctx, 10, 1)
	require.NoError(t, err)
	require.True(t, recovered.Eligible, "clearing only the conflicting row restores eligibility")
}
