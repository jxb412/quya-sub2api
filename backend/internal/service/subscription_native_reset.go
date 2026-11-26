package service

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
	"time"

	infraerrors "github.com/Wei-Shaw/sub2api/internal/pkg/errors"
)

// 2026-11-24: no redeem codes or client-supplied ownership/order lists cross the
// user API. The token confirms the exact local purchase state; JWT is authority.
type SubscriptionResetPreview struct {
	Eligible                  bool       `json:"eligible"`
	ReasonCode                string     `json:"reason_code,omitempty"`
	Reason                    string     `json:"reason,omitempty"`
	SubscriptionID            int64      `json:"subscription_id"`
	GroupID                   int64      `json:"group_id"`
	GroupName                 string     `json:"group_name"`
	CurrentOrderNo            string     `json:"current_order_no,omitempty"`
	NextOrderNo               string     `json:"next_order_no,omitempty"`
	CurrentValidityDays       int        `json:"current_validity_days,omitempty"`
	NextValidityDays          int        `json:"next_validity_days,omitempty"`
	PendingCount              int        `json:"pending_count"`
	RemainingPendingCount     int        `json:"remaining_pending_count"`
	TotalValidityDays         int        `json:"total_validity_days,omitempty"`
	OldExpiresAt              time.Time  `json:"old_expires_at"`
	NewExpiresAt              *time.Time `json:"new_expires_at,omitempty"`
	DiscardedRemainingSeconds int64      `json:"discarded_remaining_seconds"`
	PreviewToken              string     `json:"preview_token,omitempty"`
}

type SubscriptionUserResetResult struct {
	OldSubscriptionID   int64     `json:"old_subscription_id"`
	NewSubscriptionID   int64     `json:"new_subscription_id"`
	StartsAt            time.Time `json:"starts_at"`
	ExpiresAt           time.Time `json:"expires_at"`
	CurrentValidityDays int       `json:"current_validity_days"`
	TotalValidityDays   int       `json:"total_validity_days"`
	PendingCount        int       `json:"pending_count"`
	Replayed            bool      `json:"replayed"`
}

func userResetResult(r *SubscriptionRenewalActivationReceipt) *SubscriptionUserResetResult {
	return &SubscriptionUserResetResult{OldSubscriptionID: r.OldSubscriptionID, NewSubscriptionID: r.NewSubscriptionID, StartsAt: r.StartsAt, ExpiresAt: r.ExpiresAt, CurrentValidityDays: r.CurrentValidityDays, TotalValidityDays: r.TotalValidityDays, PendingCount: len(r.PendingCodes), Replayed: r.Replayed}
}

var subscriptionPreviewTokenPattern = regexp.MustCompile(`^[a-f0-9]{64}$`)

func nativeSourceStateKey(s *nativeSubscriptionSources) string {
	purchases := make(map[string]nativeSubscriptionPurchase, len(s.Purchases))
	for code, p := range s.Purchases {
		if p.UsedAt.Valid {
			p.UsedAt.Time = p.UsedAt.Time.UTC()
		}
		purchases[code] = p
	}
	raw, _ := json.Marshal(struct {
		ID        int64
		Current   string
		Pending   []string
		Purchases map[string]nativeSubscriptionPurchase
	}{s.Sub.ID, s.CurrentCode, s.PendingCodes, purchases})
	hash := sha256.Sum256(raw)
	return "user-reset:" + hex.EncodeToString(hash[:])
}

func readOwnedResetSubscription(ctx context.Context, client renewalSQLClient, id, userID int64) (nativeSubscriptionNode, string, bool, error) {
	rows, err := client.QueryContext(ctx, `SELECT s.id,s.user_id,s.group_id,s.starts_at,s.expires_at,s.status,COALESCE(s.notes,''),s.deleted_at,g.name,(g.deleted_at IS NULL AND g.status='active' AND g.subscription_type='subscription') FROM user_subscriptions s JOIN groups g ON g.id=s.group_id WHERE s.id=$1 AND s.user_id=$2 FOR UPDATE OF s`, id, userID)
	if err != nil {
		return nativeSubscriptionNode{}, "", false, err
	}
	defer func() { _ = rows.Close() }()
	var n nativeSubscriptionNode
	var name string
	var groupOK bool
	if !rows.Next() {
		return n, "", false, ErrSubscriptionNotFound
	}
	if err := rows.Scan(&n.ID, &n.UserID, &n.GroupID, &n.StartsAt, &n.ExpiresAt, &n.Status, &n.Notes, &n.DeletedAt, &name, &groupOK); err != nil {
		return n, "", false, err
	}
	return n, name, groupOK, rows.Err()
}

