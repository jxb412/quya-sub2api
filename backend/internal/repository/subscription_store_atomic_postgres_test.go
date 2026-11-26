package repository

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"sync"
	"testing"
	"time"

	infraerrors "github.com/Wei-Shaw/sub2api/internal/pkg/errors"
	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/stretchr/testify/require"
)

func storeAtomicInput(in *service.ActivateSubscriptionRenewalInput) *service.ReplaceStoreSubscriptionInput {
	notes := in.ExpectedNotes
	return &service.ReplaceStoreSubscriptionInput{
		UserID: in.UserID, GroupID: in.GroupID, ValidityDays: 61,
		Notes:            legacyStoreNotes(1, "store_next", "store_queued"),
		ExpectedStartsAt: in.ExpectedStartsAt, ExpectedExpiresAt: in.ExpectedExpiresAt, ExpectedNotes: &notes,
	}
}

type storeAtomicRowSnapshot struct {
	StartsAt, ExpiresAt, UpdatedAt time.Time
	Status, Notes                  string
	DeletedAt                      sql.NullTime
	Daily, Weekly, Monthly         float64
}

func storeAtomicRow(t *testing.T, db *sql.DB, id int64) storeAtomicRowSnapshot {
	t.Helper()
	var row storeAtomicRowSnapshot
	require.NoError(t, db.QueryRow(`SELECT starts_at,expires_at,updated_at,status,COALESCE(notes,''),deleted_at,daily_usage_usd,weekly_usage_usd,monthly_usage_usd FROM user_subscriptions WHERE id=$1`, id).
		Scan(&row.StartsAt, &row.ExpiresAt, &row.UpdatedAt, &row.Status, &row.Notes, &row.DeletedAt, &row.Daily, &row.Weekly, &row.Monthly))
	return row
}

