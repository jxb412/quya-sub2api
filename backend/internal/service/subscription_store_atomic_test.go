package service

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/DATA-DOG/go-sqlmock"
	infraerrors "github.com/Wei-Shaw/sub2api/internal/pkg/errors"
	"github.com/stretchr/testify/require"
)

func storeAtomicUnitInput(in *ActivateSubscriptionRenewalInput) *ReplaceStoreSubscriptionInput {
	notes := in.ExpectedNotes
	return &ReplaceStoreSubscriptionInput{
		UserID: in.UserID, GroupID: in.GroupID, ValidityDays: 61,
		Notes:            "通过兑换码 store_next 兑换\n套餐当前购买码: store_next\n套餐待启用购买码: store_queued\n套餐循环检查自动重开，原订阅ID 123",
		ExpectedStartsAt: in.ExpectedStartsAt, ExpectedExpiresAt: in.ExpectedExpiresAt, ExpectedNotes: &notes,
	}
}

func TestStoreLegacyAtomicRejectsIncompleteInputBeforeDatabase(t *testing.T) {
	for _, mode := range []string{"nil", "user", "group", "days_zero", "days_large", "starts", "expires", "expected_notes", "large_notes", "marker_missing", "marker_other_parent"} {
		t.Run(mode, func(t *testing.T) {
			svc, mock, source, _ := renewalFixture(t)
			in := storeAtomicUnitInput(source)
			switch mode {
			case "nil":
				in = nil
			case "user":
				in.UserID = 0
			case "group":
				in.GroupID = 0
			case "days_zero":
				in.ValidityDays = 0
			case "days_large":
				in.ValidityDays = MaxValidityDays + 1
			case "starts":
				in.ExpectedStartsAt = time.Time{}
			case "expires":
				in.ExpectedExpiresAt = time.Time{}
			case "expected_notes":
				in.ExpectedNotes = nil
			case "large_notes":
				in.Notes = strings.Repeat("x", 65537)
			case "marker_missing":
				in.Notes = "管理员赠送"
			case "marker_other_parent":
				in.Notes = strings.ReplaceAll(in.Notes, "ID 123", "ID 999")
			}
			_, err := svc.ReplaceStoreSubscription(context.Background(), 123, in)
			require.Error(t, err)
			require.True(t, infraerrors.IsBadRequest(err) || infraerrors.IsConflict(err), "%v", err)
			require.NoError(t, mock.ExpectationsWereMet())
		})
	}
}

type storeAtomicReplayCache struct {
	BillingCache
	invalidations int
	publications  int
	fail          bool
	mock          sqlmock.Sqlmock
	t             *testing.T
}

func (c *storeAtomicReplayCache) InvalidateSubscriptionCache(_ context.Context, userID, groupID int64) error {
	require.NoError(c.t, c.mock.ExpectationsWereMet(), "cache must not be invalidated before the SQL transaction commits")
	require.EqualValues(c.t, 10, userID)
	require.EqualValues(c.t, 20, groupID)
	c.invalidations++
	if c.fail {
		return errors.New("test Redis unavailable")
	}
	return nil
}

func (c *storeAtomicReplayCache) PublishSubscriptionCacheInvalidation(_ context.Context, key string) error {
	require.Equal(c.t, "sub:10:20", key)
	c.publications++
	return nil
}

func (*storeAtomicReplayCache) SubscribeSubscriptionCacheInvalidation(context.Context, func(string)) error {
	return nil
}

// Cache failures can occur after a successful durable transaction. Replaying
// the receipt must retry invalidation instead of issuing another period.
func TestStoreLegacyAtomicDurableReplayRetriesCacheInvalidation(t *testing.T) {
	svc, mock, source, now := renewalFixture(t)
	in := storeAtomicUnitInput(source)
	receipt := SubscriptionRenewalActivationReceipt{
		OldSubscriptionID: 123, NewSubscriptionID: 124, UserID: 10, GroupID: 20,
		PreviousCode: "store_current", CurrentCode: "store_next", PendingCodes: []string{"store_queued"},
		CurrentValidityDays: 30, TotalValidityDays: 61, StartsAt: now, ExpiresAt: now.AddDate(0, 0, 61),
		IdempotencyKey: "store-atomic:123",
	}
	raw, err := json.Marshal(receipt)
	require.NoError(t, err)
	cache := &storeAtomicReplayCache{fail: true, mock: mock, t: t}
	svc.billingCacheService = &BillingCacheService{cache: cache}
	for attempt := 0; attempt < 2; attempt++ {
		mock.ExpectBegin()
		mock.ExpectExec(`SELECT pg_advisory_xact_lock`).WithArgs(int64(10)).WillReturnResult(sqlmock.NewResult(0, 1))
		mock.ExpectQuery(`SELECT user_id,group_id,status,starts_at,expires_at`).WithArgs(int64(123)).WillReturnRows(sqlmock.NewRows([]string{"user_id", "group_id", "status", "starts_at", "expires_at", "notes", "deleted_at"}).AddRow(10, 20, "active", source.ExpectedStartsAt, source.ExpectedExpiresAt, source.ExpectedNotes, now))
		mock.ExpectQuery(`SELECT receipt, request_hash FROM subscription_renewal_activations`).WithArgs(int64(123)).WillReturnRows(sqlmock.NewRows([]string{"receipt", "request_hash"}).AddRow(raw, storeReplacementHash(123, in)))
		mock.ExpectCommit()
		result, err := svc.ReplaceStoreSubscription(context.Background(), 123, in)
		if attempt == 0 {
			require.ErrorContains(t, err, "Redis unavailable")
			cache.fail = false
		} else {
			require.NoError(t, err)
			require.True(t, result.Replayed)
			require.EqualValues(t, 124, result.Subscription.ID)
		}
		require.NoError(t, mock.ExpectationsWereMet())
	}
	require.Equal(t, 2, cache.invalidations)
	require.Equal(t, 1, cache.publications)
}

func TestStoreLegacyAtomicRequestIdentityIgnoresAdminAndNormalizesTimezone(t *testing.T) {
	_, _, source, _ := renewalFixture(t)
	in := storeAtomicUnitInput(source)
	copy := *in
	copy.AssignedBy = 42
	copy.ExpectedStartsAt = copy.ExpectedStartsAt.In(time.FixedZone("CST", 8*60*60))
	copy.ExpectedExpiresAt = copy.ExpectedExpiresAt.In(time.FixedZone("CST", 8*60*60))
	require.Equal(t, storeReplacementHash(123, in), storeReplacementHash(123, &copy))
	copy.ValidityDays--
	require.NotEqual(t, storeReplacementHash(123, in), storeReplacementHash(123, &copy))
}
