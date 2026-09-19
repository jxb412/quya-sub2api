# Codex Native Transport 插件 — Sub2API(S2A)部署手册

> 面向对象:人工运维 **或** AI Agent。
> 目标:把 `codex-native-transport` 插件(`.s2plugin` 包)部署 / 升级到运行中的 Sub2API 宿主,并验证成功。
> 适用插件 id key:`io.sub2api.codex-native-transport`。

本手册分两条主线:
- **Track A — 部署已有的 `.s2plugin` 包**(最常见,不需要 Rust 环境)。
- **Track B — 从源码构建 + 签名打包**(只有需要出新版本时才用)。

大多数情况你只需要 **Track A**。

---

## 0. 术语与关键事实(执行前必读)

| 概念 | 值 / 说明 |
|---|---|
| 插件 key | `io.sub2api.codex-native-transport`(升级时按 key upsert,插件数据库 id 不变) |
| 可锁票 | turn-state 长度 **292 或 332** 字节,可锁进养池 |
| 锁票后出口 | `use_account_proxy_after_lock=true`(默认)时回到账号原代理;关闭时继续使用铸出该锁票的代理池出口。旧版本锁票没有出口槽位记录,会安全回退账号原代理,等新票重铸后再固定池出口 |
| pin 关注模型(≥0.6.21) | `warming_models`(默认 `gpt-6-astra` + `gpt-5.6-sol`)。**只有列表里的模型**参与 pin:铸票出口池、身份轮换、票注入、被动捕获、休息编排、养池、面板。其它模型(gpt-5.2、codex-auto-review…)在 pin 模式下一律直通,不建格、不耗额度。pin 模式下列表不能为空 |
| 假6 / 酱汁票 | turn-state 长度 **312**,不锁,继续试 |
| 格级休息(≥0.6.21) | **按 (号×模型) 触发,不按时间**。某格出口池已整整轮转一圈(每个出口连续 3 次非 292 即换下一个,换满 `pool.len()` 个)仍无 292 → 这一格休息 `warming_rest_seconds`:不探铸、真实流量直通(走账号自己出口、保留客户端身份、不注入),不碰宿主。持有 292 的格永不休息。同一号 astra 有 292、terra 只出 312 时只有 terra 歇 |
| 账号级排空(≥0.6.21) | 叠加在格级之上:本轮有格进入休息、且该号在 `warming_models` 上**没有任何**有效 292、且距上次恢复满 `warming_duty_seconds`(防抖)→ 调 admin API 把该号 `priority` 临时改成 `warming_drain_priority`(默认 9999)。宿主只在分配新会话时比较优先级,粘性会话命中路径不看它——老会话继续留在原号(不打散、提示缓存不丢),闲置满宿主粘性 TTL(1h)自然脱落。到点写回原优先级。**不再翻 `schedulable`**。任一模型手里有 292 的号优先级不动 |
| 放弃铸票(≥0.6.21) | 某(号×模型)格连续 `pin_giveup_rounds`(默认 3)轮「转满一圈→格级休息」仍无 292,进入放弃态 `pin_giveup_retry_seconds`(默认 6h):真实流量退回直通(走账号自己的出口、保留客户端会话身份、不换身份、不注入),养池不再探它;到期后只给一圈机会,仍不行立即重新放弃。锁到 292 即清零 |
| 宿主 admin API 基址 | 在宿主容器内为 `http://127.0.0.1:8080` |
| 鉴权方式 | **admin API key(`x-api-key` 头)**。step-up 关闭时对所有 admin 端点有效,无需 JWT |
| admin key 来源 | 存在 Sub2API 的 `settings` 表,key=`admin_api_key`,**明文**,可用 SQL 直接取(见下) |
| 配置生效方式 | **热更新**,保存即通过 gRPC 推给运行中插件,**不需要重启** |
| 升级是否保留配置 | 是。按 key upsert 会保留 `ConfigEncrypted`;本手册仍会显式回灌一次以强制新版本重新解析 |
| 历史废弃配置键 | `warp_*`、`one_id_per_request`(≥0.6.12 的插件 `parse()` 会自动剥离,升级不炸旧配置) |
| 控制面板 | 插件内置,监听 `panel_addr`(如 `0.0.0.0:8848`),鉴权用 `panel_token`(Bearer) |
| 签名 | Ed25519,`key_id=codex-native-transport-publisher-v1`;宿主 `trusted_publishers` 需登记对应公钥 |

