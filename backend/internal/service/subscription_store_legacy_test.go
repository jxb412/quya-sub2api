package service

import (
	"context"
	"database/sql"
	"testing"
	"time"

	"github.com/DATA-DOG/go-sqlmock"
	"github.com/stretchr/testify/require"
)

func TestStoreLegacyReplacementMarkersDistinguishCurrentAndQueue(t *testing.T) {
	marker, err := parseLegacyStoreReplacement("通过兑换码 store_b 兑换\n套餐当前购买码: store_b\n套餐待启用购买码: store_c\n套餐循环检查自动重开，原订阅ID 123")
	require.NoError(t, err)
	require.EqualValues(t, 123, marker.parentID)
	require.Equal(t, "store_b", marker.current)
	require.Equal(t, []string{"store_c"}, marker.pending)
	legacy, err := parseLegacyStoreReplacement("通过兑换码 store_b 兑换\n套餐循环检查自动重开，原订阅ID 123")
	require.NoError(t, err)
	require.Equal(t, "store_b", legacy.current)
	_, err = parseLegacyStoreReplacement("通过兑换码 store_b 兑换\n套餐当前购买码: store_b\n套餐待启用购买码: store_b\n套餐循环检查自动重开，原订阅ID 123")
	require.Error(t, err)
	ordinary, err := parseLegacyStoreReplacement("管理员赠送30天")
	require.NoError(t, err)
	require.Nil(t, ordinary)
	_, err = parseLegacyStoreReplacement("通过兑换码 store_b 兑换\n套餐循环检查自动重开，原订阅ID 123\n通过兑换码 store_c 兑换")
	require.Error(t, err, "new requests still require an unambiguous current purchase")
}

func TestStoreLegacyHistoryParentDoesNotParseAppendedPurchases(t *testing.T) {
	parentID, err := legacyStoreHistoryParent("通过兑换码 store_old 兑换\n套餐循环检查自动重开，原订阅ID 99\n通过兑换码 store_later 兑换")
	require.NoError(t, err)
	require.EqualValues(t, 99, parentID)
	for _, notes := range []string{
		"套餐循环检查自动重开，原订阅ID 99\n套餐循环检查自动重开，原订阅ID 123",
		"套餐循环检查自动重开，原订阅ID 99\n套餐循环检查自动重开，原订阅ID broken",
		"套餐循环检查自动重开，原订阅ID 0",
		"套餐循环检查自动重开，原订阅ID 999999999999999999999999",
	} {
		_, err = legacyStoreHistoryParent(notes)
		require.Error(t, err, "ambiguous prior grants cannot be skipped")
	}
	parentID, err = legacyStoreHistoryParent("管理员赠送30天")
	require.NoError(t, err)
	require.Zero(t, parentID)
}

func TestStoreLegacyHistoricalAppendRequiresPurchaseProof(t *testing.T) {
	h, now := nativeHistoryFixture()
	nativeAddReplacement(&h, now)
	delete(h.Receipts, 2) // The old PHP cron did not create native reset receipts.
	n := h.Nodes[2]
	n.Notes = "通过兑换码 store_b 兑换\n套餐循环检查自动重开，原订阅ID 1\n通过兑换码 store_d 兑换"
	n.ExpiresAt = n.ExpiresAt.AddDate(0, 0, 30)
	n.DeletedAt = sql.NullTime{Time: now.Add(2 * time.Hour), Valid: true}
	h.Nodes[2] = n
	h.Purchases["store_d"] = nativeSubscriptionPurchase{ID: 4, Code: "store_d", UserID: 10, GroupID: 20,
		Type: "subscription", Status: "used", Days: 30, UsedAt: sql.NullTime{Time: now.Add(time.Hour), Valid: true}, Notes: "store order order_d"}
	_, err := parseLegacyStoreReplacement(n.Notes)
	require.Error(t, err, "this historical shape must remain invalid as a new assignment")
	sources, err := h.resolve(2, map[int64]bool{})
	require.NoError(t, err)
	require.Equal(t, "store_b", sources.CurrentCode)
	require.Equal(t, []string{"store_c", "store_d"}, sources.PendingCodes)
	n.Notes = "通过兑换码 store_b 兑换\n套餐循环检查自动重开，原订阅ID 1\n通过兑换码 store_c 兑换\n通过兑换码 store_d 兑换"
	h.Nodes[2] = n
	_, err = h.resolve(2, map[int64]bool{})
	require.ErrorContains(t, err, "无法唯一确定", "two parent purchases must not be guessed by grant-line order")
}

