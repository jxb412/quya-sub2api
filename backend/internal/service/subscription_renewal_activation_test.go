package service

import (
	"context"
	"encoding/json"
	"errors"
	"testing"
	"time"

	"entgo.io/ent/dialect"
	entsql "entgo.io/ent/dialect/sql"
	"github.com/DATA-DOG/go-sqlmock"
	dbent "github.com/Wei-Shaw/sub2api/ent"
	"github.com/stretchr/testify/require"
)

func renewalFixture(t *testing.T) (*SubscriptionService, sqlmock.Sqlmock, *ActivateSubscriptionRenewalInput, time.Time) {
	t.Helper()
	db, mock, err := sqlmock.New()
	require.NoError(t, err)
	client := dbent.NewClient(dbent.Driver(entsql.OpenDB(dialect.Postgres, db)))
	t.Cleanup(func() { _ = client.Close() })
	now := time.Date(2026, 11, 24, 10, 0, 0, 0, time.UTC)
	svc := &SubscriptionService{entClient: client, now: func() time.Time { return now }}
	in := &ActivateSubscriptionRenewalInput{UserID: 10, GroupID: 20, CurrentCode: "store_current", NextCode: "store_next", PendingCodes: []string{"store_queued"}, ExpectedStartsAt: now.AddDate(0, 0, -10), ExpectedExpiresAt: now.AddDate(0, 0, 81), ExpectedNotes: "通过兑换码 store_current 兑换\n通过兑换码 store_next 兑换\n通过兑换码 store_queued 兑换", IdempotencyKey: "renewal-123-next-purchase"}
	return svc, mock, in, now
}

func expectRenewalRow(mock sqlmock.Sqlmock, in *ActivateSubscriptionRenewalInput, expires time.Time, deleted any) {
	mock.ExpectBegin()
	mock.ExpectExec(`SELECT pg_advisory_xact_lock`).WithArgs(in.UserID).WillReturnResult(sqlmock.NewResult(0, 1))
	mock.ExpectQuery(`SELECT user_id, group_id, status, starts_at, expires_at`).WithArgs(int64(123)).WillReturnRows(sqlmock.NewRows([]string{"user_id", "group_id", "status", "starts_at", "expires_at", "notes", "deleted_at"}).AddRow(in.UserID, in.GroupID, "active", in.ExpectedStartsAt, expires, in.ExpectedNotes, deleted))
}

func expectNoPriorRenewal(mock sqlmock.Sqlmock) {
	mock.ExpectQuery(`SELECT receipt, request_hash FROM subscription_renewal_activations`).WithArgs(int64(123)).WillReturnRows(sqlmock.NewRows([]string{"receipt", "request_hash"}))
}

func expectNativeRenewalSources(mock sqlmock.Sqlmock, in *ActivateSubscriptionRenewalInput, now time.Time) {
	mock.ExpectQuery(`SELECT id,user_id,group_id,starts_at`).WithArgs(in.UserID, in.GroupID).WillReturnRows(sqlmock.NewRows([]string{"id", "user_id", "group_id", "starts_at", "expires_at", "status", "notes", "deleted_at"}).AddRow(123, in.UserID, in.GroupID, in.ExpectedStartsAt, in.ExpectedExpiresAt, "active", in.ExpectedNotes, nil))
	mock.ExpectQuery(`SELECT id,code,type,status,used_by`).WillReturnRows(sqlmock.NewRows([]string{"id", "code", "type", "status", "used_by", "group_id", "validity_days", "used_at", "notes"}).AddRow(1, in.CurrentCode, "subscription", "used", in.UserID, in.GroupID, 30, in.ExpectedStartsAt, "store order o_current").AddRow(2, in.NextCode, "subscription", "used", in.UserID, in.GroupID, 30, now.AddDate(0, 0, -3), "store order o_next").AddRow(3, in.PendingCodes[0], "subscription", "used", in.UserID, in.GroupID, 31, now.AddDate(0, 0, -1), "store order o_queued"))
	mock.ExpectQuery(`SELECT receipt FROM subscription_renewal_activations`).WithArgs(in.UserID, in.GroupID).WillReturnRows(sqlmock.NewRows([]string{"receipt"}))
}

func expectRedeemedRenewalCodes(mock sqlmock.Sqlmock, in *ActivateSubscriptionRenewalInput, now time.Time, nextDays int, nextStatus string) {
	expectNativeRenewalSources(mock, in, now)
	mock.ExpectQuery(`SELECT id FROM groups`).WithArgs(in.GroupID).WillReturnRows(sqlmock.NewRows([]string{"id"}).AddRow(in.GroupID))
	mock.ExpectQuery(`SELECT code, type, status, used_by`).WillReturnRows(sqlmock.NewRows([]string{"code", "type", "status", "used_by", "group_id", "validity_days", "used_at"}).
		AddRow(in.CurrentCode, "subscription", "used", in.UserID, in.GroupID, 30, now.AddDate(0, 0, -10)).
		AddRow(in.NextCode, "subscription", nextStatus, in.UserID, in.GroupID, nextDays, now.AddDate(0, 0, -3)).
		AddRow(in.PendingCodes[0], "subscription", "used", in.UserID, in.GroupID, 31, now.AddDate(0, 0, -1)))
}

