package repository

import (
	"context"
	"database/sql"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"entgo.io/ent/dialect"
	entsql "entgo.io/ent/dialect/sql"
	dbent "github.com/Wei-Shaw/sub2api/ent"
	_ "github.com/Wei-Shaw/sub2api/ent/runtime"
	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/stretchr/testify/require"
)

// These tests never use deployment configuration. An explicitly supplied
// throwaway database and a fresh schema are required; no Docker/TestMain.
func renewalPostgres(t *testing.T) (*sql.DB, *service.SubscriptionService, *service.ActivateSubscriptionRenewalInput) {
	t.Helper()
	dsn := os.Getenv("QUYA_RENEWAL_TEST_DSN")
	if dsn == "" {
		t.Skip("QUYA_RENEWAL_TEST_DSN is not set")
	}
	base, err := sql.Open("postgres", dsn)
	require.NoError(t, err)
	var database string
	require.NoError(t, base.QueryRow(`SELECT current_database()`).Scan(&database))
	if !strings.HasPrefix(database, "quya_renewal_test") {
		_ = base.Close()
		t.Fatal("renewal tests require a quya_renewal_test throwaway database")
	}
	schema := fmt.Sprintf("renewal_%d", time.Now().UnixNano())
	_, err = base.Exec(`CREATE SCHEMA ` + schema)
	require.NoError(t, err)
	localDSN := dsn + " search_path=" + schema
	if strings.Contains(dsn, "://") {
		u, err := url.Parse(dsn)
		require.NoError(t, err)
		q := u.Query()
		q.Set("search_path", schema)
		u.RawQuery = q.Encode()
		localDSN = u.String()
	}
	db, err := sql.Open("postgres", localDSN)
	require.NoError(t, err)
	db.SetMaxOpenConns(12)
	client := dbent.NewClient(dbent.Driver(entsql.OpenDB(dialect.Postgres, db)))
	svc := service.NewSubscriptionService(nil, NewUserSubscriptionRepository(client), nil, client, nil)
	t.Cleanup(func() {
		svc.Stop()
		_ = client.Close()
		_, _ = base.Exec(`DROP SCHEMA ` + schema + ` CASCADE`)
		_ = base.Close()
	})
	_, err = db.Exec(`
CREATE TABLE users(id BIGINT PRIMARY KEY);
CREATE TABLE groups(id BIGINT PRIMARY KEY,status TEXT NOT NULL DEFAULT 'active',subscription_type TEXT NOT NULL DEFAULT 'subscription',deleted_at TIMESTAMPTZ);
CREATE TABLE user_subscriptions (
id BIGSERIAL PRIMARY KEY,user_id BIGINT NOT NULL REFERENCES users(id),group_id BIGINT NOT NULL REFERENCES groups(id),
starts_at TIMESTAMPTZ NOT NULL,expires_at TIMESTAMPTZ NOT NULL,status TEXT NOT NULL DEFAULT 'active',
daily_window_start TIMESTAMPTZ,weekly_window_start TIMESTAMPTZ,monthly_window_start TIMESTAMPTZ,
daily_usage_usd NUMERIC NOT NULL DEFAULT 0,weekly_usage_usd NUMERIC NOT NULL DEFAULT 0,monthly_usage_usd NUMERIC NOT NULL DEFAULT 0,
assigned_by BIGINT,assigned_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),notes TEXT,created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),deleted_at TIMESTAMPTZ);
CREATE UNIQUE INDEX renewal_active_sub ON user_subscriptions(user_id,group_id) WHERE deleted_at IS NULL;
CREATE TABLE redeem_codes(id BIGSERIAL PRIMARY KEY,code VARCHAR(32) UNIQUE NOT NULL,type TEXT NOT NULL,status TEXT NOT NULL,used_by BIGINT,group_id BIGINT,validity_days INT,used_at TIMESTAMPTZ);
INSERT INTO users(id) VALUES(10),(11); INSERT INTO groups(id) VALUES(20),(21);
`)
	require.NoError(t, err)
	migration, err := os.ReadFile(filepath.Join("..", "..", "migrations", "253_quya_subscription_renewal_activations.sql"))
	require.NoError(t, err)
	_, err = db.Exec(string(migration))
	require.NoError(t, err)
	// Reapplying the real additive migration must be harmless.
	_, err = db.Exec(string(migration))
	require.NoError(t, err)
	now := time.Now().UTC().Truncate(time.Microsecond)
	in := &service.ActivateSubscriptionRenewalInput{UserID: 10, GroupID: 20, CurrentCode: "store_current", NextCode: "store_next", PendingCodes: []string{"store_queued"}, ExpectedStartsAt: now.AddDate(0, 0, -10), ExpectedExpiresAt: now.AddDate(0, 0, 81), ExpectedNotes: "通过兑换码 store_current 兑换\n通过兑换码 store_next 兑换\n通过兑换码 store_queued 兑换", IdempotencyKey: "renewal-1-next-code"}
	_, err = db.Exec(`INSERT INTO user_subscriptions(id,user_id,group_id,starts_at,expires_at,notes,daily_usage_usd,weekly_usage_usd,monthly_usage_usd) VALUES(1,10,20,$1,$2,$3,9,9,9);`, in.ExpectedStartsAt, in.ExpectedExpiresAt, in.ExpectedNotes)
	require.NoError(t, err)
	_, err = db.Exec(`SELECT setval(pg_get_serial_sequence('user_subscriptions','id'),1)`)
	require.NoError(t, err)
	_, err = db.Exec(`INSERT INTO redeem_codes(code,type,status,used_by,group_id,validity_days,used_at) VALUES('store_current','subscription','used',10,20,30,$1),('store_next','subscription','used',10,20,30,$1),('store_queued','subscription','used',10,20,31,$1)`, now.Add(-time.Hour))
	require.NoError(t, err)
	return db, svc, in
}

