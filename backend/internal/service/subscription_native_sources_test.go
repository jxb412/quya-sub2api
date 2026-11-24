package service

import (
	"database/sql"
	"testing"
	"time"

	"github.com/stretchr/testify/require"
)

func nativeHistoryFixture() (nativeSubscriptionHistory, time.Time) {
	now := time.Date(2026, 11, 24, 10, 0, 0, 0, time.UTC)
	start := now.AddDate(0, 0, -10)
	h := nativeSubscriptionHistory{Nodes: map[int64]nativeSubscriptionNode{}, Purchases: map[string]nativeSubscriptionPurchase{}, Refunds: map[string]bool{}, Receipts: map[int64]SubscriptionRenewalActivationReceipt{}}
	h.Nodes[1] = nativeSubscriptionNode{ID: 1, UserID: 10, GroupID: 20, StartsAt: start, ExpiresAt: start.AddDate(0, 0, 91), Status: "active", Notes: "通过兑换码 store_a 兑换\n通过兑换码 store_b 兑换\n通过兑换码 store_c 兑换"}
	for i, code := range []string{"store_a", "store_b", "store_c"} {
		used := start
		if i == 1 {
			used = now.AddDate(0, 0, -3)
		}
		if i == 2 {
			used = now.AddDate(0, 0, -1)
		}
		days := 30
		if i == 2 {
			days = 31
		}
		h.Purchases[code] = nativeSubscriptionPurchase{ID: int64(i + 1), Code: code, UserID: 10, GroupID: 20, Type: "subscription", Status: "used", Days: days, UsedAt: sql.NullTime{Time: used, Valid: true}, Notes: "store order order_" + code}
	}
	return h, now
}

func TestNativeSubscriptionSourcesQueuedPurchasesAndStateToken(t *testing.T) {
	h, _ := nativeHistoryFixture()
	s, err := h.resolve(1, map[int64]bool{})
	require.NoError(t, err)
	require.Equal(t, "store_a", s.CurrentCode)
	require.Equal(t, []string{"store_b", "store_c"}, s.PendingCodes)
	in := s.activationInput()
	require.Equal(t, "store_b", in.NextCode)
	require.Equal(t, []string{"store_c"}, in.PendingCodes)
	token := renewalActivationHash(1, in)
	p := h.Purchases["store_b"]
	p.Notes = "store order changed_order"
	h.Purchases["store_b"] = p
	s, err = h.resolve(1, map[int64]bool{})
	require.NoError(t, err)
	require.NotEqual(t, token, renewalActivationHash(1, s.activationInput()), "preview must bind the local order proof")
}

func TestNativeSubscriptionSourcesRejectUnprovenRights(t *testing.T) {
	cases := map[string]func(*nativeSubscriptionHistory){
		"gift": func(h *nativeSubscriptionHistory) {
			p := h.Purchases["store_b"]
			p.Notes = "administrator gift"
			h.Purchases["store_b"] = p
		},
		"foreign user": func(h *nativeSubscriptionHistory) {
			p := h.Purchases["store_b"]
			p.UserID = 11
			h.Purchases["store_b"] = p
		},
		"foreign group": func(h *nativeSubscriptionHistory) {
			p := h.Purchases["store_b"]
			p.GroupID = 21
			h.Purchases["store_b"] = p
		},
		"unused": func(h *nativeSubscriptionHistory) {
			p := h.Purchases["store_b"]
			p.Status = "unused"
			h.Purchases["store_b"] = p
		},
		"non monthly": func(h *nativeSubscriptionHistory) {
			p := h.Purchases["store_b"]
			p.Days = 29
			h.Purchases["store_b"] = p
		},
		"refund":         func(h *nativeSubscriptionHistory) { h.Refunds[nativeRefundCode(10, "store_b")] = true },
		"missing source": func(h *nativeSubscriptionHistory) { delete(h.Purchases, "store_b") },
		"duplicate order": func(h *nativeSubscriptionHistory) {
			p := h.Purchases["store_b"]
			p.Notes = h.Purchases["store_a"].Notes
			h.Purchases["store_b"] = p
		},
		"gifted extension": func(h *nativeSubscriptionHistory) {
			n := h.Nodes[1]
			n.ExpiresAt = n.ExpiresAt.AddDate(0, 0, 1)
			h.Nodes[1] = n
		},
		"ambiguous start": func(h *nativeSubscriptionHistory) {
			p := h.Purchases["store_b"]
			p.UsedAt.Time = h.Nodes[1].StartsAt.Add(time.Second)
			h.Purchases["store_b"] = p
		},
		"unrelated prior use": func(h *nativeSubscriptionHistory) {
			n := h.Nodes[1]
			n.ID = 8
			n.StartsAt = n.StartsAt.AddDate(0, 0, -20)
			n.DeletedAt = sql.NullTime{Time: n.StartsAt.AddDate(0, 0, 1), Valid: true}
			h.Nodes[8] = n
		},
	}
	for name, change := range cases {
		t.Run(name, func(t *testing.T) {
			h, _ := nativeHistoryFixture()
			change(&h)
			_, err := h.resolve(1, map[int64]bool{})
			require.Error(t, err)
		})
	}
}

