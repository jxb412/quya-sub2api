package service

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"regexp"
	"strconv"
	"strings"
	"time"

	dbent "github.com/Wei-Shaw/sub2api/ent"
)

// 2026-11-24: the unchanged store's cron and refund endpoints must claim the
// same purchased rights as native reset. Take this lock before any subscription
// or redemption row lock, and keep it through commit of the resulting mutation.
type storeSubscriptionSQLClient interface {
	QueryContext(context.Context, string, ...any) (*sql.Rows, error)
	ExecContext(context.Context, string, ...any) (sql.Result, error)
}

func lockStoreSubscriptionUser(ctx context.Context, client storeSubscriptionSQLClient, userID int64) error {
	if userID <= 0 {
		return ErrSubscriptionNotFound
	}
	_, err := client.ExecContext(ctx, `SELECT pg_advisory_xact_lock(hashtextextended('quya:subscription-user:' || $1::text, 0))`, userID)
	return err
}

func lockStoreSubscriptionUserForID(ctx context.Context, client storeSubscriptionSQLClient, id int64) error {
	rows, err := client.QueryContext(ctx, `SELECT user_id FROM user_subscriptions WHERE id=$1`, id)
	if err != nil {
		return err
	}
	var userID int64
	if !rows.Next() {
		err = rows.Err()
		rows.Close()
		if err != nil {
			return err
		}
		return ErrSubscriptionNotFound
	}
	err = rows.Scan(&userID)
	rows.Close()
	if err != nil {
		return err
	}
	return lockStoreSubscriptionUser(ctx, client, userID)
}

var storeParentMarker = regexp.MustCompile(`(?m)^套餐循环检查自动重开，原订阅ID[ \t]*(\d+)[ \t\r]*$`)
var storeCurrentMarker = regexp.MustCompile(`(?m)^套餐当前购买码[ \t]*[:：][ \t]*(store_[A-Za-z0-9_-]+)[ \t\r]*$`)
var storeRedeemedMarker = regexp.MustCompile(`(?m)^通过兑换码[ \t]+(store_[A-Za-z0-9_-]+)[ \t]+兑换[ \t\r]*$`)
var storePendingMarker = regexp.MustCompile(`(?m)^套餐待启用购买码[ \t]*[:：]([^\r\n]*)`)
var storeCodeToken = regexp.MustCompile(`store_[A-Za-z0-9_-]+`)

type legacyStoreReplacement struct {
	parentID int64
	current  string
	pending  []string
}

func parseLegacyStoreReplacement(notes string) (*legacyStoreReplacement, error) {
	if !strings.Contains(notes, "套餐循环检查自动重开，原订阅ID") {
		return nil, nil
	}
	bad := func() (*legacyStoreReplacement, error) {
		return nil, renewalConflict("套餐自动重开来源不完整，请管理员核对购买记录")
	}
	parents := storeParentMarker.FindAllStringSubmatch(notes, -1)
	if len(parents) != 1 {
		return bad()
	}
	id, err := strconv.ParseInt(parents[0][1], 10, 64)
	if err != nil || id <= 0 {
		return bad()
	}
	current := storeCurrentMarker.FindAllStringSubmatch(notes, -1)
	if len(current) == 0 && !strings.Contains(notes, "套餐当前购买码") {
		current = storeRedeemedMarker.FindAllStringSubmatch(notes, -1)
	}
	if len(current) != 1 {
		return bad()
	}
	out := &legacyStoreReplacement{parentID: id, current: current[0][1]}
	pending := storePendingMarker.FindAllStringSubmatch(notes, -1)
	if len(pending) > 1 {
		return bad()
	}
	if len(pending) == 1 {
		seen := map[string]bool{out.current: true}
		for _, code := range strings.Fields(pending[0][1]) {
			if !renewalCodePattern.MatchString(code) || seen[code] {
				return bad()
			}
			seen[code] = true
			out.pending = append(out.pending, code)
		}
	}
	return out, nil
}

func storeRefundCode(userID int64, purchaseCode string) string {
	sum := sha256.Sum256([]byte(strconv.FormatInt(userID, 10) + ":" + purchaseCode))
	return "sbr_" + hex.EncodeToString(sum[:])[:28]
}

func isLegacyStoreRefund(code *RedeemCode) bool {
	return code != nil && code.Type == RedeemTypeBalance && strings.HasPrefix(code.Code, "sbr_") && strings.HasPrefix(code.Notes, "subscription refund / ")
}

func IsStoreManagedRedemption(code *RedeemCode) bool {
	return isLegacyStoreRefund(code) || (code != nil && code.Type == RedeemTypeSubscription && renewalCodePattern.MatchString(code.Code))
}