func TestRenewalPostgresConcurrentReplayAndLateBilling(t *testing.T) {
	db, svc, in := renewalPostgres(t)
	const n = 8
	results := make([]*service.SubscriptionRenewalActivationReceipt, n)
	errs := make([]error, n)
	start := make(chan struct{})
	var wg sync.WaitGroup
	for i := range n {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			<-start
			results[i], errs[i] = svc.ActivateSubscriptionRenewal(context.Background(), 1, in)
		}(i)
	}
	close(start)
	wg.Wait()
	var newID int64
	fresh := 0
	for i := range n {
		require.NoError(t, errs[i])
		if newID == 0 {
			newID = results[i].NewSubscriptionID
		}
		require.Equal(t, newID, results[i].NewSubscriptionID)
		if !results[i].Replayed {
			fresh++
		}
		require.Equal(t, 61, results[i].TotalValidityDays)
		require.WithinDuration(t, results[i].StartsAt.AddDate(0, 0, 61), results[i].ExpiresAt, time.Microsecond)
	}
	require.Equal(t, 1, fresh)
	var active, receipts int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM user_subscriptions WHERE deleted_at IS NULL`).Scan(&active))
	require.Equal(t, 1, active)
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM subscription_renewal_activations`).Scan(&receipts))
	require.Equal(t, 1, receipts)
	// The exact production updater is also used by batched settlement. Costs
	// completed after replacement stay on old ID 1, never the new zeroed quota.
	tx, err := db.Begin()
	require.NoError(t, err)
	require.NoError(t, incrementUsageBillingSubscription(context.Background(), tx, 1, 2.5))
	require.NoError(t, tx.Commit())
	var oldUsage, newUsage float64
	require.NoError(t, db.QueryRow(`SELECT monthly_usage_usd FROM user_subscriptions WHERE id=1`).Scan(&oldUsage))
	require.NoError(t, db.QueryRow(`SELECT monthly_usage_usd FROM user_subscriptions WHERE id=$1`, newID).Scan(&newUsage))
	require.InDelta(t, 11.5, oldUsage, 0.00001)
	require.Zero(t, newUsage)
	// Ordinary revocation is still rejected for billing and replacement rights
	// are not restored by deleting the newly created subscription.
	require.NoError(t, svc.RevokeSubscription(context.Background(), newID))
	tx, err = db.Begin()
	require.NoError(t, err)
	require.ErrorIs(t, incrementUsageBillingSubscription(context.Background(), tx, newID, 1), service.ErrSubscriptionNotFound)
	_ = tx.Rollback()
	_, err = svc.RestoreSubscription(context.Background(), 1)
	require.Error(t, err)
	_, err = svc.GetRenewalActivation(context.Background(), 1)
	require.NoError(t, err)
}