**⚠️ 凭据处理原则**:本文档**不写死**任何 SSH 密码 / admin key。
- SSH 密码通过环境变量 `SSHPASS` 传入(见 §1)。
- admin key 由脚本在宿主上**实时从数据库读取**,永远不落进文档或命令历史里。

---

## 1. 环境变量(执行任何步骤前先设好)

在**你本地的操作终端**导出以下变量(按实际情况填写):

```bash
export S2A_HOST="root@<服务器IP>"        # 例: root@38.49.38.72
export SSHPASS="<SSH密码>"               # 密码;或改用 -i 密钥,见下
export SSH="sshpass -e ssh -o StrictHostKeyChecking=no -o ConnectTimeout=20 $S2A_HOST"
export SCP="sshpass -e scp -o StrictHostKeyChecking=no -o ConnectTimeout=30"
```

- 若用 SSH 密钥而非密码:把 `$SSH` 改成 `ssh -i <私钥路径> -o StrictHostKeyChecking=no $S2A_HOST`,`$SCP` 同理,并忽略 `SSHPASS`。
- 需要 `sshpass`(macOS: `brew install hudochenkov/sshpass/sshpass`;Debian/Ubuntu: `apt-get install sshpass`)。

**连通性自检 + 定位容器名**:

```bash
$SSH 'docker ps --format "{{.Names}}\t{{.Image}}\t{{.Ports}}" | grep -Ei "sub2api|postgres"'
```

预期看到两个容器(名字可能不同,记下来替换后续命令):
- 应用容器:默认名 `sub2api`(镜像 `weishaw/sub2api:*`,发布 `0.0.0.0:8080->8080`)。
- 数据库容器:默认名 `sub2api-postgres`(镜像 `postgres:*`)。

```bash
export APP_CTN="sub2api"           # 按上面实际输出替换
export DB_CTN="sub2api-postgres"   # 按上面实际输出替换
export DB_USER="sub2api"           # 数据库用户(容器 env DATABASE_USER)
export DB_NAME="sub2api"           # 数据库名(容器 env DATABASE_DBNAME)
```

> 如需确认 DB 用户/库名:`$SSH "docker exec $APP_CTN sh -c 'env | grep DATABASE_'"`。

---

## 2. Track A — 部署已有的 `.s2plugin` 包

### 2.1 前置检查:确认包是「新鲜且干净」的

拿到 `.s2plugin` 包后,**先在本地校验**它确实是目标版本、且不含已废弃功能。跳过这步是 90% 事故的来源。

```bash
export PKG_LOCAL="/path/to/codex-native-transport-<版本>.s2plugin"

# 2.1.1 包结构 + 版本 + 签名存在
unzip -l "$PKG_LOCAL"                       # 应含 manifest.json / signature.json / runtimes/*/plugin / ui/index.html
unzip -p "$PKG_LOCAL" manifest.json | grep '"version"'
unzip -p "$PKG_LOCAL" signature.json        # 有则为签名包

# 2.1.2 确认 linux 二进制里没有已删除的功能残留(以 WARP 路由为例)
unzip -p "$PKG_LOCAL" runtimes/linux-amd64/plugin | strings -a | grep -c "/api/warp/rotate" \
  && echo "⚠️ 含 WARP 路由,可能是旧二进制" || echo "干净"

# 2.1.3 记录本地包 sha,上传后核对
shasum -a 256 "$PKG_LOCAL"
```

### 2.2 上传包到宿主