func nativeAddReplacement(h *nativeSubscriptionHistory, now time.Time) {
	old := h.Nodes[1]
	old.DeletedAt = sql.NullTime{Time: now, Valid: true}
	h.Nodes[1] = old
	h.Nodes[2] = nativeSubscriptionNode{ID: 2, UserID: 10, GroupID: 20, StartsAt: now, ExpiresAt: now.AddDate(0, 0, 61), Status: "active", Notes: "通过兑换码 store_b 兑换\n套餐当前购买码: store_b\n套餐待启用购买码: store_c\n套餐循环检查自动重开，原订阅ID 1"}
	h.Receipts[2] = SubscriptionRenewalActivationReceipt{OldSubscriptionID: 1, NewSubscriptionID: 2, UserID: 10, GroupID: 20, PreviousCode: "store_a", CurrentCode: "store_b", PendingCodes: []string{"store_c"}, StartsAt: now, ExpiresAt: now.AddDate(0, 0, 61), CurrentValidityDays: 30, TotalValidityDays: 61}
}

func TestNativeSubscriptionSourcesReplacementPreservesQueueAndRejectsReuse(t *testing.T) {
	h, now := nativeHistoryFixture()
	nativeAddReplacement(&h, now)
	s, err := h.resolve(2, map[int64]bool{})
	require.NoError(t, err)
	require.Equal(t, "store_b", s.CurrentCode)
	require.Equal(t, []string{"store_c"}, s.PendingCodes)
	require.True(t, s.Consumed["store_a"])
	n := h.Nodes[2]
	n.Notes = "通过兑换码 store_a 兑换\n套餐当前购买码: store_a\n套餐待启用购买码: store_c\n套餐循环检查自动重开，原订阅ID 1"
	h.Nodes[2] = n
	_, err = h.resolve(2, map[int64]bool{})
	require.Error(t, err)
}

func TestNativeSubscriptionSourcesExpiredSameRowStartsNewEpoch(t *testing.T) {
	h, now := nativeHistoryFixture()
	n := h.Nodes[1]
	n.StartsAt = now
	n.ExpiresAt = now.AddDate(0, 0, 61)
	h.Nodes[1] = n
	a := h.Purchases["store_a"]
	a.Days = 7
	a.UsedAt.Time = now.AddDate(0, 0, -90)
	h.Purchases["store_a"] = a
	b := h.Purchases["store_b"]
	b.UsedAt.Time = now
	h.Purchases["store_b"] = b
	c := h.Purchases["store_c"]
	c.UsedAt.Time = now.Add(time.Hour)
	h.Purchases["store_c"] = c
	h.Refunds[nativeRefundCode(10, "store_a")] = true
	s, err := h.resolve(1, map[int64]bool{})
	require.NoError(t, err)
	require.Equal(t, "store_b", s.CurrentCode)
	require.Equal(t, []string{"store_c"}, s.PendingCodes)
	require.True(t, s.Consumed["store_a"])
}

func TestNativeSubscriptionSourcesExpiredResetRowIgnoresOldMarkers(t *testing.T) {
	for _, withReceipt := range []bool{false, true} {
		t.Run(map[bool]string{false: "legacy cron", true: "native reset"}[withReceipt], func(t *testing.T) {
			h, now := nativeHistoryFixture()
			nativeAddReplacement(&h, now)
			if !withReceipt {
				delete(h.Receipts, 2)
			}
			fresh := now.AddDate(0, 0, 70)
			n := h.Nodes[2]
			n.StartsAt = fresh
			n.ExpiresAt = fresh.AddDate(0, 0, 61)
			n.Notes += "\n通过兑换码 store_d 兑换\n通过兑换码 store_e 兑换"
			h.Nodes[2] = n
			for i, code := range []string{"store_d", "store_e"} {
				days := 30
				if i == 1 {
					days = 31
				}
				h.Purchases[code] = nativeSubscriptionPurchase{ID: int64(4 + i), Code: code, UserID: 10, GroupID: 20, Type: "subscription", Status: "used", Days: days, UsedAt: sql.NullTime{Time: fresh.Add(time.Duration(i) * time.Hour), Valid: true}, Notes: "store order order_" + code}
			}
			s, err := h.resolve(2, map[int64]bool{})
			require.NoError(t, err)
			require.Equal(t, "store_d", s.CurrentCode)
			require.Equal(t, []string{"store_e"}, s.PendingCodes)
			require.True(t, s.Consumed["store_a"])
			require.True(t, s.Consumed["store_b"])
			require.True(t, s.Consumed["store_c"])
		})
	}
}
