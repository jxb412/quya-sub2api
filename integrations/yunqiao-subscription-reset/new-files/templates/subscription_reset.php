<?php
declare(strict_types=1);
$apiBase = ($mountPrefix ?: '') . '/api/user/subscription-reset';
$subscriptionId = ctype_digit((string) ($_GET['subscription_id'] ?? '')) ? (int) $_GET['subscription_id'] : 0;
$pageError = !empty($error) ? (string) $error : (empty($session['user_id']) ? '请从订阅页面重新打开，完成登录后再操作。' : '');
?>
<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="robots" content="noindex,nofollow"><meta name="referrer" content="no-referrer">
  <title>重置订阅</title>
  <style>
    :root{color-scheme:light dark;--bg:#fff;--fg:#182235;--muted:#637083;--line:#e2e8f0;--soft:#f5f8fc;--blue:#2563eb}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.65 system-ui,sans-serif}.shell{max-width:680px;margin:auto;padding:24px}h1{font-size:23px;margin:0 0 8px}h2{font-size:17px;margin:0 0 14px}p{margin:8px 0}.muted{color:var(--muted)}.panel{margin-top:18px;padding:20px;border:1px solid var(--line);border-radius:13px}dl{display:grid;grid-template-columns:1fr minmax(0,1.5fr);gap:12px;margin:0}dt{color:var(--muted)}dd{margin:0;text-align:right;overflow-wrap:anywhere}.rule{background:var(--soft);border-radius:10px;padding:14px;margin-top:16px}.notice{padding:13px 16px;border-radius:10px;background:var(--soft);white-space:pre-line}.notice.bad{color:#b91c1c;background:#fef2f2}.notice.good{color:#047857;background:#ecfdf5}.actions{display:flex;gap:10px;margin-top:18px;flex-wrap:wrap}button{border:0;border-radius:9px;padding:11px 17px;cursor:pointer;font:inherit;font-weight:600;background:var(--blue);color:#fff}button.secondary{background:var(--soft);color:var(--fg);border:1px solid var(--line)}button:disabled{opacity:.5;cursor:not-allowed}label{display:flex;align-items:flex-start;gap:9px;margin-top:18px}input[type=checkbox]{margin-top:6px;flex-shrink:0}ul{padding-left:20px}.quota{margin:6px 0;color:var(--muted)}[hidden]{display:none!important}@media(max-width:430px){.shell{padding:16px}.panel{padding:16px}dl{grid-template-columns:1fr}dd{text-align:left;margin-top:-8px}}html.dark{--bg:#151e2d;--fg:#e6edf7;--muted:#a4b1c5;--line:#334155;--soft:#202d40}.dark .notice.bad{background:#3b2026;color:#fecaca}.dark .notice.good{background:#123c34;color:#a7f3d0}
  </style>
</head>
<body>
<main class="shell">
  <h1>重置订阅</h1>
  <p class="muted">提前启用一份已购买的同套餐续购，重新获得该套餐额度。</p>
  <p id="notice" class="notice" role="status" aria-live="polite">正在核对订阅及购买订单……</p>
  <section id="preview" class="panel" hidden>
    <h2 id="group"></h2>
    <dl>
      <dt>当前套餐订单</dt><dd id="current-order"></dd>
      <dt>即将启用的订单</dt><dd id="next-order"></dd>
      <dt>新一期有效期</dt><dd id="term"></dd>
      <dt>其余待启用续购</dt><dd id="queue-count"></dd>
      <dt>当前总到期时间</dt><dd id="old-expiry"></dd>
      <dt>重置后预计到期时间</dt><dd id="new-expiry"></dd>
    </dl>
    <ul id="queue" hidden></ul>
    <div id="quotas"></div>
    <div class="rule">
      <strong>确认前请注意</strong>
      <p>当前这一期的剩余额度和剩余时间将放弃，不能转入新一期。下一份续购从重置成功时起，按订单购买的30天或31天完整计算。</p>
      <p>如果还购买了更多份相同套餐，其他订单会继续保留在待启用队列中，不会一次用掉。页面显示的总到期时间包含这些排队续购。</p>
      <p class="muted">重置前已开始的请求仍按旧套餐结算。重置过程中请勿另外操作套餐余额转换。</p>
    </div>
    <label><input id="accepted" type="checkbox"><span>我已核对下一份续购订单，同意放弃当前这一期剩余额度和时间并启用新一期。</span></label>
    <div class="actions"><button id="confirm" type="button" disabled>确认重置并启用下一期</button><button id="refresh" type="button" class="secondary">重新核对</button></div>
  </section>
  <div id="retry-actions" class="actions" hidden><button id="retry" type="button">核对并重试</button></div>
</main>
<script>
(() => {
  'use strict';
  const api = <?= json_encode($apiBase, JSON_HEX_TAG | JSON_HEX_AMP | JSON_HEX_APOS | JSON_HEX_QUOT) ?>;
  const subscriptionId = <?= $subscriptionId ?>;
  const initialError = <?= json_encode($pageError, JSON_HEX_TAG | JSON_HEX_AMP | JSON_HEX_APOS | JSON_HEX_QUOT) ?>;
  const params = new URLSearchParams(location.search);
  if (params.get('theme') === 'dark') document.documentElement.classList.add('dark');
  let parentOrigin = '';
  try { const value = new URL(params.get('src_host') || ''); if (['https:', 'http:'].includes(value.protocol)) parentOrigin = value.origin; } catch (_) {}
  // The login token has already been exchanged by PHP; keep it out of later browser history/referrers.
  const clean = new URL(location.href); clean.searchParams.delete('token'); clean.searchParams.delete('user_id'); clean.searchParams.delete('src_url');
  history.replaceState(null, '', clean.pathname + clean.search + clean.hash);
  const el = id => document.getElementById(id);
  let quote = null, busy = false, pending = false, completed = false;
  const date = value => new Date(value).toLocaleString('zh-CN', {hour12:false});
  function notice(text, kind = '') { el('notice').textContent = text; el('notice').className = 'notice' + (kind ? ' ' + kind : ''); }
  function controls() {
    el('confirm').disabled = busy || pending || completed || !quote || !el('accepted').checked;
    el('accepted').disabled = busy || pending || completed;
    el('refresh').disabled = busy || pending || completed;
    el('retry').disabled = busy;
    el('retry-actions').hidden = !pending;
  }
  async function request(path, options = {}) {
    const response = await fetch(api + path, {credentials:'same-origin', cache:'no-store', ...options});
    const result = await response.json().catch(() => null);
    if (!response.ok || !result || result.code !== 0) {
      const failure = new Error(result?.message || '请求暂时失败，请稍后重试');
      failure.retryable = response.status >= 500 || result?.data?.retryable === true;
      throw failure;
    }
    return result.data;
  }
  async function preview() {
    if (busy || pending || completed) return;
    busy = true; quote = null; el('preview').hidden = true; el('accepted').checked = false; controls();
    notice('正在核对订阅及购买订单……');
    try {
      quote = await request('/preview?subscription_id=' + encodeURIComponent(subscriptionId));
      el('group').textContent = quote.group_name;
      el('current-order').textContent = quote.current_order_no;
      el('next-order').textContent = quote.next_order_no;
      el('term').textContent = quote.current_validity_days + '天，从重置成功时开始';
      el('queue-count').textContent = quote.queued_count + '份';
      el('old-expiry').textContent = date(quote.old_expires_at);
      el('new-expiry').textContent = date(quote.new_expires_at_estimate) + '（以重置成功时间为准）';
      el('queue').replaceChildren();
      for (const row of quote.queued_orders) { const item = document.createElement('li'); item.textContent = '保留订单 ' + row.order_no + '：' + row.validity_days + '天'; el('queue').append(item); }
      el('queue').hidden = !quote.queued_orders.length;
      el('quotas').replaceChildren();
      for (const row of quote.quotas) { const item = document.createElement('p'); item.className = 'quota'; item.textContent = '当前' + row.label + '已用 $' + Number(row.used).toFixed(2) + '，剩余 $' + Number(row.remaining).toFixed(2) + '（剩余部分将放弃）'; el('quotas').append(item); }
      el('preview').hidden = false;
      notice('已核对购买记录。请确认即将启用的订单及重置后的有效期。');
    } catch (error) { notice(error.message, 'bad'); }
    finally { busy = false; controls(); }
  }
  async function execute() {
    if (busy || completed || !quote || (!pending && !el('accepted').checked)) return;
    busy = true; controls(); notice('正在核对订单并重置，请稍候……');
    try {
      const receipt = await request('/execute', {method:'POST', headers:{'Content-Type':'application/json', 'X-Subscription-Reset-CSRF':quote.csrf_token}, body:JSON.stringify({quote_token:quote.quote_token, accepted_rules:true})});
      completed = true; pending = false; el('preview').hidden = true;
      notice('重置成功。新一期有效期为' + receipt.current_validity_days + '天，包含排队续购的总到期时间为' + date(receipt.expires_at) + '。可关闭此窗口返回订阅页面。', 'good');
      if (window.parent !== window && parentOrigin) window.parent.postMessage({type:'quya:subscription-reset-completed', old_subscription_id:receipt.old_subscription_id, new_subscription_id:receipt.new_subscription_id}, parentOrigin);
    } catch (error) {
      pending = error.retryable !== false;
      notice(pending ? error.message + '\n请点击“核对并重试”，系统会查询同一次重置，不会重复消耗续购。' : error.message, 'bad');
    } finally { busy = false; controls(); }
  }
  el('accepted').addEventListener('change', controls);
  el('confirm').addEventListener('click', execute);
  el('retry').addEventListener('click', execute);
  el('refresh').addEventListener('click', preview);
  if (initialError || subscriptionId <= 0) notice(initialError || '请从订阅页面选择需要重置的套餐。', 'bad'); else preview();
})();
</script>
</body>
</html>