```bash
$SCP "$PKG_LOCAL" "$S2A_HOST:/tmp/cnt-deploy.s2plugin"
# 核对上传后 sha 与本地一致
$SSH 'sha256sum /tmp/cnt-deploy.s2plugin'
```

两个 sha 必须完全一致,否则重传。

### 2.3 执行部署(一键脚本)

下面这段通过 stdin 把 Python 脚本喂给宿主的 `python3` 运行。它会:
读取 admin key → 找到插件 id → 备份当前配置 → 停用 → 上传新包 → 启用(接受未测试兼容 + 100% 灰度)→ 回灌配置(强制新版本重新解析,顺带清掉废弃键)→ 验证。

```bash
$SSH "APP_CTN='$APP_CTN' DB_CTN='$DB_CTN' DB_USER='$DB_USER' DB_NAME='$DB_NAME' python3 -" <<'PYEOF'
import json, os, subprocess, sys, time

APP=os.environ["APP_CTN"]; DBC=os.environ["DB_CTN"]
DBU=os.environ["DB_USER"]; DBN=os.environ["DB_NAME"]
PKG="/tmp/cnt-deploy.s2plugin"
BASE="http://127.0.0.1:8080/api/v1/admin/plugins"
KEYNAME="io.sub2api.codex-native-transport"

def sh(c): return subprocess.check_output(["sh","-c",c]).decode()

# 1) 实时取 admin key(明文,存 settings 表)
K = sh(f"docker exec {DBC} psql -U {DBU} -d {DBN} -tAc "
       f"\"select value from settings where key='admin_api_key';\"").strip()
if not K:
    sys.exit("❌ settings.admin_api_key 为空,无法鉴权。请先在后台生成 admin API key。")
H = f'-H "x-api-key: {K}"'

def api(method, path, extra=""):
    return sh(f'curl -s -X {method} {H} {extra} "{BASE}{path}"')

# 2) 定位插件 id(按 key)
lst = json.loads(api("GET",""))["data"]
match = [p for p in lst if p["plugin_key"]==KEYNAME]
if not match:
    sys.exit(f"❌ 未找到插件 {KEYNAME},请确认这是全新安装还是升级。全新安装见手册 §2.4。")
PID = match[0]["id"]
print(f"插件 id={PID} 当前版本={match[0]['version']} 状态={match[0]['state']}")

# 3) 备份当前配置
cfg = api("GET", f"/{PID}/config")
open("/tmp/cnt_cfg_backup.json","w").write(cfg)
try:
    cfg_obj = json.loads(cfg); has_cfg = isinstance(cfg_obj,dict) and bool(cfg_obj)
except Exception:
    has_cfg = False
print(f"配置备份 {len(cfg)} 字节, 有效={has_cfg}")

# 4) 停用(升级同 id 前必须先停用)
print("停用:", api("POST", f"/{PID}/disable")[:120])

# 5) 上传新包(multipart 字段名固定为 plugin)
up = json.loads(api("POST", "/upload", f'-F "plugin=@{PKG}"'))
if up.get("code")!=0:
    sys.exit(f"❌ 上传失败: {up}")
PID = up["data"]["id"]  # upsert 后 id 不变,仍以返回为准
print(f"上传成功 -> id={PID} version={up['data']['version']}")

# 6) 启用(接受未声明测试的兼容版本 + 全量灰度)
en = json.loads(api("POST", f"/{PID}/enable",
                    '-H "Content-Type: application/json" -d \'{"accept_untested":true,"rollout_percent":100}\''))
if en.get("code")!=0:
    sys.exit(f"❌ 启用失败: {en}")
print("启用成功")

# 7) 回灌配置(强制新版本重新解析,自动剥离废弃键)
if has_cfg:
    r = api("PUT", f"/{PID}/config",
            '-H "Content-Type: application/json" --data-binary @/tmp/cnt_cfg_backup.json')
    print("回灌配置:", r[:120])
else:
    print("无有效旧配置 -> 跳过回灌(升级已自动沿用)")

# 8) 验证
time.sleep(2)
d = json.loads(api("GET", f"/{PID}"))["data"]
print("="*50)
print(f"版本={d['version']} 状态={d['state']} 运行健康={d['runtime_healthy']} "
      f"签名={d['signature_status']} 信息={d['runtime_message']}")
ok = d['state']=='enabled' and d['runtime_healthy'] and d['version']==up['data']['version']
print("✅ 部署成功" if ok else "⚠️ 状态异常,请检查上面输出")

# 9) 清理含密钥的临时文件
sh("shred -u /tmp/cnt_cfg_backup.json 2>/dev/null; rm -f /tmp/cnt_cfg_backup.json")
PYEOF
```

