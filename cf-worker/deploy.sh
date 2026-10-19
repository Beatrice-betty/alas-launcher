#!/usr/bin/env bash
# 把 stable.json 作为纯静态资源发布到 Cloudflare Worker。
#
# 用法（仓库根目录）:
#   bash cf-worker/deploy.sh [wrangler deploy 的额外参数]
#
# 只做本地校验、不上传:
#   bash cf-worker/deploy.sh --dry-run
#
# 该脚本不会生成 stable.json；CI 会先生成再复制到 public/updata/。
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

manifest="public/updata/stable.json"
wrangler_version="${WRANGLER_VERSION:-4.151.1}"

if [[ ! -s "${manifest}" ]]; then
  echo "错误: 缺少 ${manifest}。请先生成 stable.json 并复制到该路径。" >&2
  exit 1
fi

# 守住“纯静态”约束：一旦出现 main，请求就会进入 Worker 脚本并消耗配额。
if grep -Eq '"main"[[:space:]]*:' wrangler.jsonc; then
  echo "错误: wrangler.jsonc 不能包含 main 入口；stable.json 必须作为静态资源发布。" >&2
  exit 1
fi

# 同理，run_worker_first: true 会把请求交给 Worker 脚本。
if grep -Eq '"run_worker_first"[[:space:]]*:[[:space:]]*true' wrangler.jsonc; then
  echo "错误: wrangler.jsonc 不能启用 run_worker_first；stable.json 必须走静态资源层。" >&2
  exit 1
fi

node - "${manifest}" <<'JS'
const fs = require("node:fs");

const path = process.argv[2];
const manifest = JSON.parse(fs.readFileSync(path, "utf8"));
const platforms = manifest.platforms ? Object.keys(manifest.platforms) : [];
if (!manifest.version || platforms.length === 0) {
  throw new Error(`${path} 缺少 version 或 platforms，拒绝发布`);
}
console.log(`准备发布 stable.json ${manifest.version}（${platforms.length} 个平台）`);
JS

exec npx --yes "wrangler@${wrangler_version}" deploy "$@"
