package service

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"time"

	dbent "github.com/Wei-Shaw/sub2api/ent"
	infraerrors "github.com/Wei-Shaw/sub2api/internal/pkg/errors"
)

// 2026-11-26: the store cron used separate revoke/assign requests. A rejected
// assignment could leave a paying customer without a subscription. Keep both
// mutations and the durable retry receipt in one transaction instead.
type ReplaceStoreSubscriptionInput struct {
	UserID            int64     `json:"user_id"`
	GroupID           int64     `json:"group_id"`
	ValidityDays      int       `json:"validity_days"`
	Notes             string    `json:"notes"`
	ExpectedStartsAt  time.Time `json:"expected_starts_at"`
	ExpectedExpiresAt time.Time `json:"expected_expires_at"`
	ExpectedNotes     *string   `json:"expected_notes"`
	AssignedBy        int64     `json:"-"`
}

type ReplaceStoreSubscriptionResult struct {
	OldSubscriptionID int64
	Subscription      *UserSubscription
	Replayed          bool
}

func storeReplacementHash(id int64, in *ReplaceStoreSubscriptionInput) string {
	copy := *in
	copy.ExpectedStartsAt = copy.ExpectedStartsAt.UTC()
	copy.ExpectedExpiresAt = copy.ExpectedExpiresAt.UTC()
	raw, _ := json.Marshal(struct {
		OldID int64
		Input ReplaceStoreSubscriptionInput
	}{id, copy})
	sum := sha256.Sum256(raw)
	return hex.EncodeToString(sum[:])
}