**判定成功**:最后一行是 `✅ 部署成功`,且 `状态=enabled 运行健康=True 签名=trusted`。

### 2.4 全新安装(宿主上还没有这个插件)

流程相同但**跳过「停用」步骤**(没有旧版本),`enable` 前先 `upload`。把 §2.3 脚本里第 4 步(disable)删掉,第 2 步找不到 id 时改为直接 upload 拿新 id 即可。

### 2.5 部署后收尾清理

```bash
$SSH 'rm -f /tmp/cnt-deploy.s2plugin'
```

---

## 3. 部署后验证(强烈建议)

### 3.1 确认存量配置已无废弃键

```bash
$SSH "python3 -" <<'PYEOF'
import json, os, subprocess
DBC=os.environ.get("DB_CTN","sub2api-postgres"); DBU=os.environ.get("DB_USER","sub2api"); DBN=os.environ.get("DB_NAME","sub2api")
def sh(c): return subprocess.check_output(["sh","-c",c]).decode()
K=sh(f"docker exec {DBC} psql -U {DBU} -d {DBN} -tAc \"select value from settings where key='admin_api_key';\"").strip()
lst=json.loads(sh(f'curl -s -H "x-api-key: {K}" http://127.0.0.1:8080/api/v1/admin/plugins'))["data"]
pid=[p["id"] for p in lst if p["plugin_key"]=="io.sub2api.codex-native-transport"][0]
cfg=json.loads(sh(f'curl -s -H "x-api-key: {K}" http://127.0.0.1:8080/api/v1/admin/plugins/{pid}/config'))
legacy=[k for k in cfg if k.startswith("warp") or k=="one_id_per_request"]
print("插件 id=", pid)
print("legacy 键:", legacy or "无(干净)")
print("turn_state_mode=", cfg.get("turn_state_mode"),
      "| 出口池行数=", len([l for l in (cfg.get("egress_pool") or "").splitlines() if l.strip() and not l.strip().startswith('#')]))
PYEOF
```

> 本脚本已自动按 key 查 id,无需手填。

### 3.2 读取控制面板实时养池状态

面板监听在容器内,需从容器内部访问 + Bearer token:

```bash
$SSH "python3 -" <<'PYEOF'
import json, os, subprocess
DBC=os.environ.get("DB_CTN","sub2api-postgres"); DBU=os.environ.get("DB_USER","sub2api")
DBN=os.environ.get("DB_NAME","sub2api"); APP=os.environ.get("APP_CTN","sub2api")
def sh(c): return subprocess.check_output(["sh","-c",c]).decode()
K=sh(f"docker exec {DBC} psql -U {DBU} -d {DBN} -tAc \"select value from settings where key='admin_api_key';\"").strip()
lst=json.loads(sh(f'curl -s -H "x-api-key: {K}" http://127.0.0.1:8080/api/v1/admin/plugins'))["data"]
pid=[p["id"] for p in lst if p["plugin_key"]=="io.sub2api.codex-native-transport"][0]
cfg=json.loads(sh(f'curl -s -H "x-api-key: {K}" http://127.0.0.1:8080/api/v1/admin/plugins/{pid}/config'))
T=cfg["panel_token"]
raw=sh(f'docker exec {APP} sh -c \'wget -qO- --header="Authorization: Bearer {T}" http://127.0.0.1:8848/api/status\'')
d=json.loads(raw)
print(f"mode={d['mode']} 主动养池={d['active_warming']} 被动={d['passive_warming']} 全池={d['admin_warming']} 出口池={d['egress_pool_size']}")
for a in d.get("accounts",[]):
    for m in a.get("models",[]):
        print(f"  acct#{a['id']:<3}{m['model']:22} state={m['state']:<8} locked={m['locked']} "
              f"ticket_len={m.get('ticket_len')} last_seen={m.get('last_seen_len')} "
              f"ok={m.get('ok')} ov={m.get('ov')} egress_idx={m.get('egress_idx')}")
PYEOF
```