func (s *SubscriptionService) nativeResetPreviewLocked(ctx context.Context, client renewalSQLClient, n nativeSubscriptionNode, name string, groupOK bool) (*SubscriptionResetPreview, *ActivateSubscriptionRenewalInput, error) {
	p := &SubscriptionResetPreview{SubscriptionID: n.ID, GroupID: n.GroupID, GroupName: name, OldExpiresAt: n.ExpiresAt}
	deny := func(code, message string) (*SubscriptionResetPreview, *ActivateSubscriptionRenewalInput, error) {
		p.Eligible = false
		p.PreviewToken = ""
		p.ReasonCode = code
		p.Reason = message
		return p, nil, nil
	}
	now := time.Now()
	if s.now != nil {
		now = s.now()
	}
	now, _ = renewalPeriod(now, 0)
	if s.autoResetEnabled {
		return deny("AUTO_RESET_ENABLED", "当前套餐规则不支持自助重置，请联系管理员")
	}
	if !groupOK || n.DeletedAt.Valid || n.Status != SubscriptionStatusActive || !n.ExpiresAt.After(now) {
		return deny("SUBSCRIPTION_INACTIVE", "仅当前有效的订阅可以重置")
	}
	if !n.ExpiresAt.After(now.Add(31 * 24 * time.Hour)) {
		return deny("REMAINING_DAYS_TOO_SHORT", "订阅剩余时间必须大于31天")
	}
	sources, err := resolveNativeSubscriptionSources(ctx, client, n.ID, n.UserID, n.GroupID)
	if err != nil {
		if infraerrors.Reason(err) == "SUBSCRIPTION_RENEWAL_CONFLICT" {
			return deny("PURCHASE_HISTORY_UNVERIFIED", infraerrors.Message(err))
		}
		return nil, nil, err
	}
	in := sources.activationInput()
	if in == nil {
		return deny("NO_QUEUED_PURCHASE", "没有可提前启用的同分组30/31天续购记录")
	}
	p.Eligible = true
	p.CurrentOrderNo = sources.Purchases[sources.CurrentCode].OrderNo
	p.NextOrderNo = sources.Purchases[in.NextCode].OrderNo
	p.CurrentValidityDays = sources.Purchases[sources.CurrentCode].Days
	p.NextValidityDays = sources.Purchases[in.NextCode].Days
	p.PendingCount = len(sources.PendingCodes)
	p.RemainingPendingCount = p.PendingCount - 1
	for _, code := range sources.PendingCodes {
		p.TotalValidityDays += sources.Purchases[code].Days
	}
	_, expiry := renewalPeriod(now, p.TotalValidityDays)
	p.NewExpiresAt = &expiry
	if expiry.After(MaxExpiresAt) {
		return deny("EXPIRY_TOO_LARGE", "重置后的期限超出支持范围")
	}
	remaining := n.ExpiresAt.Sub(now) - time.Duration(p.TotalValidityDays)*24*time.Hour
	if remaining > 0 {
		p.DiscardedRemainingSeconds = int64(remaining / time.Second)
	}
	p.PreviewToken = renewalActivationHash(n.ID, in)
	return p, in, nil
}

func (s *SubscriptionService) PreviewSubscriptionReset(ctx context.Context, userID, id int64) (*SubscriptionResetPreview, error) {
	if s.entClient == nil {
		return nil, fmt.Errorf("subscription reset requires database access")
	}
	tx, err := s.entClient.Tx(ctx)
	if err != nil {
		return nil, err
	}
	defer func() { _ = tx.Rollback() }()
	if err := lockStoreSubscriptionUser(ctx, tx.Client(), userID); err != nil {
		return nil, err
	}
	n, name, groupOK, err := readOwnedResetSubscription(ctx, tx.Client(), id, userID)
	if err != nil {
		return nil, err
	}
	p, _, err := s.nativeResetPreviewLocked(ctx, tx.Client(), n, name, groupOK)
	if err != nil {
		return nil, err
	}
	if err := tx.Commit(); err != nil {
		return nil, err
	}
	return p, nil
}

func (s *SubscriptionService) ResetUserSubscription(ctx context.Context, userID, id int64, token string, accepted bool) (*SubscriptionUserResetResult, error) {
	if !accepted || !subscriptionPreviewTokenPattern.MatchString(token) {
		return nil, infraerrors.BadRequest("INVALID_SUBSCRIPTION_RESET", "请先查看重置预览并确认重置规则")
	}
	if s.entClient == nil {
		return nil, fmt.Errorf("subscription reset requires database access")
	}
	tx, err := s.entClient.Tx(ctx)
	if err != nil {
		return nil, err
	}
	defer func() { _ = tx.Rollback() }()
	if err := lockStoreSubscriptionUser(ctx, tx.Client(), userID); err != nil {
		return nil, err
	}
	n, name, groupOK, err := readOwnedResetSubscription(ctx, tx.Client(), id, userID)
	if err != nil {
		return nil, err
	}
	prior, hash, err := readRenewalActivation(ctx, tx.Client(), id)
	if err != nil {
		return nil, err
	}
	var receipt *SubscriptionRenewalActivationReceipt
	if prior != nil {
		if prior.UserID != userID || !strings.HasPrefix(prior.IdempotencyKey, "user-reset:") || hash != token {
			return nil, renewalConflict("该订阅已变更，请刷新页面")
		}
		prior.Replayed = true
		receipt = prior
	} else {
		preview, in, err := s.nativeResetPreviewLocked(ctx, tx.Client(), n, name, groupOK)
		if err != nil {
			return nil, err
		}
		if !preview.Eligible {
			return nil, renewalConflict(preview.Reason)
		}
		if preview.PreviewToken != token {
			return nil, renewalConflict("购买记录或订阅期限已变化，请重新查看重置预览")
		}
		receipt, err = s.activateSubscriptionRenewalLocked(ctx, tx.Client(), id, in)
		if err != nil {
			return nil, err
		}
	}
	if err := tx.Commit(); err != nil {
		return nil, err
	}
	if err := s.invalidateSubscriptionCaches(userID, n.GroupID); err != nil {
		return nil, err
	}
	return userResetResult(receipt), nil
}