func storeAtomicCounts(t *testing.T, db *sql.DB, wantRows, wantActive, wantReceipts int) {
	t.Helper()
	var rows, active, receipts int
	require.NoError(t, db.QueryRow(`SELECT count(*),count(*) FILTER(WHERE deleted_at IS NULL) FROM user_subscriptions`).Scan(&rows, &active))
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM subscription_renewal_activations`).Scan(&receipts))
	require.Equal(t, wantRows, rows)
	require.Equal(t, wantActive, active)
	require.Equal(t, wantReceipts, receipts)
}

// 2026-11-26: replacement must atomically preserve paid rights and acknowledge
// retries without resetting the newly used quota or restoring an old period.
func TestLegacyStorePostgresAtomicReplacementDurableReplayAndLateBilling(t *testing.T) {
	db, svc, _, source := legacyStorePostgres(t)
	ctx := context.Background()
	in := storeAtomicInput(source)
	old := storeAtomicRow(t, db, 1)
	result, err := svc.ReplaceStoreSubscription(ctx, 1, in)
	require.NoError(t, err)
	require.False(t, result.Replayed)
	require.EqualValues(t, 1, result.OldSubscriptionID)
	newID := result.Subscription.ID
	require.NotEqualValues(t, 1, newID)
	storeAtomicCounts(t, db, 2, 1, 1)
	prior := storeAtomicRow(t, db, 1)
	require.True(t, prior.DeletedAt.Valid)
	require.Equal(t, old.Monthly, prior.Monthly)
	require.Equal(t, old.ExpiresAt, prior.ExpiresAt)
	current := storeAtomicRow(t, db, newID)
	require.Zero(t, current.Monthly)
	require.Zero(t, current.Daily)
	require.Zero(t, current.Weekly)
	require.Equal(t, 61*24*time.Hour, current.ExpiresAt.Sub(current.StartsAt))
	receipt, err := svc.GetRenewalActivation(ctx, 1)
	require.NoError(t, err)
	require.Equal(t, newID, receipt.NewSubscriptionID)
	require.Equal(t, "store_current", receipt.PreviousCode)
	require.Equal(t, "store_next", receipt.CurrentCode)
	require.Equal(t, []string{"store_queued"}, receipt.PendingCodes)
	require.Equal(t, 30, receipt.CurrentValidityDays)
	require.Equal(t, 61, receipt.TotalValidityDays)
	require.Equal(t, "store-atomic:1", receipt.IdempotencyKey)
	require.True(t, current.StartsAt.Equal(receipt.StartsAt), "receipt must preserve the stored start instant across timezone encodings")
	require.True(t, current.ExpiresAt.Equal(receipt.ExpiresAt), "receipt must preserve the stored expiry instant across timezone encodings")

	_, err = db.Exec(`UPDATE user_subscriptions SET monthly_usage_usd=7 WHERE id=$1`, newID)
	require.NoError(t, err)
	used := storeAtomicRow(t, db, newID)
	retry := *in
	retry.AssignedBy = 11 // A different authenticated administrator is not a new operation.
	again, err := svc.ReplaceStoreSubscription(ctx, 1, &retry)
	require.NoError(t, err)
	require.True(t, again.Replayed)
	require.Equal(t, newID, again.Subscription.ID)
	require.Equal(t, used, storeAtomicRow(t, db, newID))
	storeAtomicCounts(t, db, 2, 1, 1)

	tx, err := db.Begin()
	require.NoError(t, err)
	require.NoError(t, incrementUsageBillingSubscription(ctx, tx, 1, 2.5))
	require.NoError(t, tx.Commit())
	require.InDelta(t, old.Monthly+2.5, storeAtomicRow(t, db, 1).Monthly, 0.00001)
	require.Equal(t, 7.0, storeAtomicRow(t, db, newID).Monthly)

	require.NoError(t, svc.RevokeSubscription(ctx, newID))
	again, err = svc.ReplaceStoreSubscription(ctx, 1, in)
	require.NoError(t, err)
	require.True(t, again.Replayed)
	require.Equal(t, newID, again.Subscription.ID)
	storeAtomicCounts(t, db, 2, 0, 1)
	_, err = svc.RestoreSubscription(ctx, 1)
	require.ErrorContains(t, err, "consumed", "durable receipt must block restoration of surrendered rights")
	changed := *in
	changed.ValidityDays = 60
	_, err = svc.ReplaceStoreSubscription(ctx, 1, &changed)
	require.True(t, infraerrors.IsConflict(err))
	storeAtomicCounts(t, db, 2, 0, 1)
}

func TestLegacyStorePostgresAtomicValidationAndWriteFailureRollback(t *testing.T) {
	for _, mode := range []string{"wrong_days", "wrong_current", "refunded", "insert_failure", "receipt_failure"} {
		t.Run(mode, func(t *testing.T) {
			db, svc, _, source := legacyStorePostgres(t)
			in := storeAtomicInput(source)
			switch mode {
			case "wrong_days":
				in.ValidityDays = 60
			case "wrong_current":
				in.Notes = legacyStoreNotes(1, "store_queued", "store_next")
			case "refunded":
				refund := legacyStoreRefund("store_next", 9)
				_, err := db.Exec(`INSERT INTO redeem_codes(code,type,status,value,notes) VALUES($1,'balance','unused',$2,$3)`, refund.Code, refund.Value, refund.Notes)
				require.NoError(t, err)
			case "insert_failure", "receipt_failure":
				table := "user_subscriptions"
				if mode == "receipt_failure" {
					table = "subscription_renewal_activations"
				}
				_, err := db.Exec(`CREATE FUNCTION reject_atomic_test_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected test failure'; END $$; CREATE TRIGGER reject_atomic_insert BEFORE INSERT ON ` + table + ` FOR EACH ROW EXECUTE FUNCTION reject_atomic_test_insert();`)
				require.NoError(t, err)
			}
			before := storeAtomicRow(t, db, 1)
			_, err := svc.ReplaceStoreSubscription(context.Background(), 1, in)
			require.Error(t, err)
			require.Equal(t, before, storeAtomicRow(t, db, 1), "failed creation must restore the entire original row")
			storeAtomicCounts(t, db, 1, 1, 0)
		})
	}
}