func TestRenewalPostgresConcurrentDifferentKeysCannotDoubleActivate(t *testing.T) {
	db, svc, in := renewalPostgres(t)
	copy := *in
	copy.IdempotencyKey = "renewal-1-different-key"
	start := make(chan struct{})
	errs := make([]error, 2)
	var wg sync.WaitGroup
	for i, payload := range []*service.ActivateSubscriptionRenewalInput{in, &copy} {
		wg.Add(1)
		go func(i int, payload *service.ActivateSubscriptionRenewalInput) {
			defer wg.Done()
			<-start
			_, errs[i] = svc.ActivateSubscriptionRenewal(context.Background(), 1, payload)
		}(i, payload)
	}
	close(start)
	wg.Wait()
	success := 0
	for _, err := range errs {
		if err == nil {
			success++
		}
	}
	require.Equal(t, 1, success)
	var count int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM subscription_renewal_activations`).Scan(&count))
	require.Equal(t, 1, count)
}

func TestRenewalPostgresSequentialQueuePreservesFull31DayPurchase(t *testing.T) {
	db, svc, in := renewalPostgres(t)
	first, err := svc.ActivateSubscriptionRenewal(context.Background(), 1, in)
	require.NoError(t, err)
	next := *in
	next.CurrentCode = in.NextCode
	next.NextCode = in.PendingCodes[0]
	next.PendingCodes = nil
	next.IdempotencyKey = "renewal-second-queued-code"
	require.NoError(t, db.QueryRow(`SELECT starts_at,expires_at,notes FROM user_subscriptions WHERE id=$1`, first.NewSubscriptionID).Scan(&next.ExpectedStartsAt, &next.ExpectedExpiresAt, &next.ExpectedNotes))
	second, err := svc.ActivateSubscriptionRenewal(context.Background(), first.NewSubscriptionID, &next)
	require.NoError(t, err)
	require.Equal(t, 31, second.TotalValidityDays)
	require.Equal(t, 31, second.CurrentValidityDays)
	require.Empty(t, second.PendingCodes)
	require.WithinDuration(t, second.StartsAt.AddDate(0, 0, 31), second.ExpiresAt, time.Microsecond)
	var active, receipts int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM user_subscriptions WHERE deleted_at IS NULL`).Scan(&active))
	require.Equal(t, 1, active)
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM subscription_renewal_activations`).Scan(&receipts))
	require.Equal(t, 2, receipts)
}

func TestRenewalPostgresLegacyRevokeCannotWinAfterActivation(t *testing.T) {
	_, svc, in := renewalPostgres(t)
	start := make(chan struct{})
	errs := make([]error, 2)
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		<-start
		_, errs[0] = svc.ActivateSubscriptionRenewal(context.Background(), 1, in)
	}()
	go func() { defer wg.Done(); <-start; errs[1] = svc.RevokeSubscription(context.Background(), 1) }()
	close(start)
	wg.Wait()
	success := 0
	for _, err := range errs {
		if err == nil {
			success++
		}
	}
	require.Equal(t, 1, success, "only one operation may claim the old subscription")
}

func TestRenewalPostgresRollbackBoundaryAndForeignPurchase(t *testing.T) {
	for _, mode := range []string{"receipt_failure", "unused_code", "foreign_user", "31_days", "already_revoked"} {
		t.Run(mode, func(t *testing.T) {
			db, svc, in := renewalPostgres(t)
			switch mode {
			case "receipt_failure":
				_, err := db.Exec(`ALTER TABLE subscription_renewal_activations ADD CONSTRAINT injected_failure CHECK(next_code <> 'store_next')`)
				require.NoError(t, err)
			case "unused_code":
				_, err := db.Exec(`UPDATE redeem_codes SET status='unused' WHERE code='store_next'`)
				require.NoError(t, err)
			case "foreign_user":
				_, err := db.Exec(`UPDATE redeem_codes SET used_by=11 WHERE code='store_next'`)
				require.NoError(t, err)
			case "31_days":
				in.ExpectedExpiresAt = time.Now().UTC().Truncate(time.Microsecond).Add(31 * 24 * time.Hour)
				_, err := db.Exec(`UPDATE user_subscriptions SET expires_at=$1 WHERE id=1`, in.ExpectedExpiresAt)
				require.NoError(t, err)
			case "already_revoked":
				require.NoError(t, svc.RevokeSubscription(context.Background(), 1))
			}
			_, err := svc.ActivateSubscriptionRenewal(context.Background(), 1, in)
			require.Error(t, err)
			var rows, receipts int
			require.NoError(t, db.QueryRow(`SELECT count(*) FROM user_subscriptions`).Scan(&rows))
			require.Equal(t, 1, rows)
			require.NoError(t, db.QueryRow(`SELECT count(*) FROM subscription_renewal_activations`).Scan(&receipts))
			require.Zero(t, receipts)
			if mode != "already_revoked" {
				var active int
				require.NoError(t, db.QueryRow(`SELECT count(*) FROM user_subscriptions WHERE deleted_at IS NULL`).Scan(&active))
				require.Equal(t, 1, active)
			}
		})
	}
}