// ReplaceStoreSubscription is an administrator-only store operation. It does
// not inherit the user self-reset switch or its >31-day remaining-time rule.
// Purchases, queue order, refunds and manual grants are still checked by the
// same store guard used by the legacy assignment endpoint.
func (s *SubscriptionService) ReplaceStoreSubscription(ctx context.Context, id int64, in *ReplaceStoreSubscriptionInput) (*ReplaceStoreSubscriptionResult, error) {
	if in == nil || id <= 0 || in.UserID <= 0 || in.GroupID <= 0 || in.ValidityDays < 1 ||
		in.ValidityDays > MaxValidityDays || len(in.Notes) > 65536 || in.ExpectedNotes == nil ||
		len(*in.ExpectedNotes) > 65536 || in.ExpectedStartsAt.IsZero() || in.ExpectedExpiresAt.IsZero() {
		return nil, infraerrors.BadRequest("INVALID_STORE_REPLACEMENT", "invalid atomic store replacement request")
	}
	marker, err := parseLegacyStoreReplacement(in.Notes)
	if err != nil {
		return nil, err
	}
	if marker == nil || marker.parentID != id {
		return nil, renewalConflict("自动重开标记与原套餐不一致")
	}
	if s.entClient == nil {
		return nil, fmt.Errorf("atomic store replacement requires database access")
	}
	result := &ReplaceStoreSubscriptionResult{OldSubscriptionID: id}
	hash := storeReplacementHash(id, in)
	key := fmt.Sprintf("store-atomic:%d", id)
	err = s.withSubscriptionUpdateTx(ctx, func(txCtx context.Context) error {
		client := dbent.TxFromContext(txCtx).Client()
		if err := lockStoreSubscriptionUser(txCtx, client, in.UserID); err != nil {
			return err
		}
		rows, err := client.QueryContext(txCtx, `SELECT user_id,group_id,status,starts_at,expires_at,COALESCE(notes,''),deleted_at FROM user_subscriptions WHERE id=$1 FOR UPDATE`, id)
		if err != nil {
			return err
		}
		var old UserSubscription
		var deleted sql.NullTime
		if !rows.Next() {
			err = rows.Err()
			_ = rows.Close()
			if err != nil {
				return err
			}
			return ErrSubscriptionNotFound
		}
		err = rows.Scan(&old.UserID, &old.GroupID, &old.Status, &old.StartsAt, &old.ExpiresAt, &old.Notes, &deleted)
		_ = rows.Close()
		if err != nil {
			return err
		}
		if old.UserID != in.UserID || old.GroupID != in.GroupID {
			return renewalConflict("自动重开的原套餐归属不一致")
		}
		prior, priorHash, err := readRenewalActivation(txCtx, client, id)
		if err != nil {
			return err
		}
		if prior != nil {
			if priorHash != hash || prior.IdempotencyKey != key || prior.UserID != in.UserID || prior.GroupID != in.GroupID {
				return renewalConflict("原套餐已由其他操作重开；不会重复发放")
			}
			result.Subscription = &UserSubscription{ID: prior.NewSubscriptionID, UserID: prior.UserID, GroupID: prior.GroupID}
			result.Replayed = true
			return nil
		}
		if !old.StartsAt.Equal(in.ExpectedStartsAt) || !old.ExpiresAt.Equal(in.ExpectedExpiresAt) || old.Notes != *in.ExpectedNotes {
			return renewalConflict("原套餐或续购记录已变更，请重新核对循环任务")
		}
		if s.autoResetEnabled {
			return renewalConflict("自动额度重置仍开启，需先核对套餐循环规则")
		}
		now := time.Now()
		if s.now != nil {
			now = s.now()
		}
		now, _ = renewalPeriod(now, 0)
		if !deleted.Valid {
			if old.Status != SubscriptionStatusActive || !old.ExpiresAt.After(now) {
				return renewalConflict("原套餐已暂停或到期，请管理员核对循环任务")
			}
			// No cache invalidation is allowed before commit. Concurrent readers
			// continue to see the old period until the new one is fully validated.
			if _, err := client.ExecContext(txCtx, `UPDATE user_subscriptions SET deleted_at=$2,updated_at=$2 WHERE id=$1 AND deleted_at IS NULL`, id, now); err != nil {
				return err
			}
		}
		sub, reused, err := s.assignLegacyStoreReplacement(txCtx, &AssignSubscriptionInput{
			UserID: in.UserID, GroupID: in.GroupID, ValidityDays: in.ValidityDays, AssignedBy: in.AssignedBy, Notes: in.Notes,
		}, marker)
		if err != nil {
			return err // withSubscriptionUpdateTx rolls back the old-row revoke too.
		}
		if reused {
			if !deleted.Valid {
				return renewalConflict("原套餐已存在历史后继，请管理员核对；不会再次撤销")
			}
			// An interrupted old PHP flow may have completed before this API
			// existed. A historical successor is success, never a new grant.
			result.Subscription, result.Replayed = sub, true
			return nil
		}
		sources, err := resolveNativeSubscriptionSources(txCtx, client, id, in.UserID, in.GroupID)
		if err != nil {
			return err
		}
		receipt := &SubscriptionRenewalActivationReceipt{
			OldSubscriptionID: id, NewSubscriptionID: sub.ID, UserID: in.UserID, GroupID: in.GroupID,
			PreviousCode: sources.CurrentCode, CurrentCode: marker.current,
			PendingCodes: append([]string{}, marker.pending...), CurrentValidityDays: sources.Purchases[marker.current].Days,
			TotalValidityDays: in.ValidityDays, StartsAt: sub.StartsAt, ExpiresAt: sub.ExpiresAt, IdempotencyKey: key,
		}
		raw, err := json.Marshal(receipt)
		if err != nil {
			return err
		}
		_, err = client.ExecContext(txCtx, `INSERT INTO subscription_renewal_activations (old_subscription_id,new_subscription_id,user_id,group_id,next_code,idempotency_key,request_hash,receipt,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)`,
			id, sub.ID, in.UserID, in.GroupID, marker.current, key, hash, string(raw), now)
		if err != nil {
			return err
		}
		result.Subscription = sub
		return nil
	})
	if err != nil {
		return nil, err
	}
	// A first attempt can commit but lose its cache invalidation or HTTP reply.
	// Replays therefore invalidate again; they never reset quota a second time.
	if dbent.TxFromContext(ctx) == nil {
		if err := s.invalidateSubscriptionCaches(in.UserID, in.GroupID); err != nil {
			return nil, err
		}
	}
	return result, nil
}
