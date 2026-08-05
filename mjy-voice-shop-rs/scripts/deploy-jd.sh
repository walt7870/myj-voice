#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

REMOTE="${REMOTE:-root@jd}"
APP_DIR="${APP_DIR:-/opt/mjy-voice-shop-rs}"
SRC_DIR="${SRC_DIR:-/opt/mjy-voice-shop-rs-src}"
SERVICE_NAME="${SERVICE_NAME:-mjy-voice-shop-rs.service}"
OLD_SERVICE_NAMES="${OLD_SERVICE_NAMES:-mjy-order-mcp.service mjy_order_mcp.service mjy-mcp.service cbm_mcp.service}"
OLD_APP_DIRS="${OLD_APP_DIRS:-/opt/mjy-order-mcp /opt/mjy_order_mcp /opt/cbm_mcp}"
ROTATE_ADMIN_CREDENTIALS="${ROTATE_ADMIN_CREDENTIALS:-0}"

if [[ "$ROTATE_ADMIN_CREDENTIALS" != "0" && "$ROTATE_ADMIN_CREDENTIALS" != "1" ]]; then
  echo "ROTATE_ADMIN_CREDENTIALS must be 0 or 1" >&2
  exit 2
fi

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

echo "Packaging source for remote Linux build..."
export COPYFILE_DISABLE=1
TAR_ARGS=(
  --exclude="./target"
  --exclude="./node_modules"
  --exclude="./logs"
  --exclude="./.run"
  --exclude="./ui-report"
  --exclude="./mjy_voice_shop.db"
  --exclude="./.env"
  --exclude="./.DS_Store"
)
tar "${TAR_ARGS[@]}" -czf "$TMP_DIR/mjy-voice-shop-rs-source.tar.gz" .

echo "Uploading source package to ${REMOTE}..."
scp "$TMP_DIR/mjy-voice-shop-rs-source.tar.gz" "${REMOTE}:/tmp/mjy-voice-shop-rs-source.tar.gz"

echo "Building and installing on ${REMOTE}..."
ssh "$REMOTE" "APP_DIR='$APP_DIR' SRC_DIR='$SRC_DIR' SERVICE_NAME='$SERVICE_NAME' OLD_SERVICE_NAMES='$OLD_SERVICE_NAMES' OLD_APP_DIRS='$OLD_APP_DIRS' ROTATE_ADMIN_CREDENTIALS='$ROTATE_ADMIN_CREDENTIALS' bash -s" <<'REMOTE_SCRIPT'
set -euo pipefail

if [[ "$(id -u)" == "0" ]]; then
  SUDO=""
else
  SUDO="sudo"
fi

timestamp="$(date +%Y%m%d%H%M%S)"
backup_dir="${APP_DIR}-backups/${timestamp}"
initial_admin_password=""

ensure_rust() {
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
    local version
    version="$(rustc --version | awk '{print $2}')"
    if printf '%s\n%s\n' "1.85.0" "$version" | sort -V -C; then
      return
    fi
  fi
  echo "Installing Rust toolchain through rsproxy mirror..."
  curl -L --connect-timeout 10 --max-time 120 \
    https://rsproxy.cn/rustup/dist/x86_64-unknown-linux-gnu/rustup-init \
    -o /tmp/rustup-init
  chmod +x /tmp/rustup-init
  RUSTUP_DIST_SERVER=https://rsproxy.cn \
    RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup \
    RUSTUP_INIT_SKIP_PATH_CHECK=yes \
    /tmp/rustup-init -y --profile minimal --default-toolchain stable
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
}

configure_cargo_mirror() {
  mkdir -p "$HOME/.cargo"
  cat > "$HOME/.cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
EOF
}

service_exists() {
  local svc="$1"
  systemctl list-unit-files --type=service --no-pager 2>/dev/null | awk '{print $1}' | grep -Fx "$svc" >/dev/null 2>&1
}

ensure_rust
configure_cargo_mirror

$SUDO rm -rf "$SRC_DIR"
$SUDO mkdir -p "$SRC_DIR" "$APP_DIR"
$SUDO tar -xzf /tmp/mjy-voice-shop-rs-source.tar.gz -C "$SRC_DIR"

cd "$SRC_DIR"
cargo build --release

for svc in $OLD_SERVICE_NAMES; do
  [[ "$svc" == *.service ]] || svc="${svc}.service"
  if service_exists "$svc"; then
    echo "Disabling old MCP service: $svc"
    $SUDO systemctl stop "$svc" || true
    $SUDO systemctl disable "$svc" || true
  fi
done

for old_dir in $OLD_APP_DIRS; do
  if [[ -d "$old_dir" ]]; then
    archived="${old_dir}.disabled-${timestamp}"
    echo "Archiving old MCP directory: $old_dir -> $archived"
    $SUDO mv "$old_dir" "$archived"
  fi
done