func TestStoreLegacyRefundMatchesPHPHash(t *testing.T) {
	require.Equal(t, nativeRefundCode(42, "store_purchase_1"), storeRefundCode(42, "store_purchase_1"))
	require.NotEqual(t, storeRefundCode(41, "store_purchase_1"), storeRefundCode(42, "store_purchase_1"))
	require.False(t, IsStoreManagedRedemption(&RedeemCode{Code: "gift", Type: RedeemTypeBalance, Notes: "管理员赠送"}))
}

func TestStoreLegacyRefundOrdinaryAndResetPeriods(t *testing.T) {
	for _, mode := range []string{"ordinary_revoked", "still_active", "wrong_order_hash", "reset_activated", "reset_surrendered", "reset_queue_revoked", "reused_old_row"} {
		t.Run(mode, func(t *testing.T) {
			svc, mock, _, now := renewalFixture(t)
			purchase := "store_b"
			refund := &RedeemCode{Code: storeRefundCode(10, purchase), Type: RedeemTypeBalance, Value: 5, Notes: "subscription refund / P123"}
			purchaseRows := sqlmock.NewRows([]string{"code", "group_id", "used_at"})
			if mode != "wrong_order_hash" {
				purchaseRows.AddRow(purchase, int64(20), now)
			}
			mock.ExpectQuery(`SELECT code,group_id,used_at FROM redeem_codes`).WithArgs(int64(10), "store order P123").WillReturnRows(purchaseRows)
			if mode != "wrong_order_hash" {
				resetRows := sqlmock.NewRows([]string{"next_code", "previous_code", "notes", "starts_at"})
				if mode == "reset_activated" {
					resetRows.AddRow(purchase, "store_a", "通过兑换码 store_a 兑换", now)
				}
				if mode == "reset_surrendered" {
					resetRows.AddRow("store_c", purchase, "通过兑换码 store_b 兑换\n套餐当前购买码: store_b", now)
				}
				if mode == "reset_queue_revoked" {
					resetRows.AddRow("store_c", "store_a", "通过兑换码 store_a 兑换", now)
				}
				if mode == "reused_old_row" {
					resetRows.AddRow("store_c", "", "通过兑换码 store_a 兑换\n通过兑换码 store_b 兑换\n通过兑换码 store_c 兑换", now)
				}
				mock.ExpectQuery(`SELECT ra.next_code`).WithArgs(int64(10), int64(20)).WillReturnRows(resetRows)
				if mode != "reset_activated" && mode != "reset_surrendered" && mode != "reused_old_row" {
					var deleted any = now
					if mode == "still_active" {
						deleted = nil
					}
					mock.ExpectQuery(`SELECT id,COALESCE\(notes,''\),deleted_at FROM user_subscriptions`).WithArgs(int64(10), int64(20)).WillReturnRows(sqlmock.NewRows([]string{"id", "notes", "deleted_at"}).AddRow(123, "通过兑换码 store_b 兑换", deleted))
				}
			}
			err := validateLegacyStoreRefund(context.Background(), svc.entClient, 10, refund)
			if mode == "ordinary_revoked" || mode == "reset_queue_revoked" {
				require.NoError(t, err)
			} else {
				require.ErrorContains(t, err, "管理员")
			}
			require.NoError(t, mock.ExpectationsWereMet())
		})
	}
}

func TestStoreLegacyRefundManualGiftRetainsCompatibility(t *testing.T) {
	svc, mock, _, _ := renewalFixture(t)
	refund := &RedeemCode{Code: storeRefundCode(10, "manual_123"), Type: RedeemTypeBalance, Value: 5, Notes: "subscription refund / manual-sub-123"}
	mock.ExpectQuery(`SELECT COALESCE\(notes,''\),deleted_at`).WithArgs(int64(123), int64(10)).WillReturnRows(sqlmock.NewRows([]string{"notes", "deleted_at"}).AddRow("管理员手动分配", time.Now()))
	require.NoError(t, validateLegacyStoreRefund(context.Background(), svc.entClient, 10, refund))
	require.NoError(t, mock.ExpectationsWereMet())
}

