# Single Admin Device Authorization MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a usable single-`admin` login and device authorization management page without changing or invalidating existing device authentication.

**Architecture:** Read the single administrator username and Argon2id password hash from `.env`; store only opaque random browser Session hashes in SQLite. Keep `/api/device/auth`, device token signing, and `/api/device/voice` unchanged, while adding administrator-only CRUD operations over the existing `devices` table.

**Tech Stack:** Rust 2021, Axum 0.8, SQLx/SQLite, Argon2id, vanilla HTML/CSS/JavaScript, Playwright, Bash/systemd.

---

### Task 1: Environment Admin Credentials and Opaque Sessions

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/lib.rs`
- Create: `src/admin_auth.rs`
- Modify: `src/main.rs`
- Modify: `src/web/mod.rs`
- Modify: `tests/app_tests.rs`
- Test: `src/admin_auth.rs`

- [ ] **Step 1: Write failing authentication-domain tests**

Add tests for password verification, session creation/lookup/revocation, config fingerprint invalidation, and idempotent schema initialization:

```rust
#[tokio::test]
async fn opaque_session_is_revoked_and_invalidated_by_config_change() {
    let pool = test_pool().await;
    init_schema(&pool).await.unwrap();
    let config = AdminConfig::new("admin", hash_password("first-pass").unwrap()).unwrap();
    let token = create_session(&pool, &config).await.unwrap();
    assert!(load_session(&pool, &config, &token).await.unwrap());
    revoke_session(&pool, &token).await.unwrap();
    assert!(!load_session(&pool, &config, &token).await.unwrap());

    let second = create_session(&pool, &config).await.unwrap();
    let changed = AdminConfig::new("admin", hash_password("second-pass").unwrap()).unwrap();
    assert!(!load_session(&pool, &changed, &second).await.unwrap());
}
```

- [ ] **Step 2: Verify the tests fail before implementation**

Run: `DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test admin_auth::tests -- --nocapture`

Expected: compilation fails because `admin_auth` and its types do not exist.

- [ ] **Step 3: Implement the focused auth module**

Add `argon2 = { version = "0.5", features = ["std"] }` and `cookie = "0.18"`, then export `pub mod admin_auth;`. Define:

```rust
pub const ADMIN_COOKIE: &str = "mjy_admin_session";

#[derive(Clone)]
pub struct AdminConfig {
    pub username: Arc<String>,
    pub password_hash: Arc<String>,
    pub fingerprint: Arc<String>,
}

pub async fn init_schema(pool: &SqlitePool) -> Result<()>;
pub fn hash_password(password: &str) -> Result<String>;
pub fn verify_password(hash: &str, password: &str) -> bool;
pub async fn create_session(pool: &SqlitePool, config: &AdminConfig) -> Result<String>;
pub async fn load_session(pool: &SqlitePool, config: &AdminConfig, token: &str) -> Result<bool>;
pub async fn revoke_session(pool: &SqlitePool, token: &str) -> Result<()>;
```

Use this additive table:

```sql
CREATE TABLE IF NOT EXISTS admin_sessions (
  session_hash TEXT PRIMARY KEY,
  config_fingerprint TEXT NOT NULL,
  created_at TEXT NOT NULL,
  revoked_at TEXT
);
```

Generate 32 random bytes with `rand::rng()` and encode URL-safe base64 without padding. Store `SHA-256(token)` only. Build `config_fingerprint` as SHA-256 of a length-delimited username and password hash. `load_session` requires matching fingerprint and `revoked_at IS NULL`.

- [ ] **Step 4: Wire production configuration without touching device secrets**

In `main.rs`, call `admin_auth::init_schema(&pool)` after `db::init`. Read `ADMIN_USERNAME` and `ADMIN_PASSWORD_HASH`; public binds fail startup when either is missing, while loopback development uses an explicitly logged development-only hash. Add `admin_config: AdminConfig` to `AppState` and update test state constructors with a fixed test hash.

- [ ] **Step 5: Run focused and full tests**

Run:

```bash
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test admin_auth::tests -- --nocapture
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test device_auth_issues_token_for_seeded_demo_device -- --nocapture
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test --all-targets
```

Expected: all pass, including all existing device-token tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/admin_auth.rs src/main.rs src/web/mod.rs tests/app_tests.rs
git commit -m "功能：新增单管理员安全会话"
```

### Task 2: Login Boundary and Device Authorization APIs

**Files:**
- Create: `src/web/admin.rs`
- Modify: `src/web/mod.rs`
- Modify: `src/db.rs`
- Create: `src/bin/mjy-admin-password.rs`
- Test: `tests/app_tests.rs`

- [ ] **Step 1: Write failing public-boundary and authorization tests**

Add integration tests for login success/failure, five-failure limit, cookie attributes, logout, public `401`, local bypass, spoofed-header rejection, device list/create/update/reset, duplicate conflict, and preservation of existing device authentication. The preservation test snapshots the `devices` rows, performs login/logout, and asserts rows remain byte-for-byte equal.

Use exact new endpoints:

```text
POST /api/admin/auth/login
POST /api/admin/auth/logout
GET  /api/admin/auth/me
GET  /api/admin/device-authorizations
POST /api/admin/device-authorizations
PUT  /api/admin/device-authorizations/{device_id}
POST /api/admin/device-authorizations/{device_id}/reset-secret
```

- [ ] **Step 2: Verify the boundary tests fail**

Run: `DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test public_admin_login_and_device_authorization_management -- --nocapture`

Expected: `404` because the routes do not exist.

- [ ] **Step 3: Implement device authorization repository functions**

Add typed values and functions in `db.rs`:

