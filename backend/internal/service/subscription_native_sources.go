package service

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/lib/pq"
)

// 2026-11-24 Quya: prove queued rights from this application's redeemed store
// records. This intentionally does not claim to verify the PHP payment ledger.
type nativeSubscriptionNode struct {
	ID, UserID, GroupID int64
	StartsAt, ExpiresAt time.Time
	Status, Notes       string
	DeletedAt           sql.NullTime
}

type nativeSubscriptionPurchase struct {
	ID, UserID, GroupID                int64
	Code, Type, Status, Notes, OrderNo string
	Days                               int
	UsedAt                             sql.NullTime
}

type nativeSubscriptionSources struct {
	CurrentCode  string
	PendingCodes []string
	Sub          nativeSubscriptionNode
	Purchases    map[string]nativeSubscriptionPurchase
	Lineage      map[int64]bool
	Consumed     map[string]bool
}

type nativeSubscriptionHistory struct {
	Nodes     map[int64]nativeSubscriptionNode
	Purchases map[string]nativeSubscriptionPurchase
	Refunds   map[string]bool
	Receipts  map[int64]SubscriptionRenewalActivationReceipt // by new subscription ID
}

var nativeStoreCode = regexp.MustCompile(`(?:^|[^A-Za-z0-9_-])(store_[A-Za-z0-9_-]+)`)
var nativeGrantCode = regexp.MustCompile(`通过兑换码\s+(store_[A-Za-z0-9_-]+)\s+兑换`)
var nativeCurrentMarker = regexp.MustCompile(`(?m)^套餐当前购买码\s*[:：]\s*(store_[A-Za-z0-9_-]+)\s*$`)
var nativePendingMarker = regexp.MustCompile(`(?m)^套餐待启用购买码\s*[:：]([^\r\n]*)$`)
var nativeParentMarker = regexp.MustCompile(`套餐循环检查自动重开，原订阅ID\s*(\d+)`)
var nativeStoreOrder = regexp.MustCompile(`^store order ([A-Za-z0-9_-]{1,100})$`)

func nativeSourceConflict(message string) error {
	return renewalConflict(message + "，请联系管理员核对购买记录")
}

func nativeCodes(notes string) []string {
	var out []string
	seen := map[string]bool{}
	for _, m := range nativeStoreCode.FindAllStringSubmatch(notes, -1) {
		if !seen[m[1]] {
			seen[m[1]] = true
			out = append(out, m[1])
		}
	}
	return out
}

func nativeRefundCode(userID int64, code string) string {
	hash := sha256.Sum256([]byte(strconv.FormatInt(userID, 10) + ":" + code))
	return "sbr_" + hex.EncodeToString(hash[:])[:28]
}