// CreateAndRedeemStoreCode holds the user claim across creation and redemption.
// Failed refunds leave no newly-created unused code that could freeze a renewal.
func (s *RedeemService) CreateAndRedeemStoreCode(ctx context.Context, userID int64, code *RedeemCode) (*RedeemCode, error) {
	if !IsStoreManagedRedemption(code) || s.entClient == nil {
		return nil, fmt.Errorf("invalid store redemption")
	}
	tx, err := s.entClient.Tx(ctx)
	if err != nil {
		return nil, err
	}
	defer func() { _ = tx.Rollback() }()
	txCtx := dbent.NewTxContext(ctx, tx)
	if err := lockStoreSubscriptionUser(txCtx, tx.Client(), userID); err != nil {
		return nil, err
	}
	existing, err := s.redeemRepo.GetByCode(txCtx, code.Code)
	if err == nil {
		if existing.IsUsed() {
			if existing.UsedBy == nil || *existing.UsedBy != userID {
				return nil, ErrRedeemCodeUsed
			}
			if err := tx.Commit(); err != nil {
				return nil, err
			}
			return existing, nil
		}
		code = existing
	} else {
		if !errors.Is(err, ErrRedeemCodeNotFound) {
			return nil, err
		}
		if err := validateLegacyStoreRefund(txCtx, tx.Client(), userID, code); err != nil {
			return nil, err
		}
		if err := s.CreateCode(txCtx, code); err != nil {
			return nil, err
		}
	}
	result, err := s.RedeemForAdminFulfillment(txCtx, userID, code.Code)
	if err != nil {
		return nil, err
	}
	if err := tx.Commit(); err != nil {
		return nil, err
	}
	s.invalidateRedeemCaches(ctx, userID, result)
	if result.Type == RedeemTypeBalance && result.Value > 0 {
		s.tryAccrueAffiliateRebateForRedeem(ctx, userID, result.Value)
	}
	return result, nil
}