```rust
#[derive(Debug, Serialize)]
pub struct DeviceAuthorization {
    pub device_id: String,
    pub name: String,
    pub enabled: bool,
    pub last_conversation_at: Option<String>,
}

pub async fn list_device_authorizations(pool: &SqlitePool) -> Result<Vec<DeviceAuthorization>>;
pub async fn create_device_authorization(pool: &SqlitePool, device_id: &str, name: &str, secret: &str) -> Result<()>;
pub async fn update_device_authorization(pool: &SqlitePool, device_id: &str, name: &str, enabled: bool) -> Result<bool>;
pub async fn reset_device_secret(pool: &SqlitePool, device_id: &str, secret: &str) -> Result<bool>;
```

Reuse `domain::device_auth::secret_hash`. Never update an existing row during create. Generate new 24-character secrets from an unambiguous alphabet and return plaintext only from successful create/reset handlers.

- [ ] **Step 4: Implement login middleware and handlers**

Create `src/web/admin.rs` for admin-only web concerns. Preserve `is_trusted_internal_source`: local requests attach `AdminPrincipal::Local`, public requests validate the opaque Session. Login accepts JSON `{username,password}`, returns the same `invalid_credentials` for every credential failure, and limits a source to five failures per ten minutes. Logout revokes the current Session.

Set the production login cookie as:

```rust
Cookie::build((ADMIN_COOKIE, token))
    .path("/")
    .http_only(true)
    .secure(true)
    .same_site(cookie::SameSite::Strict)
    .build()
```

Do not set `Max-Age` or `Expires`. Public state-changing management requests require same-host `Origin`. Device routes never pass through this middleware.

Store `secure_cookie: bool` in auth state: production always uses `true`; plain-HTTP integration tests use `false` only to round-trip the same cookie behavior locally.

- [ ] **Step 5: Implement device authorization handlers and password CLI**

Map duplicate device creation to `409 {"error":"device_already_exists"}`. Require JSON `{device_id,name}` for create and `{name,enabled}` for update. Reset accepts `{"confirm":true}` and rejects false/missing confirmation. The CLI reads a password from stdin and prints only the Argon2id hash:

```text
mjy-admin-password hash
```

- [ ] **Step 6: Run security and device compatibility tests**

Run:

```bash
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test public_admin_login_and_device_authorization_management -- --nocapture
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test configured_non_demo_device_can_auth_and_upgrade_through_public_proxy -- --nocapture
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test --all-targets
```

Expected: all pass; unchanged device auth and voice tests remain green.

- [ ] **Step 7: Commit**

```bash
git add src/web/admin.rs src/web/mod.rs src/db.rs src/bin/mjy-admin-password.rs tests/app_tests.rs
git commit -m "功能：新增登录与设备授权接口"
```

### Task 3: Login UI, Authorization Page, and Safe Release Gate

**Files:**
- Create: `static/admin-login.html`
- Create: `static/admin-auth.js`
- Create: `static/admin-authorizations.html`
- Create: `static/admin-authorizations.js`
- Modify: `static/admin.js`
- Modify: `static/styles.css`
- Modify: `static/admin*.html`
- Modify: `scripts/ui-acceptance.mjs`
- Modify: `scripts/deploy-jd.sh`
- Modify: `.env.example`
- Modify: `docs/接口接入说明.md`
- Modify: `docs/规划迭代记录.md`

- [ ] **Step 1: Write failing Playwright cases**

Cover unauthenticated redirect, login return URL, no browser storage, authorization list/create/enable/disable/reset, one-time secret removal from DOM, logout, and mobile overflow. Preserve the existing `noticeInsideShell` regression.

- [ ] **Step 2: Verify UI tests fail**

Run: `npm run ui:check`

Expected: login and authorization page cases fail because files are absent.

- [ ] **Step 3: Implement minimal login and shared auth controller**

`admin-auth.js` calls `/api/admin/auth/me`, redirects an unauthenticated `/admin.html` request to `/admin-login.html?next=%2Fadmin.html`, exposes `window.adminFetch`, and wires logout. The login page posts JSON credentials and clears the password after failure. It never uses Web Storage.

- [ ] **Step 4: Implement authorization management page**

Render device rows with name and enabled status. Provide create, edit/enable/disable, and confirmed reset. Show generated secret in a modal once; closing first empties its text node and then removes the modal. No delete action is rendered.

- [ ] **Step 5: Integrate navigation without losing the deployed layout fix**

Add “授权管理” and “退出登录” to admin pages. Before editing `static/admin.js` and `scripts/ui-acceptance.mjs`, port the original worktree's existing `noticeInsideShell` changes so the management grid regression remains fixed.

- [ ] **Step 6: Harden deployment**

Back up database, binary, static, and `.env` before install. Install `mjy-admin-password`. If `ADMIN_PASSWORD_HASH` is missing, generate a random password on the server, pipe it to the CLI, append only the hash to `.env`, and display the plaintext exactly once after health verification. Never modify existing `devices` rows.

- [ ] **Step 7: Run the complete release gate**

Run:

```bash
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo fmt --all -- --check
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo clippy --all-targets --all-features -- -D warnings
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test --all-targets
npm run ui:check
npm run voice:check
npm run sdk:check
bash -n scripts/deploy-jd.sh
git diff --check
```

Expected: every command exits zero, device auth/voice tests pass, and the existing UI layout regression remains green.

- [ ] **Step 8: Commit and stop before production**

```bash
git add static scripts/ui-acceptance.mjs scripts/deploy-jd.sh .env.example docs/接口接入说明.md docs/规划迭代记录.md
git commit -m "功能：交付单管理员授权后台"
```

Report test evidence, database compatibility, and rollback path design. Production deployment still requires explicit user approval.
