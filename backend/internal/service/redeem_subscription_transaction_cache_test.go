//go:build unit

package service

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/DATA-DOG/go-sqlmock"
	"github.com/dgraph-io/ristretto"
	"github.com/stretchr/testify/require"
)

type redeemSubscriptionCacheRepo struct {
	*lockingRenewalRepo
	beforeCommit func()
}

func (r *redeemSubscriptionCacheRepo) GetByID(ctx context.Context, id int64) (*UserSubscription, error) {
	r.beforeCommit()
	return r.lockingRenewalRepo.GetByID(ctx, id)
}

func TestRedeemSubscriptionCacheInvalidatesOnlyAfterSuccessfulCommit(t *testing.T) {
	for _, store := range []bool{false, true} {
		for _, commitFails := range []bool{false, true} {
			name := "public"
			if store {
				name = "store_outer_transaction"
			}
			if commitFails {
				name += "_commit_failure"
			}
			t.Run(name, func(t *testing.T) {
				subs, mock, _, now := renewalFixture(t)
				cache, err := ristretto.NewCache(&ristretto.Config{NumCounters: 1000, MaxCost: 100, BufferItems: 64})
				require.NoError(t, err)
				t.Cleanup(cache.Close)
				subs.subCacheL1 = cache
				subs.groupRepo = &subscriptionGroupRepoStub{group: &Group{ID: 20, SubscriptionType: SubscriptionTypeSubscription}}
				current := UserSubscription{ID: 123, UserID: 10, GroupID: 20, StartsAt: now, ExpiresAt: now.Add(30 * 24 * time.Hour), Status: SubscriptionStatusActive}
				key := subCacheKey(10, 20)
				require.True(t, cache.Set(key, &current, 1))
				cache.Wait()
				checkedBeforeCommit := false
				subs.userSubRepo = &redeemSubscriptionCacheRepo{
					lockingRenewalRepo: &lockingRenewalRepo{stale: current, current: current},
					beforeCommit: func() {
						checkedBeforeCommit = true
						cache.Wait()
						_, cached := cache.Get(key)
						require.True(t, cached, "uncommitted assignment must not evict the committed subscription")
					},
				}
				groupID := int64(20)
				code := RedeemCode{ID: 1, Code: "ordinary_subscription", Type: RedeemTypeSubscription, Value: 1, Status: StatusUnused, GroupID: &groupID, ValidityDays: 30}
				if store {
					code.Code = "store_cache"
				}
				repo := &redeemRejectRepo{code: code}
				r := NewRedeemService(repo, &mockUserRepo{getByIDUser: &User{ID: 10}}, subs, nil, nil, subs.entClient, nil, nil)
				invalidator := &authCacheInvalidatorStub{}
				r.authCacheInvalidator = invalidator
				mock.ExpectBegin()
				if store {
					mock.ExpectExec(`SELECT pg_advisory_xact_lock`).WithArgs(int64(10)).WillReturnResult(sqlmock.NewResult(0, 1))
					mock.ExpectExec(`SELECT pg_advisory_xact_lock`).WithArgs(int64(10)).WillReturnResult(sqlmock.NewResult(0, 1))
				}
				commit := mock.ExpectCommit()
				if commitFails {
					commit.WillReturnError(errors.New("commit failed"))
				}
				if store {
					_, err = r.CreateAndRedeemStoreCode(context.Background(), 10, &code)
				} else {
					_, err = r.Redeem(context.Background(), 10, code.Code)
				}
				require.True(t, checkedBeforeCommit)
				cache.Wait()
				_, cached := cache.Get(key)
				if commitFails {
					require.ErrorContains(t, err, "commit failed")
					require.True(t, cached)
					require.Empty(t, invalidator.userIDs)
				} else {
					require.NoError(t, err)
					require.False(t, cached)
					require.Equal(t, []int64{10}, invalidator.userIDs)
				}
				require.NoError(t, mock.ExpectationsWereMet())
			})
		}
	}
}