// Called after the common user lock and before Use/balance update, including
// ordinary redemption of a code left unused by an interrupted admin request.
func validateLegacyStoreRefund(ctx context.Context, client storeSubscriptionSQLClient, userID int64, refund *RedeemCode) error {
	if !isLegacyStoreRefund(refund) {
		return nil
	}
	deny := func(message string) error {
		return renewalConflict(message + "，请管理员核对后处理余额转换")
	}
	orderNo := strings.TrimPrefix(refund.Notes, "subscription refund / ")
	// Preserve the store's administrator-priced, non-store subscriptions.
	if strings.HasPrefix(orderNo, "manual-sub-") {
		id, err := strconv.ParseInt(strings.TrimPrefix(orderNo, "manual-sub-"), 10, 64)
		if err != nil || id <= 0 || refund.Code != storeRefundCode(userID, "manual_"+strconv.FormatInt(id, 10)) {
			return deny("手动套餐余额转换来源不一致")
		}
		rows, err := client.QueryContext(ctx, `SELECT COALESCE(notes,''),deleted_at FROM user_subscriptions WHERE id=$1 AND user_id=$2`, id, userID)
		if err != nil {
			return err
		}
		var notes string
		var deleted sql.NullTime
		if !rows.Next() {
			rows.Close()
			return deny("手动套餐撤销记录缺失")
		}
		err = rows.Scan(&notes, &deleted)
		rows.Close()
		if err != nil {
			return err
		}
		if !deleted.Valid || storeCodeToken.MatchString(notes) {
			return deny("手动套餐仍有效或包含商城购买权益")
		}
		return nil
	}
	rows, err := client.QueryContext(ctx, `SELECT code,group_id,used_at FROM redeem_codes WHERE type='subscription' AND status='used' AND used_by=$1 AND used_at IS NOT NULL AND notes=$2`, userID, "store order "+orderNo)
	if err != nil {
		return err
	}
	var purchase string
	var groupID int64
	var purchasedAt time.Time
	for rows.Next() {
		var code string
		var group sql.NullInt64
		var usedAt time.Time
		if err := rows.Scan(&code, &group, &usedAt); err != nil {
			rows.Close()
			return err
		}
		if renewalCodePattern.MatchString(code) && refund.Code == storeRefundCode(userID, code) && group.Valid {
			if purchase != "" {
				rows.Close()
				return deny("购买来源重复")
			}
			purchase, groupID, purchasedAt = code, group.Int64, usedAt
		}
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return err
	}
	if purchase == "" {
		return deny("余额转换与原购买码、订单或用户不一致")
	}
	// A purchase activated by reset can also appear in an older refund's pending
	// quote. That old API carries neither subscription ID nor the price snapshot,
	// so those two refund amounts cannot safely be distinguished here.
	rows, err = client.QueryContext(ctx, `SELECT ra.next_code,COALESCE(ra.receipt->>'previous_code',''),COALESCE(old.notes,''),old.starts_at FROM subscription_renewal_activations ra JOIN user_subscriptions old ON old.id=ra.old_subscription_id WHERE ra.user_id=$1 AND ra.group_id=$2`, userID, groupID)
	if err != nil {
		return err
	}
	for rows.Next() {
		var next, previous, notes string
		var startsAt time.Time
		if err := rows.Scan(&next, &previous, &notes, &startsAt); err != nil {
			rows.Close()
			return err
		}
		if next == purchase {
			rows.Close()
			return deny("该购买已启用新一期，旧版退款请求无法核对原订阅及应退金额")
		}
		if previous != "" {
			if previous == purchase {
				rows.Close()
				return deny("该购买的剩余权益已在重置时放弃，不能重复转换余额")
			}
			continue
		}
		// Older receipts have no previous_code. An expired row can be reused
		// with its prior grant notes retained; never infer current from first grant.
		if !nativeContains(nativeCodes(notes), purchase) {
			continue
		}
		current := storeCurrentMarker.FindAllStringSubmatch(notes, -1)
		if len(current) == 1 {
			if current[0][1] == purchase {
				rows.Close()
				return deny("该购买的剩余权益已在重置时放弃，不能重复转换余额")
			}
		} else if strings.Contains(notes, "套餐当前购买码") || !purchasedAt.After(startsAt.Add(2*time.Second)) {
			rows.Close()
			return deny("已重置套餐的历史当前购买记录无法核对")
		}
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return err
	}
	rows, err = client.QueryContext(ctx, `SELECT id,COALESCE(notes,''),deleted_at FROM user_subscriptions WHERE user_id=$1 AND group_id=$2 ORDER BY id`, userID, groupID)
	if err != nil {
		return err
	}
	found := false
	for rows.Next() {
		var id int64
		var notes string
		var deleted sql.NullTime
		if err := rows.Scan(&id, &notes, &deleted); err != nil {
			rows.Close()
			return err
		}
		for _, code := range storeCodeToken.FindAllString(notes, -1) {
			if code != purchase {
				continue
			}
			found = true
			if !deleted.Valid {
				rows.Close()
				return deny("该购买仍属于有效套餐或待启用队列，不能由旧退款请求转换余额")
			}
		}
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return err
	}
	if !found {
		return deny("未找到可核对的套餐购买来源链")
	}
	return nil
}