func TestStoreLegacyCronRecognizesRevokedHistoricalSuccessor(t *testing.T) {
	svc, mock, _, now := renewalFixture(t)
	notes := "通过兑换码 store_b 兑换\n套餐当前购买码: store_b\n套餐待启用购买码: store_c\n套餐循环检查自动重开，原订阅ID 123"
	mock.ExpectBegin()
	mock.ExpectExec(`SELECT pg_advisory_xact_lock`).WithArgs(int64(10)).WillReturnResult(sqlmock.NewResult(0, 1))
	mock.ExpectQuery(`SELECT user_id,group_id,deleted_at`).WithArgs(int64(123)).WillReturnRows(sqlmock.NewRows([]string{"user_id", "group_id", "deleted_at"}).AddRow(10, 20, now))
	mock.ExpectQuery(`SELECT id,COALESCE\(notes,''\) FROM user_subscriptions`).WithArgs(int64(10), int64(20), int64(123)).WillReturnRows(sqlmock.NewRows([]string{"id", "notes"}).
		AddRow(100, "通过兑换码 store_old 兑换\n套餐循环检查自动重开，原订阅ID 99\n通过兑换码 store_later 兑换").
		AddRow(124, notes).
		AddRow(125, "通过兑换码 store_c 兑换\n套餐当前购买码: store_c\n套餐待启用购买码: \n套餐循环检查自动重开，原订阅ID 124"))
	mock.ExpectCommit()
	sub, err := svc.AssignSubscription(context.Background(), &AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 60, Notes: notes})
	require.NoError(t, err)
	require.EqualValues(t, 124, sub.ID)
	require.NoError(t, mock.ExpectationsWereMet())
}

func TestStoreLegacyCronStillRejectsAmbiguousTargetSuccessors(t *testing.T) {
	for _, mode := range []string{"duplicate", "wrong_current", "malformed_current"} {
		t.Run(mode, func(t *testing.T) {
			svc, mock, _, now := renewalFixture(t)
			notes := "通过兑换码 store_b 兑换\n套餐当前购买码: store_b\n套餐待启用购买码: store_c\n套餐循环检查自动重开，原订阅ID 123"
			prior := notes
			if mode == "wrong_current" {
				prior = "通过兑换码 store_other 兑换\n套餐循环检查自动重开，原订阅ID 123"
			}
			if mode == "malformed_current" {
				prior = "通过兑换码 store_b 兑换\n通过兑换码 store_c 兑换\n套餐当前购买码: broken\n套餐循环检查自动重开，原订阅ID 123"
			}
			history := sqlmock.NewRows([]string{"id", "notes"}).AddRow(124, prior)
			if mode == "duplicate" {
				history.AddRow(125, notes)
			}
			mock.ExpectBegin()
			mock.ExpectExec(`SELECT pg_advisory_xact_lock`).WithArgs(int64(10)).WillReturnResult(sqlmock.NewResult(0, 1))
			mock.ExpectQuery(`SELECT user_id,group_id,deleted_at`).WithArgs(int64(123)).WillReturnRows(sqlmock.NewRows([]string{"user_id", "group_id", "deleted_at"}).AddRow(10, 20, now))
			mock.ExpectQuery(`SELECT id,COALESCE\(notes,''\) FROM user_subscriptions`).WithArgs(int64(10), int64(20), int64(123)).WillReturnRows(history)
			mock.ExpectRollback()
			_, err := svc.AssignSubscription(context.Background(), &AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 60, Notes: notes})
			require.Error(t, err)
			require.NoError(t, mock.ExpectationsWereMet())
		})
	}
}

func TestStoreLegacyRefundRejectedBeforeNewCodeCreation(t *testing.T) {
	svc, mock, _, _ := renewalFixture(t)
	r := &RedeemService{entClient: svc.entClient, redeemRepo: &redeemRejectRepo{}}
	refund := &RedeemCode{Code: storeRefundCode(10, "store_b"), Type: RedeemTypeBalance, Value: 5, Notes: "subscription refund / P123"}
	mock.ExpectBegin()
	mock.ExpectExec(`SELECT pg_advisory_xact_lock`).WithArgs(int64(10)).WillReturnResult(sqlmock.NewResult(0, 1))
	mock.ExpectQuery(`SELECT code,group_id,used_at FROM redeem_codes`).WithArgs(int64(10), "store order P123").WillReturnRows(sqlmock.NewRows([]string{"code", "group_id", "used_at"}).AddRow("store_b", 20, time.Now()))
	mock.ExpectQuery(`SELECT ra.next_code`).WithArgs(int64(10), int64(20)).WillReturnRows(sqlmock.NewRows([]string{"next_code", "previous_code", "notes", "starts_at"}).AddRow("store_b", "store_a", "通过兑换码 store_a 兑换", time.Now()))
	mock.ExpectRollback()
	_, err := r.CreateAndRedeemStoreCode(context.Background(), 10, refund)
	require.ErrorContains(t, err, "应退金额")
	require.NoError(t, mock.ExpectationsWereMet())
}