deployment_started=0
rollback_on_error() {
  local exit_code="$?"
  if [[ "${deployment_started:-0}" == "1" ]]; then
    echo "Deployment failed; restoring release files from ${backup_dir}" >&2
    $SUDO systemctl stop "$SERVICE_NAME" || true
    if [[ -f "${backup_dir}/mjy-voice-shop-rs" ]]; then
      $SUDO install -m 0755 "${backup_dir}/mjy-voice-shop-rs" "${APP_DIR}/mjy-voice-shop-rs"
    fi
    if [[ -f "${backup_dir}/mjy-admin-password" ]]; then
      $SUDO install -m 0755 "${backup_dir}/mjy-admin-password" "${APP_DIR}/mjy-admin-password"
    fi
    if [[ -d "${backup_dir}/static" ]]; then
      $SUDO rm -rf "${APP_DIR}/static"
      $SUDO cp -a "${backup_dir}/static" "${APP_DIR}/static"
    fi
    if [[ -f "${backup_dir}/.env" ]]; then
      $SUDO install -m 0600 "${backup_dir}/.env" "${APP_DIR}/.env"
    fi
  fi
  $SUDO systemctl start "$SERVICE_NAME" || true
  exit "$exit_code"
}
trap rollback_on_error ERR

if service_exists "$SERVICE_NAME"; then
  $SUDO systemctl stop "$SERVICE_NAME"
fi
$SUDO mkdir -p "$backup_dir"
for path in mjy-voice-shop-rs mjy-admin-password static .env; do
  if [[ -e "${APP_DIR}/${path}" ]]; then
    $SUDO cp -a "${APP_DIR}/${path}" "$backup_dir/"
  fi
done
database_url="$(awk -F= '/^DATABASE_URL=/{sub(/^[^=]*=/, ""); print}' "${APP_DIR}/.env" 2>/dev/null | tail -n 1 | tr -d '"' || true)"
if [[ "$database_url" == sqlite://* ]]; then
  database_path="${database_url#sqlite://}"
  database_path="${database_path%%\?*}"
  if [[ "$database_path" != /* ]]; then
    database_path="${APP_DIR}/${database_path}"
  fi
  if [[ -f "$database_path" ]]; then
    $SUDO cp -a "$database_path" "${backup_dir}/database.sqlite3"
    $SUDO python3 - "${backup_dir}/database.sqlite3" <<'PY'
import sqlite3
import sys

connection = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
result = connection.execute("PRAGMA quick_check").fetchone()[0]
if result != "ok":
    raise SystemExit("SQLite backup quick_check failed: " + result)
PY
    echo "Database backup verified: ${backup_dir}/database.sqlite3"
  fi
fi
echo "Release backup: ${backup_dir}"
deployment_started=1

$SUDO install -m 0755 "${SRC_DIR}/target/release/mjy-voice-shop-rs" "${APP_DIR}/mjy-voice-shop-rs"
$SUDO install -m 0755 "${SRC_DIR}/target/release/mjy-admin-password" "${APP_DIR}/mjy-admin-password"
$SUDO rm -rf "${APP_DIR}/static.next"
$SUDO cp -R "${SRC_DIR}/static" "${APP_DIR}/static.next"
$SUDO rm -rf "${APP_DIR}/static"
$SUDO mv "${APP_DIR}/static.next" "${APP_DIR}/static"

if [[ ! -f "${APP_DIR}/.env" && -f "${SRC_DIR}/.env.example" ]]; then
  $SUDO install -m 0600 "${SRC_DIR}/.env.example" "${APP_DIR}/.env"
fi

$SUDO sed -i '/^ADMIN_USERNAME=/d' "${APP_DIR}/.env"
printf '\n%s\n' 'ADMIN_USERNAME=myjadmin' | $SUDO tee -a "${APP_DIR}/.env" >/dev/null
if [[ "$ROTATE_ADMIN_CREDENTIALS" == "1" ]] || ! $SUDO grep -q '^ADMIN_PASSWORD_HASH=\$argon2id\$' "${APP_DIR}/.env"; then
  initial_admin_password="$("${APP_DIR}/mjy-admin-password" generate)"
  admin_password_hash="$(printf '%s' "$initial_admin_password" | "${APP_DIR}/mjy-admin-password" hash)"
  $SUDO sed -i '/^ADMIN_PASSWORD_HASH=/d' "${APP_DIR}/.env"
  printf 'ADMIN_PASSWORD_HASH=%s\n' "$admin_password_hash" | $SUDO tee -a "${APP_DIR}/.env" >/dev/null
fi
$SUDO chmod 0600 "${APP_DIR}/.env"

$SUDO install -m 0644 "${SRC_DIR}/deploy/mjy-voice-shop-rs.service" "/etc/systemd/system/${SERVICE_NAME}"
$SUDO systemctl daemon-reload
$SUDO systemctl enable --now "$SERVICE_NAME"

sleep 1
$SUDO systemctl --no-pager --full status "$SERVICE_NAME" | sed -n '1,18p'

port="$(awk -F= '/^PORT=/{print $2}' "${APP_DIR}/.env" 2>/dev/null | tail -n 1 | tr -d '"' || true)"
port="${port:-8787}"
curl -fsS "http://127.0.0.1:${port}/api/health"
echo
deployment_started=0
trap - ERR
if [[ -n "$initial_admin_password" ]]; then
  echo "INITIAL_ADMIN_USERNAME=myjadmin"
  echo "INITIAL_ADMIN_PASSWORD=${initial_admin_password}"
fi
echo "RELEASE_BACKUP=${backup_dir}"
REMOTE_SCRIPT

echo "Done. Service: ${SERVICE_NAME}, remote dir: ${APP_DIR}"