// Only the legacy store marker takes this path. Ordinary administrator grants
// retain their existing behavior. Historical successors count even if revoked.
func (s *SubscriptionService) assignLegacyStoreReplacement(ctx context.Context, in *AssignSubscriptionInput, marker *legacyStoreReplacement) (*UserSubscription, bool, error) {
	var result *UserSubscription
	reused := false
	err := s.withSubscriptionUpdateTx(ctx, func(txCtx context.Context) error {
		if s.entClient == nil {
			return fmt.Errorf("store replacement requires database access")
		}
		client := dbent.TxFromContext(txCtx).Client()
		if err := lockStoreSubscriptionUser(txCtx, client, in.UserID); err != nil {
			return err
		}
		rows, err := client.QueryContext(txCtx, `SELECT user_id,group_id,deleted_at FROM user_subscriptions WHERE id=$1`, marker.parentID)
		if err != nil {
			return err
		}
		var owner, group int64
		var deleted sql.NullTime
		if !rows.Next() {
			rows.Close()
			return renewalConflict("原套餐历史缺失，请管理员核对自动重开记录")
		}
		err = rows.Scan(&owner, &group, &deleted)
		rows.Close()
		if err != nil {
			return err
		}
		if owner != in.UserID || group != in.GroupID || !deleted.Valid {
			return renewalConflict("原套餐未撤销或归属不一致，请管理员核对自动重开记录")
		}
		// Detect an earlier completed attempt before considering current activity.
		rows, err = client.QueryContext(txCtx, `SELECT id,COALESCE(notes,'') FROM user_subscriptions WHERE user_id=$1 AND group_id=$2 AND id<>$3 ORDER BY id`, in.UserID, in.GroupID, marker.parentID)
		if err != nil {
			return err
		}
		var successorID int64
		historyNotes := map[int64]string{}
		for rows.Next() {
			var id int64
			var notes string
			if err := rows.Scan(&id, &notes); err != nil {
				rows.Close()
				return err
			}
			historyNotes[id] = notes
			prior, parseErr := parseLegacyStoreReplacement(notes)
			if parseErr != nil {
				rows.Close()
				return parseErr
			}
			if prior == nil || prior.parentID != marker.parentID {
				continue
			}
			if prior.current != marker.current || successorID != 0 {
				rows.Close()
				return renewalConflict("原套餐已有其他重开记录，请管理员核对；不会再次发放")
			}
			successorID = id
		}
		err = rows.Err()
		rows.Close()
		if err != nil {
			return err
		}
		if successorID != 0 {
			// Return a receipt-like success for unchanged cron recovery without
			// restoring or extending the already-issued (possibly revoked) period.
			result = &UserSubscription{ID: successorID, UserID: in.UserID, GroupID: in.GroupID}
			reused = true
			return nil
		}
		receipt, _, err := readRenewalActivation(txCtx, client, marker.parentID)
		if err != nil {
			return err
		}
		if receipt != nil {
			return renewalConflict("原套餐已完成重置，请管理员核对；不会重复发放续购权益")
		}
		sources, err := resolveNativeSubscriptionSources(txCtx, client, marker.parentID, in.UserID, in.GroupID)
		if err != nil {
			return err
		}
		// Parent resolution permits later descendants while walking history.
		// With no known successor, an unrelated historical copy must not let
		// this legacy request issue the same queued purchase a second time.
		for id, notes := range historyNotes {
			if sources.Lineage[id] {
				continue
			}
			for _, code := range nativeCodes(notes) {
				if nativeContains(sources.PendingCodes, code) {
					return renewalConflict("待启用购买已出现在其他历史套餐，请管理员核对；不会重复发放")
				}
			}
		}
		if len(sources.PendingCodes) == 0 || marker.current != sources.PendingCodes[0] {
			return renewalConflict("自动重开未选择最早待启用购买，请管理员核对续购顺序")
		}
		if len(marker.pending) != len(sources.PendingCodes)-1 {
			return renewalConflict("自动重开没有完整保留待启用购买，请管理员核对")
		}
		for i, code := range marker.pending {
			if code != sources.PendingCodes[i+1] {
				return renewalConflict("自动重开的排队顺序与购买记录不一致，请管理员核对")
			}
		}
		available := make(map[string]bool, len(sources.PendingCodes))
		for _, code := range sources.PendingCodes {
			available[code] = true
		}
		codes := append([]string{marker.current}, marker.pending...)
		if len(codes) != len(available) {
			return renewalConflict("自动重开没有完整保留待启用购买，请管理员核对")
		}
		totalDays := 0
		for _, code := range codes {
			if !available[code] {
				return renewalConflict("自动重开包含已消费或不属于原套餐的购买，请管理员核对")
			}
			delete(available, code)
			rows, err := client.QueryContext(txCtx, `SELECT validity_days FROM redeem_codes WHERE code=$1 AND type='subscription' AND status='used' AND used_by=$2 AND group_id=$3 AND used_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM redeem_codes refund WHERE refund.code=$4)`, code, in.UserID, in.GroupID, storeRefundCode(in.UserID, code))
			if err != nil {
				return err
			}
			var days int
			if !rows.Next() {
				rows.Close()
				return renewalConflict("购买记录已退款或无法核对，请管理员处理自动重开")
			}
			err = rows.Scan(&days)
			rows.Close()
			if err != nil {
				return err
			}
			if days <= 0 || days > MaxValidityDays {
				return renewalConflict("购买期限异常，请管理员核对")
			}
			totalDays += days
		}
		if totalDays != in.ValidityDays {
			return renewalConflict("自动重开期限与购买记录不一致，请管理员核对")
		}
		rows, err = client.QueryContext(txCtx, `SELECT id FROM groups WHERE id=$1 AND deleted_at IS NULL AND subscription_type='subscription' FOR SHARE`, in.GroupID)
		if err != nil {
			return err
		}
		groupOK := rows.Next()
		err = rows.Err()
		rows.Close()
		if err != nil {
			return err
		}
		if !groupOK {
			return ErrGroupNotSubscriptionType
		}
		// Creation fails against the existing partial unique index if any other
		// live row exists. Never reuse/extend that row for a stale cron assignment.
		result, err = s.createSubscription(txCtx, in)
		return err
	})
	if err != nil {
		return nil, false, err
	}
	if !reused && dbent.TxFromContext(ctx) == nil {
		if err := s.invalidateSubscriptionCaches(in.UserID, in.GroupID); err != nil {
			return nil, false, err
		}
	}
	return result, reused, nil
}