// Callers hold lockStoreSubscriptionUser before reading. Deleted nodes remain
// resolvable so the legacy PHP replacement guard can validate its old parent.
func resolveNativeSubscriptionSources(ctx context.Context, client renewalSQLClient, subID, userID, groupID int64) (*nativeSubscriptionSources, error) {
	h := nativeSubscriptionHistory{Nodes: map[int64]nativeSubscriptionNode{}, Purchases: map[string]nativeSubscriptionPurchase{}, Refunds: map[string]bool{}, Receipts: map[int64]SubscriptionRenewalActivationReceipt{}}
	rows, err := client.QueryContext(ctx, `SELECT id,user_id,group_id,starts_at,expires_at,status,COALESCE(notes,''),deleted_at FROM user_subscriptions WHERE user_id=$1 AND group_id=$2 ORDER BY id LIMIT 501`, userID, groupID)
	if err != nil {
		return nil, err
	}
	allCodes := map[string]bool{}
	for rows.Next() {
		var n nativeSubscriptionNode
		if err = rows.Scan(&n.ID, &n.UserID, &n.GroupID, &n.StartsAt, &n.ExpiresAt, &n.Status, &n.Notes, &n.DeletedAt); err != nil {
			rows.Close()
			return nil, err
		}
		h.Nodes[n.ID] = n
		for _, code := range nativeCodes(n.Notes) {
			allCodes[code] = true
		}
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return nil, err
	}
	if len(h.Nodes) > 500 || len(allCodes) > 2000 {
		return nil, nativeSourceConflict("历史购买记录过多")
	}
	if _, ok := h.Nodes[subID]; !ok {
		return nil, ErrSubscriptionNotFound
	}
	if len(allCodes) == 0 {
		return nil, nativeSourceConflict("未找到已兑换的商城购买记录")
	}
	keys := make([]string, 0, len(allCodes)*2)
	for code := range allCodes {
		keys = append(keys, code, nativeRefundCode(userID, code))
	}
	sort.Strings(keys)
	rows, err = client.QueryContext(ctx, `SELECT id,code,type,status,used_by,group_id,validity_days,used_at,COALESCE(notes,'') FROM redeem_codes WHERE code=ANY($1) ORDER BY id FOR SHARE`, pq.Array(keys))
	if err != nil {
		return nil, err
	}
	for rows.Next() {
		var p nativeSubscriptionPurchase
		var user, group sql.NullInt64
		if err = rows.Scan(&p.ID, &p.Code, &p.Type, &p.Status, &user, &group, &p.Days, &p.UsedAt, &p.Notes); err != nil {
			rows.Close()
			return nil, err
		}
		p.UserID = user.Int64
		p.GroupID = group.Int64
		if strings.HasPrefix(p.Code, "sbr_") {
			h.Refunds[p.Code] = true
			continue
		}
		h.Purchases[p.Code] = p
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return nil, err
	}
	rows, err = client.QueryContext(ctx, `SELECT receipt FROM subscription_renewal_activations WHERE user_id=$1 AND group_id=$2 ORDER BY old_subscription_id LIMIT 501`, userID, groupID)
	if err != nil {
		return nil, err
	}
	for rows.Next() {
		var raw []byte
		var receipt SubscriptionRenewalActivationReceipt
		if err = rows.Scan(&raw); err != nil {
			rows.Close()
			return nil, err
		}
		if err = json.Unmarshal(raw, &receipt); err != nil {
			rows.Close()
			return nil, err
		}
		if receipt.UserID != userID || receipt.GroupID != groupID {
			rows.Close()
			return nil, nativeSourceConflict("重置回执归属异常")
		}
		h.Receipts[receipt.NewSubscriptionID] = receipt
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return nil, err
	}
	if len(h.Receipts) > 500 {
		return nil, nativeSourceConflict("重置历史过多")
	}
	return h.resolve(subID, map[int64]bool{})
}

func (h *nativeSubscriptionHistory) purchase(code string, n nativeSubscriptionNode) (nativeSubscriptionPurchase, error) {
	p, ok := h.Purchases[code]
	order := nativeStoreOrder.FindStringSubmatch(strings.TrimSpace(p.Notes))
	if !ok || !renewalCodePattern.MatchString(code) || p.Type != "subscription" || p.Status != "used" || p.UserID != n.UserID || p.GroupID != n.GroupID || !p.UsedAt.Valid || (p.Days < 1 || p.Days > MaxValidityDays) || len(order) != 2 {
		return p, nativeSourceConflict("购买来源、使用者、分组或30/31天期限无法核对")
	}
	p.OrderNo = order[1]
	return p, nil
}

func nativeNear(a, b time.Time) bool { return a.Sub(b) <= 2*time.Second && b.Sub(a) <= 2*time.Second }
func nativeContains(list []string, code string) bool {
	for _, s := range list {
		if s == code {
			return true
		}
	}
	return false
}