**读面板速查**:
- `locked=True ticket_len=292/332` = 锁到有效票,正常。
- `state=degraded last_seen=292 locked=False` = 曾拿过 292,现在没票(历史高水位,不是当前有票)。
- `last_seen=312` = 该(号×出口IP×模型)只出假6,上游拒发真6 → 需要更干净的出口 IP。
- `egress_idx` 在涨 = 养池正在换出口重试(每 +1 代表连吃 3 次非-292)。
- `egress_idx=-1` = 已锁票走原生出口,养池跳过该格。

---

## 4. 配置变更(热更新,无需重启)

改配置有两种途径,**都不需要重启插件**:
1. **插件控制台 UI**(推荐):后台 → 插件 → 打开控制台 → 改表单 → 保存。
2. **admin API**:`PUT /api/v1/admin/plugins/<id>/config`,body 是完整配置 JSON。

保存后插件立即通过 gRPC 收到新配置,转发路径与养池循环每轮重读,秒级生效。

**验证是否生效**(以出口池为例):对比「存量配置行数」与「运行中 `egress_pool_size`」,两者相等即已生效:

```bash
# 见 §3.1 取配置行数、§3.2 取运行中 egress_pool_size,二者应一致
```

> 出口轮转器游标按 `pool.len()` 取模,增删 IP 会自动适配,不产生需要重启的陈旧状态。

### 4.1 休息编排相关键(≥0.6.21)

| 键 | 默认 | 含义 |
|---|---|---|
| `warming_rest_seconds` | 600 | 休息时长(秒),0 = 不休息。格级:该格停探、直通;账号级:优先级改成排空值 |
| `warming_duty_seconds` | 1800 | 同一号两次账号级排空之间的最短活跃时间(秒),0 = 不限。**不再是触发条件**,只做防抖;不影响格级休息 |
| `warming_drain_priority` | 9999 | 休息期间写入宿主的调度优先级(数值越大越靠后) |
| `pin_giveup_rounds` | 3 | 连续多少轮「转满一圈→休息」后放弃该格铸票,0 = 永不放弃 |
| `pin_giveup_retry_seconds` | 21600 | 放弃态持续时长(秒),到期给一圈机会 |
| `egress_advance_threshold` | 3 | 同一格连续多少次非 292 就换下一个出口(≥0.6.22 可配,1..10)。一圈 = 出口数 × 本值 次失败 |
| `active_warming_interval_seconds` | 15 | 主动养池探铸间隔,≥0.6.22 下限放宽到 1 秒(全池养池下限 5 秒) |

休息编排生效条件:`turn_state_mode=pin` + 开了任一养池(主动/全池) + `admin_api_base`/`admin_api_key` 配齐 + `warming_rest_seconds>0` + `egress_pool` 非空(没有出口池就没有「转满一圈」这个信号,不会休息)。

休息集(含原优先级)落在 `pin_persist_path` 的 `.rest` 兄弟文件里;插件重启后到点写回,停用/升级/SIGTERM 时关停前主动写回。**从 ≤0.6.20 升级**:旧版本的休息集是 `schedulable=false` 语义,新版本启动时会忽略这些记录——升级前先按 §2.3 的脚本把休息中的号恢复调度。