func TestActivateRenewalAtomicAndQueuesFullPurchasedTerms(t *testing.T) {
	svc, mock, in, now := renewalFixture(t)
	expectRenewalRow(mock, in, in.ExpectedExpiresAt, nil)
	expectNoPriorRenewal(mock)
	expectRedeemedRenewalCodes(mock, in, now, 30, "used")
	mock.ExpectQuery(`SELECT old_subscription_id FROM subscription_renewal_activations`).WillReturnRows(sqlmock.NewRows([]string{"old_subscription_id"}))
	mock.ExpectExec(`UPDATE user_subscriptions SET deleted_at`).WithArgs(int64(123), now).WillReturnResult(sqlmock.NewResult(0, 1))
	mock.ExpectQuery(`INSERT INTO user_subscriptions`).WithArgs(in.UserID, in.GroupID, now, now.AddDate(0, 0, 61), sqlmock.AnyArg(), "通过兑换码 store_next 兑换\n套餐当前购买码: store_next\n套餐待启用购买码: store_queued\n套餐循环检查自动重开，原订阅ID 123").WillReturnRows(sqlmock.NewRows([]string{"id"}).AddRow(124))
	mock.ExpectExec(`INSERT INTO subscription_renewal_activations`).WithArgs(int64(123), int64(124), in.UserID, in.GroupID, in.NextCode, in.IdempotencyKey, renewalActivationHash(123, in), sqlmock.AnyArg(), now).WillReturnResult(sqlmock.NewResult(0, 1))
	mock.ExpectCommit()
	r, err := svc.ActivateSubscriptionRenewal(context.Background(), 123, in)
	require.NoError(t, err)
	require.Equal(t, 61, r.TotalValidityDays)
	require.Equal(t, 30, r.CurrentValidityDays)
	require.Equal(t, now.AddDate(0, 0, 61), r.ExpiresAt)
	require.Equal(t, []string{"store_queued"}, r.PendingCodes)
	require.False(t, r.Replayed)
	require.NoError(t, mock.ExpectationsWereMet())
}

func TestActivateRenewalRejects31DayBoundaryAndStalePreview(t *testing.T) {
	for _, mode := range []string{"exactly31", "under31", "changedExpiry", "changedNotes", "revoked"} {
		t.Run(mode, func(t *testing.T) {
			svc, mock, in, now := renewalFixture(t)
			exp := in.ExpectedExpiresAt
			var deleted any
			switch mode {
			case "exactly31":
				exp = now.Add(31 * 24 * time.Hour)
				in.ExpectedExpiresAt = exp
			case "under31":
				exp = now.Add(31*24*time.Hour - time.Nanosecond)
				in.ExpectedExpiresAt = exp
			case "changedExpiry":
				exp = exp.AddDate(0, 0, 30)
			case "revoked":
				deleted = now
			}
			expectRenewalRow(mock, in, exp, deleted)
			expectNoPriorRenewal(mock)
			if mode == "changedNotes" {
				in.ExpectedNotes = "changed preview"
			}
			mock.ExpectRollback()
			_, err := svc.ActivateSubscriptionRenewal(context.Background(), 123, in)
			require.Error(t, err)
			require.NoError(t, mock.ExpectationsWereMet())
		})
	}
}

func TestActivateRenewalRejectsUnusedOrNonMonthlyPurchase(t *testing.T) {
	for _, tc := range []struct {
		name   string
		days   int
		status string
	}{{"unused", 30, "unused"}, {"annual", 365, "used"}} {
		t.Run(tc.name, func(t *testing.T) {
			svc, mock, in, now := renewalFixture(t)
			expectRenewalRow(mock, in, in.ExpectedExpiresAt, nil)
			expectNoPriorRenewal(mock)
			expectRedeemedRenewalCodes(mock, in, now, tc.days, tc.status)
			mock.ExpectRollback()
			_, err := svc.ActivateSubscriptionRenewal(context.Background(), 123, in)
			require.Error(t, err)
			require.NoError(t, mock.ExpectationsWereMet())
		})
	}
}

func TestActivateRenewalDurableReplayAndConflictingRequest(t *testing.T) {
	for _, conflict := range []bool{false, true} {
		t.Run(map[bool]string{false: "replay", true: "conflict"}[conflict], func(t *testing.T) {
			svc, mock, in, now := renewalFixture(t)
			svc.autoResetEnabled = true // Historical receipts remain replayable after a config change.
			expectRenewalRow(mock, in, in.ExpectedExpiresAt, now)
			raw, _ := json.Marshal(SubscriptionRenewalActivationReceipt{OldSubscriptionID: 123, NewSubscriptionID: 124, UserID: 10, GroupID: 20, CurrentCode: in.NextCode, IdempotencyKey: in.IdempotencyKey})
			hash := renewalActivationHash(123, in)
			mock.ExpectQuery(`SELECT receipt, request_hash FROM subscription_renewal_activations`).WithArgs(int64(123)).WillReturnRows(sqlmock.NewRows([]string{"receipt", "request_hash"}).AddRow(raw, hash))
			if conflict {
				in.NextCode = "store_other"
				mock.ExpectRollback()
			} else {
				mock.ExpectCommit()
			}
			r, err := svc.ActivateSubscriptionRenewal(context.Background(), 123, in)
			if conflict {
				require.Error(t, err)
			} else {
				require.NoError(t, err)
				require.True(t, r.Replayed)
				require.EqualValues(t, 124, r.NewSubscriptionID)
			}
			require.NoError(t, mock.ExpectationsWereMet())
		})
	}
}