func (h *nativeSubscriptionHistory) resolve(id int64, visiting map[int64]bool) (*nativeSubscriptionSources, error) {
	n, ok := h.Nodes[id]
	if !ok || visiting[id] || len(visiting) >= 30 {
		return nil, nativeSourceConflict("套餐历史链缺失或循环")
	}
	// 2026-11-25: production may lack the live user/group unique index. Reject
	// an ambiguous active target even when the competing row has no store codes.
	// Deleted ancestors remain resolvable while validating replacement history.
	if !n.DeletedAt.Valid {
		for otherID, other := range h.Nodes {
			if otherID != id && other.UserID == n.UserID && other.GroupID == n.GroupID && !other.DeletedAt.Valid {
				return nil, nativeSourceConflict("该分组存在多个未撤销套餐，暂不能自助重置")
			}
		}
	}
	visiting[id] = true
	defer delete(visiting, id)
	if n.StartsAt.IsZero() || !n.ExpiresAt.After(n.StartsAt) || len(n.Notes) > 65536 {
		return nil, nativeSourceConflict("套餐起止时间或备注异常")
	}
	codes := nativeCodes(n.Notes)
	if len(codes) == 0 || len(codes) > 100 {
		return nil, nativeSourceConflict("套餐购买记录缺失或数量超出限制")
	}
	out := &nativeSubscriptionSources{Sub: n, Purchases: map[string]nativeSubscriptionPurchase{}, Lineage: map[int64]bool{id: true}, Consumed: map[string]bool{}}
	for _, code := range codes {
		p, err := h.purchase(code, n)
		if err != nil {
			return nil, err
		}
		out.Purchases[code] = p
	}
	currentMatches := nativeCurrentMarker.FindAllStringSubmatch(n.Notes, -1)
	pendingMatches := nativePendingMarker.FindAllStringSubmatch(n.Notes, -1)
	if len(currentMatches) > 1 || len(pendingMatches) > 1 || (strings.Contains(n.Notes, "套餐当前购买码") && len(currentMatches) != 1) || (strings.Contains(n.Notes, "套餐待启用购买码") && len(pendingMatches) != 1) {
		return nil, nativeSourceConflict("购买码标记不完整或重复")
	}
	var markedPending []string
	if len(pendingMatches) > 0 {
		for _, code := range strings.Fields(pendingMatches[0][1]) {
			if !renewalCodePattern.MatchString(code) || nativeContains(markedPending, code) {
				return nil, nativeSourceConflict("待启用购买码标记异常")
			}
			markedPending = append(markedPending, code)
		}
		if len(currentMatches) != 1 {
			return nil, nativeSourceConflict("缺少当前购买码标记")
		}
	}
	parents := nativeParentMarker.FindAllStringSubmatch(n.Notes, -1)
	if len(parents) > 1 || (strings.Contains(n.Notes, "原订阅ID") && len(parents) != 1) {
		return nil, nativeSourceConflict("原套餐标记不明确")
	}
	var parent *nativeSubscriptionSources
	if len(parents) == 1 {
		parentID, _ := strconv.ParseInt(parents[0][1], 10, 64)
		old, exists := h.Nodes[parentID]
		if !exists || !old.DeletedAt.Valid || old.DeletedAt.Time.After(n.StartsAt.Add(2*time.Second)) || old.StartsAt.After(n.StartsAt) {
			return nil, nativeSourceConflict("原套餐撤销时间或归属无法核对")
		}
		var err error
		parent, err = h.resolve(parentID, visiting)
		if err != nil {
			return nil, err
		}
		for k := range parent.Lineage {
			out.Lineage[k] = true
		}
		for k := range parent.Consumed {
			out.Consumed[k] = true
		}
	}
	// Official expiry renewal reuses the same row and appends notes. An older
	// reset marker/receipt is evidence of the previous epoch, not the new current.
	freshEpoch := false
	freshCode := ""
	for _, grant := range nativeGrantCode.FindAllStringSubmatch(n.Notes, -1) {
		code := grant[1]
		if nativeNear(out.Purchases[code].UsedAt.Time, n.StartsAt) && code != freshCode {
			if freshCode != "" {
				return nil, nativeSourceConflict("当前套餐起点对应多笔购买，无法唯一确定")
			}
			freshCode = code
		}
	}
	receipt, hasReceipt := h.Receipts[id]
	if freshCode != "" && ((hasReceipt && !receipt.StartsAt.Equal(n.StartsAt)) || (len(currentMatches) == 1 && currentMatches[0][1] != freshCode) || (parent != nil && !nativeNear(parent.Sub.DeletedAt.Time, n.StartsAt))) {
		priorStart := time.Time{}
		if hasReceipt {
			priorStart = receipt.StartsAt
		} else if parent != nil {
			priorStart = parent.Sub.DeletedAt.Time
		} else if len(currentMatches) == 1 {
			priorStart = out.Purchases[currentMatches[0][1]].UsedAt.Time
		}
		oldRights := map[string]bool{}
		if parent != nil {
			for _, code := range parent.PendingCodes {
				oldRights[code] = true
				out.Purchases[code] = parent.Purchases[code]
			}
		}
		for _, code := range codes {
			if out.Purchases[code].UsedAt.Time.Before(n.StartsAt.Add(-2*time.Second)) && !out.Consumed[code] {
				oldRights[code] = true
			}
		}
		oldDays := 0
		for code := range oldRights {
			oldDays += out.Purchases[code].Days
		}
		if priorStart.IsZero() || oldDays < 1 || oldDays > MaxValidityDays || n.StartsAt.Before(priorStart.Add(time.Duration(oldDays)*24*time.Hour).Add(-2*time.Second)) {
			return nil, nativeSourceConflict("旧套餐尚未自然到期，无法认定新的购买周期")
		}
		for code := range oldRights {
			out.Consumed[code] = true
		}
		freshEpoch = true
		out.CurrentCode = freshCode
		markedPending = nil
	}
	if freshEpoch {
		// Current was proven by the new starts_at and a uniquely matching grant.
	} else if len(currentMatches) == 1 {
		out.CurrentCode = currentMatches[0][1]
		if _, ok := out.Purchases[out.CurrentCode]; !ok {
			return nil, nativeSourceConflict("当前购买码不在来源中")
		}
	} else if parent != nil {
		for _, code := range codes {
			if nativeContains(parent.PendingCodes, code) {
				if out.CurrentCode != "" {
					return nil, nativeSourceConflict("旧版重开的当前购买记录无法唯一确定")
				}
				out.CurrentCode = code
			}
		}
		if out.CurrentCode == "" {
			return nil, nativeSourceConflict("旧版重开缺少未启用购买记录")
		}
	} else {
		grants := nativeGrantCode.FindAllStringSubmatch(n.Notes, -1)
		if len(grants) == 0 {
			return nil, nativeSourceConflict("缺少原始兑换记录")
		}
		for _, grant := range grants {
			code := grant[1]
			p := out.Purchases[code]
			if nativeNear(p.UsedAt.Time, n.StartsAt) && code != out.CurrentCode {
				if out.CurrentCode != "" {
					return nil, nativeSourceConflict("当前套餐起点对应多笔购买，无法唯一确定")
				}
				out.CurrentCode = code
			}
		}
		if out.CurrentCode == "" {
			return nil, nativeSourceConflict("当前套餐起点与购买兑换时间不一致")
		}
	}
	if parent == nil {
		for _, code := range codes {
			if code != out.CurrentCode && out.Purchases[code].UsedAt.Time.Before(n.StartsAt.Add(-2*time.Second)) {
				out.Consumed[code] = true
			}
		}
	}
	current := out.Purchases[out.CurrentCode]
	if out.Consumed[out.CurrentCode] {
		return nil, nativeSourceConflict("当前购买记录已在历史套餐使用")
	}
	if parent != nil && !freshEpoch {
		if !nativeContains(parent.PendingCodes, out.CurrentCode) {
			return nil, nativeSourceConflict("重开购买记录不属于原套餐待启用记录")
		}
		for _, code := range parent.PendingCodes {
			if code != out.CurrentCode {
				out.PendingCodes = append(out.PendingCodes, code)
				out.Purchases[code] = parent.Purchases[code]
			}
		}
	} else if !freshEpoch && len(currentMatches) > 0 && !nativeNear(current.UsedAt.Time, n.StartsAt) {
		return nil, nativeSourceConflict("缺少当前购买记录的历史来源")
	}
	for _, code := range codes {
		if code == out.CurrentCode || out.Consumed[code] || nativeContains(out.PendingCodes, code) {
			continue
		}
		p := out.Purchases[code]
		if p.UsedAt.Time.Before(current.UsedAt.Time) || (p.UsedAt.Time.Equal(current.UsedAt.Time) && p.ID < current.ID) {
			return nil, nativeSourceConflict("续购顺序早于当前购买记录")
		}
		if p.UsedAt.Time.After(n.ExpiresAt.Add(2 * time.Second)) {
			return nil, nativeSourceConflict("续购兑换时间超出套餐期限")
		}
		if !nativeContains(markedPending, code) && p.UsedAt.Time.Before(n.StartsAt.Add(-2*time.Second)) {
			return nil, nativeSourceConflict("续购是否尚未启用无法核对")
		}
		out.PendingCodes = append(out.PendingCodes, code)
	}
	for _, code := range markedPending {
		if code == out.CurrentCode || out.Consumed[code] || !nativeContains(out.PendingCodes, code) {
			return nil, nativeSourceConflict("待启用标记包含已使用或未知购买记录")
		}
	}
	if receipt, ok := h.Receipts[id]; ok && !freshEpoch {
		if parent == nil || receipt.OldSubscriptionID != parent.Sub.ID || receipt.CurrentCode != out.CurrentCode || !receipt.StartsAt.Equal(n.StartsAt) {
			return nil, nativeSourceConflict("重置回执与套餐来源不一致")
		}
		for _, code := range receipt.PendingCodes {
			if !nativeContains(out.PendingCodes, code) {
				return nil, nativeSourceConflict("重置回执中的待启用记录缺失")
			}
		}
	}
	sort.Slice(out.PendingCodes, func(i, j int) bool {
		a, b := out.Purchases[out.PendingCodes[i]], out.Purchases[out.PendingCodes[j]]
		if a.UsedAt.Time.Equal(b.UsedAt.Time) {
			return a.ID < b.ID
		}
		return a.UsedAt.Time.Before(b.UsedAt.Time)
	})
	for _, code := range append([]string{out.CurrentCode}, out.PendingCodes...) {
		if out.Purchases[code].Days != 30 && out.Purchases[code].Days != 31 {
			return nil, nativeSourceConflict("仅支持30或31天的当前购买与续购")
		}
		if h.Refunds[nativeRefundCode(n.UserID, code)] {
			return nil, nativeSourceConflict("购买记录存在余额退款凭据")
		}
	}
	days := current.Days
	orders := map[string]bool{current.OrderNo: true}
	for _, code := range out.PendingCodes {
		p := out.Purchases[code]
		if orders[p.OrderNo] {
			return nil, nativeSourceConflict("同一订单重复发放")
		}
		orders[p.OrderNo] = true
		days += p.Days
	}
	if !nativeNear(n.ExpiresAt, n.StartsAt.Add(time.Duration(days)*24*time.Hour)) {
		return nil, nativeSourceConflict("套餐期限与完整购买记录不一致")
	}
	// A store code copied into an unrelated revoked subscription is ambiguous:
	// never recycle it merely because the current row still has enough days.
	for otherID, other := range h.Nodes {
		if out.Lineage[otherID] || (n.DeletedAt.Valid && other.StartsAt.After(n.StartsAt)) {
			continue
		}
		for _, code := range nativeCodes(other.Notes) {
			if code == out.CurrentCode || nativeContains(out.PendingCodes, code) {
				return nil, nativeSourceConflict("购买记录同时出现在其他历史套餐")
			}
		}
	}
	out.Consumed[out.CurrentCode] = true
	return out, nil
}

func (s *nativeSubscriptionSources) activationInput() *ActivateSubscriptionRenewalInput {
	if len(s.PendingCodes) == 0 {
		return nil
	}
	return &ActivateSubscriptionRenewalInput{UserID: s.Sub.UserID, GroupID: s.Sub.GroupID, CurrentCode: s.CurrentCode, NextCode: s.PendingCodes[0], PendingCodes: append([]string{}, s.PendingCodes[1:]...), ExpectedStartsAt: s.Sub.StartsAt, ExpectedExpiresAt: s.Sub.ExpiresAt, ExpectedNotes: s.Sub.Notes, IdempotencyKey: nativeSourceStateKey(s)}
}