面板 `/api/status` 新增字段:账号级 `orig_priority`(排空中显示原优先级);格级 `resting` / `rest_s`(格级休息)、`abandoned` / `abandoned_s`、`stuck_rounds`、`lap_exhausted`。

### 4.2 出口池:每行一个独立出口(≥0.7.2)

出口池按**行号**隔离 client:第 N 行有自己的一条连接,同一代理 URL 复制多行就是多个独立出口。配合「一条新连接分配一个新 IP」的网关(如 `socks5h://user:pass@gate-us.vaultproxies.com:31`,实测每条连接落在不同 /48 的 IPv6;注意必须用 `socks5h://`,`socks5://` 会因本地解析成 IPv4 而连不上),复制 N 行就得到 N 个不同出口,一圈 = N × `egress_advance_threshold`。

`use_account_proxy_after_lock` 控制锁票后的业务出口:
- `true`(默认):代理池只用于铸票,锁定后业务请求回到账号自身配置的代理。
- `false`:锁定后继续使用实际铸出该票的代理池行。该行号随票持久化,插件重启后仍保持;代理池缩短时按新池长度安全取模。

历史:0.7.0 的「IPv6 轮换代理接口」(`egress_api_*`)与 0.7.1 的 `egress_pool_fresh_client` / `egress_pool_lap_size` 均已移除,升级时自动剥离这些键。

面板(≥0.7.1)新增:`POST /api/unpark?account=&model=` 与 `POST /api/unpark-all`,清除放弃态 / 格级休息 / 卡住轮数并重开一圈;页面上每个停铸格有「解除放弃/休息」按钮,顶部有「全部解除」。诊断 JSONL(≥0.7.1)每行新增 `path`(forward / warm / admin_warm)与 `egress`(本次铸票出口 host:port),养池探铸也会记录,可按出口统计 292 率。

## 5. 回滚

若新版本运行不健康,回滚到上一个已知良好的包:

```bash
# 把旧版本 .s2plugin 当作新包,重跑 Track A(§2.2 + §2.3)即可。
# id 不变,配置会自动沿用/回灌。
```

紧急情况下也可只**停用**插件让流量回退到宿主默认出站:

```bash
$SSH "python3 -" <<'PYEOF'
import json,os,subprocess
DBC=os.environ.get("DB_CTN","sub2api-postgres");DBU=os.environ.get("DB_USER","sub2api");DBN=os.environ.get("DB_NAME","sub2api")
def sh(c): return subprocess.check_output(["sh","-c",c]).decode()
K=sh(f"docker exec {DBC} psql -U {DBU} -d {DBN} -tAc \"select value from settings where key='admin_api_key';\"").strip()
lst=json.loads(sh(f'curl -s -H "x-api-key: {K}" http://127.0.0.1:8080/api/v1/admin/plugins'))["data"]
pid=[p["id"] for p in lst if p["plugin_key"]=="io.sub2api.codex-native-transport"][0]
print(sh(f'curl -s -X POST -H "x-api-key: {K}" http://127.0.0.1:8080/api/v1/admin/plugins/{pid}/disable')[:120])
PYEOF
```

---

## 6. Track B — 从源码构建 + 打包(仅发新版本时)

### 6.1 本地前置

- Rust + 目标:`rustup target add aarch64-apple-darwin x86_64-unknown-linux-musl`。
- musl 交叉工具链:提供 `x86_64-linux-musl-gcc`(macOS 可 `brew install FiloSottile/musl-cross/musl-cross`)。
- 签名私钥:`~/.sub2api/plugin-keys/codex-native-transport.key`(**只有原发布者有**,详见 §6.5)。
- Python 3(打包脚本用)。

### 6.2 改版本号

编辑 `Cargo.toml` 的 `version = "x.y.z"`。

### 6.3 测试 + 构建双平台

```bash
cd plugins/codex-native-transport
cargo test                                   # 应全绿

# darwin-arm64(本机)
cargo build --release

# linux-amd64(musl 静态)
CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc \
  cargo build --release --target x86_64-unknown-linux-musl
```

