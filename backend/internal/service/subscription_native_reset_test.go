package service

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/DATA-DOG/go-sqlmock"
	"github.com/stretchr/testify/require"
)

func TestNativeSubscriptionResetPreviewContractAndBoundary(t *testing.T) {
	svc, mock, in, now := renewalFixture(t)
	n := nativeSubscriptionNode{ID: 123, UserID: in.UserID, GroupID: in.GroupID, StartsAt: in.ExpectedStartsAt, ExpiresAt: in.ExpectedExpiresAt, Notes: in.ExpectedNotes, Status: "active"}
	expectNativeRenewalSources(mock, in, now)
	p, input, err := svc.nativeResetPreviewLocked(context.Background(), svc.entClient, n, "Test plan", true)
	require.NoError(t, err)
	require.True(t, p.Eligible)
	require.Equal(t, 2, p.PendingCount)
	require.Equal(t, 1, p.RemainingPendingCount)
	require.Equal(t, 61, p.TotalValidityDays)
	require.Equal(t, "o_next", p.NextOrderNo)
	require.Equal(t, renewalActivationHash(123, input), p.PreviewToken)
	require.Equal(t, int64(20*86400), p.DiscardedRemainingSeconds)
	require.Equal(t, now.AddDate(0, 0, 61), *p.NewExpiresAt)
	raw, err := json.Marshal(p)
	require.NoError(t, err)
	require.NotContains(t, string(raw), "store_")
	n.ExpiresAt = now.Add(31 * 24 * time.Hour)
	p, input, err = svc.nativeResetPreviewLocked(context.Background(), svc.entClient, n, "Test plan", true)
	require.NoError(t, err)
	require.False(t, p.Eligible)
	require.Nil(t, input)
	require.Equal(t, "REMAINING_DAYS_TOO_SHORT", p.ReasonCode)
	require.NoError(t, mock.ExpectationsWereMet())
}

func expectOwnedNativeRow(mock sqlmock.Sqlmock, in *ActivateSubscriptionRenewalInput, userID int64) {
	mock.ExpectBegin()
	mock.ExpectExec(`SELECT pg_advisory_xact_lock`).WithArgs(userID).WillReturnResult(sqlmock.NewResult(0, 1))
	rows := sqlmock.NewRows([]string{"id", "user_id", "group_id", "starts_at", "expires_at", "status", "notes", "deleted_at", "name", "group_ok"})
	if userID == in.UserID {
		rows.AddRow(123, in.UserID, in.GroupID, in.ExpectedStartsAt, in.ExpectedExpiresAt, "active", in.ExpectedNotes, nil, "Test plan", true)
	}
	mock.ExpectQuery(`SELECT s.id,s.user_id,s.group_id`).WithArgs(int64(123), userID).WillReturnRows(rows)
}

func TestNativeSubscriptionResetBindsUserAndRequiresRules(t *testing.T) {
	svc, mock, in, _ := renewalFixture(t)
	_, err := svc.ResetUserSubscription(context.Background(), in.UserID, 123, strings.Repeat("a", 64), false)
	require.Error(t, err)
	expectOwnedNativeRow(mock, in, 11)
	mock.ExpectRollback()
	_, err = svc.ResetUserSubscription(context.Background(), 11, 123, strings.Repeat("a", 64), true)
	require.ErrorIs(t, err, ErrSubscriptionNotFound)
	require.NoError(t, mock.ExpectationsWereMet())
}

func TestNativeSubscriptionResetStalePreviewRollsBack(t *testing.T) {
	svc, mock, in, now := renewalFixture(t)
	expectOwnedNativeRow(mock, in, in.UserID)
	expectNoPriorRenewal(mock)
	expectNativeRenewalSources(mock, in, now)
	mock.ExpectRollback()
	_, err := svc.ResetUserSubscription(context.Background(), in.UserID, 123, strings.Repeat("a", 64), true)
	require.ErrorContains(t, err, "重新查看重置预览")
	require.NoError(t, mock.ExpectationsWereMet())
}

func TestNativeSubscriptionResetReplaysWithoutReadingChangedPurchases(t *testing.T) {
	svc, mock, in, now := renewalFixture(t)
	svc.autoResetEnabled = true
	token := strings.Repeat("a", 64)
	expectOwnedNativeRow(mock, in, in.UserID)
	raw, _ := json.Marshal(SubscriptionRenewalActivationReceipt{OldSubscriptionID: 123, NewSubscriptionID: 124, UserID: 10, GroupID: 20, PreviousCode: in.CurrentCode, CurrentCode: in.NextCode, PendingCodes: in.PendingCodes, StartsAt: now, ExpiresAt: now.AddDate(0, 0, 61), IdempotencyKey: "user-reset:" + token, CurrentValidityDays: 30, TotalValidityDays: 61})
	mock.ExpectQuery(`SELECT receipt, request_hash FROM subscription_renewal_activations`).WithArgs(int64(123)).WillReturnRows(sqlmock.NewRows([]string{"receipt", "request_hash"}).AddRow(raw, token))
	mock.ExpectCommit()
	r, err := svc.ResetUserSubscription(context.Background(), in.UserID, 123, token, true)
	require.NoError(t, err)
	require.True(t, r.Replayed)
	require.EqualValues(t, 124, r.NewSubscriptionID)
	data, err := json.Marshal(r)
	require.NoError(t, err)
	require.NotContains(t, string(data), "store_")
	require.NoError(t, mock.ExpectationsWereMet())
}