func TestLegacyStorePostgresAtomicPreservesManualSubscription(t *testing.T) {
	for _, oldDeleted := range []bool{false, true} {
		t.Run(fmt.Sprintf("old_deleted_%t", oldDeleted), func(t *testing.T) {
			db, svc, _, source := legacyStorePostgres(t)
			if oldDeleted {
				require.NoError(t, svc.RevokeSubscription(context.Background(), 1))
			}
			var manualID int64
			require.NoError(t, db.QueryRow(`INSERT INTO user_subscriptions(user_id,group_id,starts_at,expires_at,status,notes,monthly_usage_usd) VALUES(10,20,NOW(),NOW()+INTERVAL '30 days','active','管理员已手动处理',8) RETURNING id`).Scan(&manualID))
			old, manual := storeAtomicRow(t, db, 1), storeAtomicRow(t, db, manualID)
			_, err := svc.ReplaceStoreSubscription(context.Background(), 1, storeAtomicInput(source))
			require.ErrorContains(t, err, "已有其他有效套餐")
			require.Equal(t, old, storeAtomicRow(t, db, 1))
			require.Equal(t, manual, storeAtomicRow(t, db, manualID))
			active := 2
			if oldDeleted {
				active = 1
			}
			storeAtomicCounts(t, db, 2, active, 0)
		})
	}
}

func TestLegacyStorePostgresAtomicRecoversDeletedPendingAndLegacySuccessor(t *testing.T) {
	for _, legacyDone := range []bool{false, true} {
		t.Run(fmt.Sprintf("legacy_done_%t", legacyDone), func(t *testing.T) {
			db, svc, _, source := legacyStorePostgres(t)
			ctx := context.Background()
			in := storeAtomicInput(source)
			require.NoError(t, svc.RevokeSubscription(ctx, 1))
			var priorID int64
			if legacyDone {
				prior, err := svc.AssignSubscription(ctx, &service.AssignSubscriptionInput{UserID: in.UserID, GroupID: in.GroupID, ValidityDays: in.ValidityDays, Notes: in.Notes})
				require.NoError(t, err)
				priorID = prior.ID
				_, err = db.Exec(`UPDATE user_subscriptions SET monthly_usage_usd=13 WHERE id=$1`, priorID)
				require.NoError(t, err)
			}
			result, err := svc.ReplaceStoreSubscription(ctx, 1, in)
			require.NoError(t, err)
			require.Equal(t, legacyDone, result.Replayed)
			if legacyDone {
				require.Equal(t, priorID, result.Subscription.ID)
				require.Equal(t, 13.0, storeAtomicRow(t, db, priorID).Monthly)
				storeAtomicCounts(t, db, 2, 1, 0)
			} else {
				storeAtomicCounts(t, db, 2, 1, 1)
			}
		})
	}
}

func TestLegacyStorePostgresAtomicRejectsStaleSnapshotWithoutMutation(t *testing.T) {
	for _, mode := range []string{"starts", "expires", "notes", "ownership"} {
		t.Run(mode, func(t *testing.T) {
			db, svc, _, source := legacyStorePostgres(t)
			in := storeAtomicInput(source)
			switch mode {
			case "starts":
				in.ExpectedStartsAt = in.ExpectedStartsAt.Add(time.Second)
			case "expires":
				in.ExpectedExpiresAt = in.ExpectedExpiresAt.Add(time.Second)
			case "notes":
				other := *in.ExpectedNotes + "\nnew purchase"
				in.ExpectedNotes = &other
			case "ownership":
				in.UserID = 11
			}
			before := storeAtomicRow(t, db, 1)
			_, err := svc.ReplaceStoreSubscription(context.Background(), 1, in)
			require.True(t, infraerrors.IsConflict(err), "%v", err)
			require.Equal(t, before, storeAtomicRow(t, db, 1))
			storeAtomicCounts(t, db, 1, 1, 0)
		})
	}
}