### 6.4 ⚠️ 关键坑:先定位真正的输出目录,别信 `./target`

某些环境(如带沙箱的 IDE)会把 `CARGO_TARGET_DIR` **重定向到临时目录**,导致 `./target/release/plugin` 是旧文件、而新二进制在别处。**打包前务必按下面方式取真实路径并校验新鲜度**:

```bash
# 取真实输出根目录
TGT="$(cargo metadata --format-version=1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
echo "target_directory=$TGT"

DARWIN="$TGT/release/plugin"
LINUX="$TGT/x86_64-unknown-linux-musl/release/plugin"

# 校验:mtime 是刚构建的、文件类型正确
ls -la "$DARWIN" "$LINUX"
file "$DARWIN"   # 期望 Mach-O ... arm64
file "$LINUX"    # 期望 ELF 64-bit ... x86-64 ... static-pie ... stripped

# 校验:确实包含本次改动 / 不含已删功能(以 WARP 路由为例)
strings -a "$LINUX" | grep -c "/api/warp/rotate" && echo "⚠️ 仍含 WARP,构建没生效" || echo "干净"
```

若二进制 mtime 是旧的:`touch src/*.rs` 强制重编,再重跑 §6.3。

### 6.5 打包 + 签名

```bash
python3 tools/package.py \
  --runtime "darwin-arm64=$DARWIN" \
  --runtime "linux-amd64=$LINUX" \
  --sign-key ~/.sub2api/plugin-keys/codex-native-transport.key
# 产物: dist/codex-native-transport-<版本>.s2plugin
```

- **有签名私钥**:产出可被 `trusted_publishers` 信任的签名包(`signature_status=trusted`)。
- **没有签名私钥**(别人重打包):两条路——
  1. 生成新发布者密钥(`python3 tools/ed25519_tool.py keygen`),用 `--sign-key <新key> --key-id <新id>` 打包,并把打印出来的公钥登记进宿主配置 `plugins.trusted_publishers.<新id>: "<公钥base64>"`;
  2. 或不签名打包(去掉 `--sign-key`),并在宿主开 `plugins.allow_unsigned: true`(仅建议内网/调试)。

当前发布者信息(供登记参考):
- `key_id`: `codex-native-transport-publisher-v1`
- 公钥(base64): `wX85IXM43wm/U+9ydMsdBDPHHM0V2u+BO9F6fZ8k54Y=`

打完包后回到 **Track A** 部署。

---

## 7. 故障排查速查表

| 现象 | 原因 | 处理 |
|---|---|---|
| `请先停用当前插件，再上传同 ID 的新版本` | 升级前没停用 | 先 `POST /:id/disable` 再 upload(§2.3 已含) |
| `INVALID_ADMIN_KEY` / 401 | admin key 取错 / 为空 / step-up 开着 | 确认 `settings.admin_api_key` 非空;`select value from settings where key='step_up_enabled'` 应为 `false`;为 true 时 x-api-key 被禁,需改用管理员 JWT |
| 上传成功但版本没变 / 功能没更新 | **打了旧二进制**(CARGO_TARGET_DIR 重定向坑) | 按 §6.4 用 `cargo metadata` 定位真实路径 + `strings` 校验后重新打包 |
| 升级后插件起不来 / 配置解析失败 | 旧配置含新版本不认的键 | ≥0.6.12 已自动剥离 `warp_*`/`one_id_per_request`;其它未知键需从配置里删掉 |
| `signature_status` 非 `trusted` | 公钥没登记 / 用了别的 key 签 / 未签名 | 登记 `trusted_publishers` 或开 `allow_unsigned`(§6.5) |
| 面板连不上 | `panel_addr` 为空 / token 错 / 在容器外访问 | 面板须在容器内访问(`docker exec`),带 `Authorization: Bearer <panel_token>` |
| 养池「又降智又票292」 | 面板在未锁票时显示的是 `last_seen_len`(历史高水位),非当前票 | 属正常显示;`locked=False`+`ticket_len=0` 即当前无票 |
| 某些号一直造不出 292 | 上游对该(号×出口IP×模型)只发假6(312) | 换更干净的出口 IP 进 `egress_pool`;6 档模型门槛比 gpt-5.5 高。≥0.6.21 这类格会在 `pin_giveup_rounds` 轮后自动放弃并退回直通,面板显示「已放弃铸票·直通」,不再无限烧额度 |
| 某号优先级停在 9999 不回来 | 插件在休息中被强杀(未走 Shutdown/SIGTERM)且没配 `pin_persist_path` | 配 `pin_persist_path`(休息集落盘,重启到点写回);或手动 `PUT /api/v1/admin/accounts/:id` body `{"priority":原值}` |
| 缓存命中率随休息掉下来 | 说明还在跑 ≤0.6.20:休息是 `schedulable=false` 摘号,宿主会清掉粘性会话 | 升级到 ≥0.6.21(优先级排空,老会话不打散) |
| `runtime_healthy=False` | 插件进程崩溃 / 二进制平台不匹配 | 检查宿主架构是否为 linux-amd64;查 `runtime_message`;必要时回滚(§5) |