func TestActivateRenewalRefusesAutomaticQuotaResets(t *testing.T) {
	svc, mock, in, _ := renewalFixture(t)
	svc.autoResetEnabled = true
	expectRenewalRow(mock, in, in.ExpectedExpiresAt, nil)
	expectNoPriorRenewal(mock)
	mock.ExpectRollback()
	_, err := svc.ActivateSubscriptionRenewal(context.Background(), 123, in)
	require.ErrorContains(t, err, "automatic quota reset to be disabled")
	require.NoError(t, mock.ExpectationsWereMet())
}

func TestReadRenewalReceiptRemainsAvailableWithAutomaticQuotaResets(t *testing.T) {
	svc, mock, _, _ := renewalFixture(t)
	svc.autoResetEnabled = true
	raw, err := json.Marshal(SubscriptionRenewalActivationReceipt{OldSubscriptionID: 123, NewSubscriptionID: 124})
	require.NoError(t, err)
	mock.ExpectQuery(`SELECT receipt, request_hash FROM subscription_renewal_activations`).WithArgs(int64(123)).WillReturnRows(sqlmock.NewRows([]string{"receipt", "request_hash"}).AddRow(raw, "hash"))
	r, err := svc.GetRenewalActivation(context.Background(), 123)
	require.NoError(t, err)
	require.EqualValues(t, 124, r.NewSubscriptionID)
	require.NoError(t, mock.ExpectationsWereMet())
}

func TestActivateRenewalRollsBackReplacementWhenReceiptCannotPersist(t *testing.T) {
	svc, mock, in, now := renewalFixture(t)
	expectRenewalRow(mock, in, in.ExpectedExpiresAt, nil)
	expectNoPriorRenewal(mock)
	expectRedeemedRenewalCodes(mock, in, now, 30, "used")
	mock.ExpectQuery(`SELECT old_subscription_id FROM subscription_renewal_activations`).WillReturnRows(sqlmock.NewRows([]string{"old_subscription_id"}))
	mock.ExpectExec(`UPDATE user_subscriptions SET deleted_at`).WillReturnResult(sqlmock.NewResult(0, 1))
	mock.ExpectQuery(`INSERT INTO user_subscriptions`).WillReturnRows(sqlmock.NewRows([]string{"id"}).AddRow(124))
	mock.ExpectExec(`INSERT INTO subscription_renewal_activations`).WillReturnError(errors.New("receipt write failed"))
	mock.ExpectRollback()
	_, err := svc.ActivateSubscriptionRenewal(context.Background(), 123, in)
	require.ErrorContains(t, err, "receipt write failed")
	require.NoError(t, mock.ExpectationsWereMet())
}

func TestActivateRenewalInputRejectsDuplicateAndMalformedCodes(t *testing.T) {
	_, _, in, _ := renewalFixture(t)
	copy := *in
	copy.PendingCodes = []string{in.NextCode}
	require.Error(t, validateRenewalActivationInput(123, &copy))
	copy = *in
	copy.NextCode = "ordinary_gift"
	require.Error(t, validateRenewalActivationInput(123, &copy))
	copy = *in
	copy.IdempotencyKey = ""
	require.Error(t, validateRenewalActivationInput(123, &copy))
	copy = *in
	copy.PendingCodes = nil
	require.NoError(t, validateRenewalActivationInput(123, &copy))
}

func TestRenewalHashCanonicalizesTimezoneAndEmptyQueue(t *testing.T) {
	_, _, in, _ := renewalFixture(t)
	in.PendingCodes = nil
	copy := *in
	copy.PendingCodes = []string{}
	copy.ExpectedStartsAt = copy.ExpectedStartsAt.In(time.FixedZone("CST", 8*3600))
	copy.ExpectedExpiresAt = copy.ExpectedExpiresAt.In(time.FixedZone("CST", 8*3600))
	require.Equal(t, renewalActivationHash(123, in), renewalActivationHash(123, &copy))
}

func TestRenewalPeriodPreservesPurchasedDaysAcrossDST(t *testing.T) {
	location, err := time.LoadLocation("America/New_York")
	require.NoError(t, err)
	for _, days := range []int{30, 31, 61} {
		start, end := renewalPeriod(time.Date(2026, 10, 31, 12, 0, 0, 123456789, location), days)
		require.Equal(t, time.Duration(days)*24*time.Hour, end.Sub(start))
		require.Equal(t, time.UTC, start.Location())
		require.Equal(t, 123456000, start.Nanosecond())
	}
}