func TestLegacyStorePostgresAtomicConcurrentRetriesIssueOnce(t *testing.T) {
	db, svc, _, source := legacyStorePostgres(t)
	in := storeAtomicInput(source)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	const count = 4
	results := make([]*service.ReplaceStoreSubscriptionResult, count)
	errs := make([]error, count)
	start := make(chan struct{})
	var wg sync.WaitGroup
	for index := range count {
		wg.Add(1)
		go func(index int) {
			defer wg.Done()
			<-start
			results[index], errs[index] = svc.ReplaceStoreSubscription(ctx, 1, in)
		}(index)
	}
	close(start)
	wg.Wait()
	require.NoError(t, ctx.Err())
	var newID int64
	fresh := 0
	for i, err := range errs {
		require.NoError(t, err)
		if newID == 0 {
			newID = results[i].Subscription.ID
		}
		require.Equal(t, newID, results[i].Subscription.ID)
		if !results[i].Replayed {
			fresh++
		}
	}
	require.Equal(t, 1, fresh)
	storeAtomicCounts(t, db, 2, 1, 1)
}

func TestLegacyStorePostgresAtomicConcurrentNativeResetAndRefund(t *testing.T) {
	for _, competitor := range []string{"native_reset", "refund", "manual_grant"} {
		t.Run(competitor, func(t *testing.T) {
			db, svc, redeem, source := legacyStorePostgres(t)
			in := storeAtomicInput(source)
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()
			errs := make([]error, 2)
			start := make(chan struct{})
			var wg sync.WaitGroup
			wg.Add(2)
			go func() { defer wg.Done(); <-start; _, errs[0] = svc.ReplaceStoreSubscription(ctx, 1, in) }()
			go func() {
				defer wg.Done()
				<-start
				switch competitor {
				case "native_reset":
					_, errs[1] = svc.ActivateSubscriptionRenewal(ctx, 1, source)
				case "refund":
					_, errs[1] = redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund("store_next", 9))
				case "manual_grant":
					_, errs[1] = svc.AssignSubscription(ctx, &service.AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 7, Notes: "管理员并发赠送"})
				}
			}()
			close(start)
			wg.Wait()
			require.NoError(t, ctx.Err())
			if competitor == "native_reset" {
				require.NotEqual(t, errs[0] == nil, errs[1] == nil)
			} else {
				require.NoError(t, errs[0])
				require.Error(t, errs[1])
			}
			for _, err := range errs {
				if err != nil {
					if competitor == "manual_grant" && errors.Is(err, service.ErrSubscriptionNotFound) {
						// A manual grant may read the old active row immediately before
						// replacement commits; its subsequent lookup must not issue anew.
						continue
					}
					require.True(t, infraerrors.IsConflict(err), "competitor must fail with a domain conflict: %v", err)
				}
			}
			storeAtomicCounts(t, db, 2, 1, 1)
			var balance float64
			require.NoError(t, db.QueryRow(`SELECT balance FROM users WHERE id=10`).Scan(&balance))
			require.Zero(t, balance)
		})
	}
}

func TestLegacyStorePostgresAtomicDoesNotInheritSelfReset31DayGate(t *testing.T) {
	db, svc, _, source := legacyStorePostgres(t)
	now := time.Now().UTC().Truncate(time.Microsecond)
	source.ExpectedStartsAt = now.AddDate(0, 0, -86)
	source.ExpectedExpiresAt = now.AddDate(0, 0, 5)
	_, err := db.Exec(`UPDATE user_subscriptions SET starts_at=$1,expires_at=$2 WHERE id=1`, source.ExpectedStartsAt, source.ExpectedExpiresAt)
	require.NoError(t, err)
	_, err = db.Exec(`UPDATE redeem_codes SET used_at=$1 WHERE code='store_current'`, source.ExpectedStartsAt)
	require.NoError(t, err)
	result, err := svc.ReplaceStoreSubscription(context.Background(), 1, storeAtomicInput(source))
	require.NoError(t, err)
	require.False(t, result.Replayed)
	storeAtomicCounts(t, db, 2, 1, 1)
}