---

## 8. Admin API 端点参考

基址(容器内):`http://127.0.0.1:8080/api/v1/admin/plugins`
鉴权:`x-api-key: <admin_api_key>`(step-up 关闭时通用)

| 方法 | 路径 | 用途 | body |
|---|---|---|---|
| GET | `` | 列出所有插件 | — |
| GET | `/:id` | 单个插件详情(版本/状态/健康) | — |
| GET | `/:id/config` | 取当前配置 JSON | — |
| PUT | `/:id/config` | 保存配置(热生效) | 完整配置 JSON |
| POST | `/upload` | 上传 `.s2plugin` | multipart,字段名 `plugin` |
| POST | `/:id/enable` | 启用 | `{"accept_untested":true,"rollout_percent":100}` |
| POST | `/:id/disable` | 停用 | — |
| POST | `/:id/test` | 兼容性/连通性自检 | — |
| DELETE | `/:id` | 卸载 | — |

插件自身调用的宿主账号端点(基址 `http://127.0.0.1:8080/api/v1/admin/accounts`):

| 方法 | 路径 | 用途 | body |
|---|---|---|---|
| GET | `?platform=openai&type=oauth&status=active&lite=1` | 枚举可调度 openai oauth 号 | — |
| GET | `/data?platform=openai&type=oauth` | 导出凭据(全池养池取 bearer) | — |
| GET | `/:id` | 读账号当前 `priority`(休息前记原值) | — |
| PUT | `/:id` | 改账号 `priority`(休息排空 / 到点写回;指针字段局部更新,不动其它字段) | `{"priority": N}` |

---

## 9. 一页纸 SOP(给赶时间的执行者)

```bash
# 0. 设变量
export S2A_HOST="root@<IP>"; export SSHPASS="<密码>"
export SSH="sshpass -e ssh -o StrictHostKeyChecking=no $S2A_HOST"
export SCP="sshpass -e scp -o StrictHostKeyChecking=no"
export APP_CTN=sub2api DB_CTN=sub2api-postgres DB_USER=sub2api DB_NAME=sub2api
export PKG_LOCAL="/path/to/xxx.s2plugin"

# 1. 校验包
unzip -p "$PKG_LOCAL" manifest.json | grep version
shasum -a 256 "$PKG_LOCAL"

# 2. 上传 + 核对 sha
$SCP "$PKG_LOCAL" "$S2A_HOST:/tmp/cnt-deploy.s2plugin"
$SSH 'sha256sum /tmp/cnt-deploy.s2plugin'

# 3. 跑 §2.3 一键部署脚本

# 4. 看最后一行是否 ✅ 部署成功 + 状态=enabled 健康=True

# 5. 清理
$SSH 'rm -f /tmp/cnt-deploy.s2plugin'
```
