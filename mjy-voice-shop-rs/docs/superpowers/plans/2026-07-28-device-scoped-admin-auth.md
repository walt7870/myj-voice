# Device-Scoped Admin Authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a public account/password login to the administration UI while preserving trusted loopback access and enforcing device-level isolation for ordinary accounts' conversation history.

**Architecture:** Keep the current Axum application and SQLite database, add an authentication domain module plus a focused web module, and carry an explicit browser/device owner through every conversation write path. Authorization is decided server-side from a signed eight-hour cookie and a fresh account/device lookup on every protected request; trusted loopback traffic receives an in-memory super-admin principal without a cookie.

**Tech Stack:** Rust 2021, Axum 0.8, SQLx 0.8/SQLite, Argon2id, HMAC-SHA256, vanilla HTML/CSS/JavaScript, Playwright, Bash/systemd.

---

## Implementation constraints

- Work in `/Users/niu/Documents/公司/项目/美宜佳/新玩偶需求/mjy-voice-shop-rs`.
- Preserve the existing SQLite file and all rows. The migration is additive only.
- Do not stage the unrelated modified or untracked files already present in the parent worktree.
- Use `apply_patch` for hand-written edits.
- Keep the current strict loopback peer plus `X-Real-IP` check; never trust forwarded headers from a public peer.
- Never log a password, session cookie, generated password, or complete login request body.
- Run the focused red test before production code for every task, then the focused green test.
- Commit only the files listed in the task's commit command.

## Task 1: Add the schema migration and conversation ownership primitives

**Files:**

- Modify: `src/db.rs`
- Modify: `tests/app_tests.rs`

### Step 1: Write failing migration and ownership tests

Add tests named `migration_preserves_legacy_conversations_and_adds_device_owner` and `device_conversation_ownership_cannot_be_claimed_or_crossed` to `tests/app_tests.rs`. Seed a legacy row before a second `db::init`, then assert that its owner remains `NULL`. Exercise all four boundaries with this exact public API:

```rust
let browser = db::ConversationOwner::Browser;
let doll_a = db::ConversationOwner::Device("DOLL-A".to_string());
let doll_b = db::ConversationOwner::Device("DOLL-B".to_string());

db::ensure_conversation_owned(&pool, "legacy", &browser).await.unwrap();
assert!(db::ensure_conversation_owned(&pool, "legacy", &doll_a).await.is_err());

db::ensure_conversation_owned(&pool, "owned-a", &doll_a).await.unwrap();
db::ensure_conversation_owned(&pool, "owned-a", &doll_a).await.unwrap();
assert!(db::ensure_conversation_owned(&pool, "owned-a", &doll_b).await.is_err());
assert!(db::ensure_conversation_owned(&pool, "owned-a", &browser).await.is_err());
```

Also read `PRAGMA table_info(conversations)` and assert one `device_id` column exists after calling `db::init` twice.

### Step 2: Run the tests and confirm the API is absent

Run:

```bash
cargo test migration_preserves_legacy_conversations_and_adds_device_owner -- --nocapture
cargo test device_conversation_ownership_cannot_be_claimed_or_crossed -- --nocapture
```

Expected: compilation fails because `ConversationOwner` and `ensure_conversation_owned` do not exist.

### Step 3: Implement the additive migration

In `db::init`, check `PRAGMA table_info(conversations)` and execute the `ALTER` only when needed:

```rust
let has_device_id = sqlx::query("PRAGMA table_info(conversations)")
    .fetch_all(pool)
    .await?
    .iter()
    .any(|row| row.get::<String, _>("name") == "device_id");
if !has_device_id {
    sqlx::query("ALTER TABLE conversations ADD COLUMN device_id TEXT NULL")
        .execute(pool)
        .await?;
}
sqlx::query(
    "CREATE INDEX IF NOT EXISTS idx_conversations_device_created \
     ON conversations(device_id, created_at DESC)",
)
.execute(pool)
.await?;
sqlx::query(
    "CREATE INDEX IF NOT EXISTS idx_conversation_messages_conversation_created \
     ON conversation_messages(conversation_id, created_at DESC)",
)
.execute(pool)
.await?;
```

Add the owner type and a transaction-backed ownership check:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationOwner {
    Browser,
    Device(String),
}

pub async fn ensure_conversation_owned(
    pool: &SqlitePool,
    conversation_id: &str,
    owner: &ConversationOwner,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let existing = sqlx::query("SELECT device_id FROM conversations WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_optional(&mut *tx)
        .await?;
    match (existing, owner) {
        (None, ConversationOwner::Browser) => {
            sqlx::query("INSERT INTO conversations(conversation_id, created_at, device_id) VALUES(?, ?, NULL)")
                .bind(conversation_id)
                .bind(chrono::Utc::now().to_rfc3339())
                .execute(&mut *tx)
                .await?;
        }
        (None, ConversationOwner::Device(device_id)) => {
            sqlx::query("INSERT INTO conversations(conversation_id, created_at, device_id) VALUES(?, ?, ?)")
                .bind(conversation_id)
                .bind(chrono::Utc::now().to_rfc3339())
                .bind(device_id)
                .execute(&mut *tx)
                .await?;
        }
        (Some(row), ConversationOwner::Browser) if row.get::<Option<String>, _>("device_id").is_none() => {}
        (Some(row), ConversationOwner::Device(device_id))
            if row.get::<Option<String>, _>("device_id").as_deref() == Some(device_id.as_str()) => {}
        _ => anyhow::bail!("conversation owner mismatch"),
    }
    tx.commit().await?;
    Ok(())
}
```

Make `ensure_conversation` a compatibility wrapper around `ConversationOwner::Browser` until Task 2 removes ambiguous write calls.

### Step 4: Re-run focused tests

Run:

```bash
cargo test migration_preserves_legacy_conversations_and_adds_device_owner -- --nocapture
cargo test device_conversation_ownership_cannot_be_claimed_or_crossed -- --nocapture
```

Expected: both tests pass and the pre-migration row remains unowned.

### Step 5: Commit the ownership migration

```bash
git add src/db.rs tests/app_tests.rs
git commit -m "功能：固定设备对话归属"
```

## Task 2: Carry authenticated device ownership through every conversation write path

**Files:**

- Modify: `src/db.rs`
- Modify: `src/web/mod.rs`
- Modify: `tests/app_tests.rs`

### Step 1: Write failing end-to-end ownership tests

Add `browser_and_device_voice_create_distinct_conversation_owners` and `device_voice_cannot_reuse_another_owner_conversation_id` to `tests/app_tests.rs`.

The first test must:

1. Call `/api/chat/text` with `conversation_id = "browser-one"`.
2. Authenticate `DOLL-0001` and open `/api/device/voice?conversation_id=device-one` over real TCP.
3. Send a valid mocked voice turn and wait for `voice_done`.
4. Query SQLite directly and assert `browser-one.device_id IS NULL` and `device-one.device_id = 'DOLL-0001'`.

The second test must pre-create an unowned conversation and an owned conversation, then assert a device voice connection cannot append to either invalid owner. Verify message counts do not change.

### Step 2: Run the tests and confirm the device owner is not recorded

Run:

```bash
cargo test browser_and_device_voice_create_distinct_conversation_owners -- --nocapture
cargo test device_voice_cannot_reuse_another_owner_conversation_id -- --nocapture
```

Expected: the owner assertion or mismatch rejection fails because all current paths use the ownerless `ensure_conversation`.

### Step 3: Thread `ConversationOwner` through the web flow

Change the key signatures to make an omitted owner impossible. Insert `owner` in the positions shown, then retain the current bodies while forwarding `owner.clone()` at every asynchronous handoff:

```rust
async fn handle_ws(
    socket: WebSocket,
    state: AppState,
    owner: db::ConversationOwner,
    negotiated: VoiceAudioNegotiation,
);

struct RecognizedTurn {
    state: AppState,
    tx: mpsc::Sender<StreamEvent>,
    conversation_id: String,
    owner: db::ConversationOwner,
    text: String,
    trace: Option<Value>,
    audio_context: VoiceAudioContext,
}

async fn run_turn<F, Fut>(
    state: AppState,
    conversation_id: String,
    owner: db::ConversationOwner,
    text: String,
    trace: Option<Value>,
    audio_context: VoiceAudioContext,
    emit: F,
) -> Result<()>
where
    F: FnMut(StreamEvent) -> Fut,
    Fut: Future<Output = Result<(), ApiError>>,
;
```

Use `ConversationOwner::Browser` in `/api/conversations/new`, `/api/chat/text`, and `/api/chat/voice`. Use `ConversationOwner::Device(device_id)` only after `verify_device_token` succeeds in `/api/device/voice`.

At the beginning of `run_turn`, call `ensure_conversation_owned`; change `append_conversation_message` so it no longer creates a conversation implicitly. This guarantees the ownership check occurs before every message write.

### Step 4: Re-run focused and existing voice tests

Run:

```bash
cargo test browser_and_device_voice_create_distinct_conversation_owners -- --nocapture
cargo test device_voice_cannot_reuse_another_owner_conversation_id -- --nocapture
cargo test chat_and_authenticated_device_voice_upgrade_over_real_tcp -- --nocapture
```

Expected: all pass; browser conversations are unowned and authenticated device conversations are permanently owned.

### Step 5: Commit the propagation change

```bash
git add src/db.rs src/web/mod.rs tests/app_tests.rs
git commit -m "功能：贯通设备对话身份"
```

## Task 3: Add account storage, Argon2id passwords, signed sessions, and audit records

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/lib.rs`
- Create: `src/admin_auth.rs`

### Step 1: Add failing unit tests in the new module

Create `src/admin_auth.rs` with a `#[cfg(test)] mod tests` containing:

- `argon2_password_round_trip_and_wrong_password`
- `session_cookie_round_trip_rejects_tamper_and_expiry`
- `username_validation_accepts_only_safe_ascii`
- `account_schema_enforces_unique_username_and_device`
- `password_reset_increments_session_version`
- `account_mutation_writes_secret_free_audit_log`

The session test uses a fixed clock and this claim shape:

```rust
let claims = SessionClaims {
    account_id: "account-1".into(),
    role: AdminRole::DeviceViewer,
    expires_at: 1_722_182_400,
    session_version: 3,
};
let token = sign_session(&claims, b"32-byte-test-session-secret-value").unwrap();
assert_eq!(verify_session(&token, b"32-byte-test-session-secret-value", 1_722_182_399).unwrap(), claims);
assert!(verify_session(&token, b"wrong-32-byte-test-session-secret", 1_722_182_399).is_err());
assert!(verify_session(&token, b"32-byte-test-session-secret-value", 1_722_182_401).is_err());
```

### Step 2: Run the tests and confirm dependencies/types are missing

Run:

```bash
cargo test admin_auth::tests -- --nocapture
```

Expected: compilation fails until the module and cryptographic dependencies are implemented.

### Step 3: Add dependencies and domain types

Add:

```toml
argon2 = { version = "0.5", features = ["std"] }
cookie = "0.18"
```

Export `pub mod admin_auth;` from `src/lib.rs`. Define these stable types in `src/admin_auth.rs`:

```rust
pub const SESSION_COOKIE_NAME: &str = "mjy_admin_session";
pub const SESSION_TTL_SECONDS: i64 = 8 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRole { SuperAdmin, DeviceViewer }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminAccount {
    pub account_id: String,
    pub username: String,
    pub role: AdminRole,
    pub enabled: bool,
    pub session_version: i64,
    pub device_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClaims {
    pub account_id: String,
    pub role: AdminRole,
    pub expires_at: i64,
    pub session_version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationScope {
    All,
    Devices(Vec<String>),
}
```

Implement password hashing with `Argon2::default()` and a fresh `SaltString::generate(&mut OsRng)`; verify with `PasswordHash` and never return the hash in serialized account values.

Implement HMAC-SHA256 as `base64url(no-pad JSON claims).base64url(no-pad signature)`. Verify the signature before parsing/trusting claims and reject `expires_at <= now`.

### Step 4: Create the account tables and repository functions

Add `init_schema`, `create_account`, `find_account_for_login`, `load_active_session_account`, `list_accounts`, `set_account_enabled`, `replace_account_devices`, `reset_password`, and `account_exists`.

Use this schema verbatim:

```sql
CREATE TABLE IF NOT EXISTS admin_accounts (
    account_id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('super_admin', 'device_viewer')),
    enabled INTEGER NOT NULL DEFAULT 1,
    session_version INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS admin_account_devices (
    account_id TEXT NOT NULL,
    device_id TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    PRIMARY KEY(account_id, device_id),
    FOREIGN KEY(account_id) REFERENCES admin_accounts(account_id),
    FOREIGN KEY(device_id) REFERENCES devices(device_id)
);
CREATE TABLE IF NOT EXISTS admin_audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_account_id TEXT,
    action TEXT NOT NULL,
    target_account_id TEXT,
    detail TEXT NOT NULL,
    created_at TEXT NOT NULL
);
```

Every mutating repository function uses one transaction for data plus audit. `reset_password` and enable/disable changes increment `session_version`; device replacement does not need to increment because device bindings are reloaded per request.

Validate usernames using ASCII characters only, length `3..=64`, with each byte matching `[A-Za-z0-9._-]`. Map the unique device constraint to a typed `AdminAuthError::DeviceAlreadyBound`.

### Step 5: Run tests and inspect dependency policy

Run:

```bash
cargo test admin_auth::tests -- --nocapture
cargo tree -i argon2
cargo tree -i cookie
```

Expected: all six tests pass; each new dependency has one resolved version and no unexpected duplicate direct version.

### Step 6: Commit the auth domain

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/admin_auth.rs
git commit -m "功能：新增后台账号与安全会话"
```

## Task 4: Replace public management denial with authenticated route middleware

**Files:**

- Create: `src/web/admin_auth.rs`
- Modify: `src/web/mod.rs`
- Modify: `src/main.rs`
- Modify: `tests/app_tests.rs`

### Step 1: Add failing authentication boundary tests

Add these integration tests:

- `public_admin_api_requires_login_but_local_tcp_bypasses_auth`
- `login_sets_secure_strict_http_only_cookie_and_me_returns_identity`
- `login_errors_are_uniform_and_rate_limited_after_five_failures`
- `tampered_expired_disabled_and_reset_sessions_are_rejected`
- `logout_clears_cookie`
- `public_state_change_rejects_cross_origin_request`
- `public_spoofed_forwarding_headers_never_gain_local_bypass`

Build a public request by applying `ConnectInfo(203.0.113.10:4567)` and `x-real-ip: 203.0.113.10`. Seed accounts through repository functions, not direct password hashes.

Assert exact response codes/error keys:

```rust
assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
assert_eq!(body["error"], "login_required");
assert_eq!(viewer_forbidden.status(), StatusCode::FORBIDDEN);
assert_eq!(rate_limited.status(), StatusCode::TOO_MANY_REQUESTS);
assert_eq!(body["error"], "login_rate_limited");
```

### Step 2: Run focused tests and confirm current public behavior is wrong

Run:

```bash
cargo test public_admin_api_requires_login_but_local_tcp_bypasses_auth -- --nocapture
cargo test login_sets_secure_strict_http_only_cookie_and_me_returns_identity -- --nocapture
```

Expected: public management still returns `403`, and login routes do not exist.

### Step 3: Add authentication state and startup validation

Create state that is cheap to clone and supports deterministic tests:

```rust
#[derive(Clone)]
pub struct AdminAuthState {
    pub session_secret: Arc<Vec<u8>>,
    pub login_limiter: Arc<tokio::sync::Mutex<LoginAttemptStore>>,
    pub secure_cookie: bool,
}
```

Add `admin_auth: AdminAuthState` to `AppState` and all test constructors.

In `main.rs`, call `admin_auth::init_schema(&pool)` immediately after `db::init(&pool)`, then read `ADMIN_SESSION_SECRET`. Require at least 32 bytes when `HOST` is non-loopback. For loopback development only, permit a clearly logged development fallback secret. Production must exit before binding if the variable is missing or short. `scripts/deploy-jd.sh` will generate the production value in Task 9. Test setup calls `init_schema` explicitly and uses a fixed 32-byte secret.

### Step 4: Implement login, logout, current identity, limiter, and middleware

Create these routes before the protected admin router:

```rust
.route("/api/admin/auth/login", post(admin_auth::login))
.route("/api/admin/auth/logout", post(admin_auth::logout))
.route("/api/admin/auth/me", get(admin_auth::me))
```

Use a private `HashMap<(IpAddr, String), VecDeque<Instant>>` guarded by the state mutex. Before counting, remove entries older than ten minutes. A sixth failed attempt in the active window returns `429`; a successful login removes the key. Store lowercased usernames in limiter keys, while account lookup follows the database's exact unique username.

Set the success cookie with:

```rust
Cookie::build((SESSION_COOKIE_NAME, token))
    .path("/")
    .http_only(true)
    .secure(state.admin_auth.secure_cookie)
    .same_site(cookie::SameSite::Strict)
    .max_age(cookie::time::Duration::hours(8))
    .build()
```

Production sets `secure_cookie = true`; focused plain-HTTP tests set it to `false` only so the test client can round-trip the cookie.

Replace `require_internal_access` with `require_admin_access(State(state), request, next)`. The middleware order is:

1. Ignore non-management paths.
2. If `is_trusted_internal_source` passes, attach `AdminPrincipal::LocalSuperAdmin` and continue.
3. Allow the login endpoint without an existing session.
4. Parse and verify the cookie.
5. Load the account from SQLite and compare `enabled`, role, and `session_version` with the signed claims.
6. Attach `AdminPrincipal::Account(account)` and enforce route permissions.

For public non-GET/HEAD requests, parse `Origin` and require its authority to equal the request `Host` before invoking a state-changing handler. When the immediate peer is the trusted loopback reverse proxy, derive the expected scheme from its single `X-Forwarded-Proto` value; otherwise use the request URI scheme. Reject missing, malformed, duplicated, or mismatched origin/proto headers. Login is included in this check. Direct trusted local traffic is exempt.

Return JSON `401` for unauthenticated APIs. HTML redirects are implemented in Task 7 after static page classification is added.

### Step 5: Run the full authentication boundary set

Run:

```bash
cargo test public_admin_api_requires_login_but_local_tcp_bypasses_auth -- --nocapture
cargo test login_sets_secure_strict_http_only_cookie_and_me_returns_identity -- --nocapture
cargo test login_errors_are_uniform_and_rate_limited_after_five_failures -- --nocapture
cargo test tampered_expired_disabled_and_reset_sessions_are_rejected -- --nocapture
cargo test logout_clears_cookie -- --nocapture
cargo test public_state_change_rejects_cross_origin_request -- --nocapture
cargo test public_spoofed_forwarding_headers_never_gain_local_bypass -- --nocapture
```

Expected: all pass; existing trusted loopback tests continue to pass.

### Step 6: Commit the web authentication boundary

```bash
git add src/web/admin_auth.rs src/web/mod.rs src/main.rs tests/app_tests.rs
git commit -m "功能：开放受认证的后台路由"
```

## Task 5: Enforce role and device scope in every conversation read query

**Files:**

- Modify: `src/db.rs`
- Modify: `src/web/mod.rs`
- Modify: `src/web/admin_auth.rs`
- Modify: `tests/app_tests.rs`

### Step 1: Write failing isolation tests

Add:

- `super_admin_sees_all_owned_and_legacy_conversations`
- `device_viewers_only_list_their_bound_device_conversations`
- `device_viewer_out_of_scope_detail_returns_not_found`
- `device_rebinding_changes_visible_conversations_without_relogin`
- `device_viewer_cannot_access_non_conversation_admin_routes`

Seed `legacy` with `NULL`, `doll-a` with `DOLL-A`, and `doll-b` with `DOLL-B`. Give viewer A only `DOLL-A`, viewer B only `DOLL-B`, and assert list totals as well as message/event/order detail contents. The out-of-scope detail response must be `404`, never `403`.

### Step 2: Run isolation tests and observe unfiltered reads

Run:

```bash
cargo test super_admin_sees_all_owned_and_legacy_conversations -- --nocapture
cargo test device_viewers_only_list_their_bound_device_conversations -- --nocapture
cargo test device_viewer_out_of_scope_detail_returns_not_found -- --nocapture
```

Expected: viewer tests fail because `list_conversations_page` and detail helpers currently have no scope parameter.

### Step 3: Add scope-aware SQL at the repository boundary

Replace unscoped administrative reads with signatures that require scope:

```rust
pub async fn list_conversations_page_scoped(
    pool: &SqlitePool,
    page: i64,
    page_size: i64,
    scope: &ConversationScope,
) -> Result<ConversationPage>;

pub async fn conversation_visible(
    pool: &SqlitePool,
    conversation_id: &str,
    scope: &ConversationScope,
) -> Result<bool>;
```

For `ConversationScope::Devices`, use `sqlx::QueryBuilder<Sqlite>` and `push_separated` to build an `IN` predicate with one bound slot for every device ID. An empty device list returns an empty page without issuing `IN ()`. Use the identical predicate for the count and page query.

The detail handler must call `conversation_visible` first. Only after it returns true may it call the existing message, event, and order helpers. If false, return `ApiError::not_found()`.

Convert the request principal to scope exactly as follows:

```rust
fn conversation_scope(principal: &AdminPrincipal) -> ConversationScope {
    match principal {
        AdminPrincipal::LocalSuperAdmin => ConversationScope::All,
        AdminPrincipal::Account(account) if account.role == AdminRole::SuperAdmin => ConversationScope::All,
        AdminPrincipal::Account(account) => ConversationScope::Devices(account.device_ids.clone()),
    }
}
```

The middleware allows `device_viewer` only for:

- `GET /api/admin/auth/me`
- `POST /api/admin/auth/logout`
- `GET /api/admin/conversations`
- `GET /api/admin/conversations/{id}`

All other management APIs return `403 forbidden`.

### Step 4: Run all isolation tests

Run:

```bash
cargo test super_admin_sees_all_owned_and_legacy_conversations -- --nocapture
cargo test device_viewers_only_list_their_bound_device_conversations -- --nocapture
cargo test device_viewer_out_of_scope_detail_returns_not_found -- --nocapture
cargo test device_rebinding_changes_visible_conversations_without_relogin -- --nocapture
cargo test device_viewer_cannot_access_non_conversation_admin_routes -- --nocapture
```

Expected: all pass, including immediate permission changes after device rebinding.

### Step 5: Commit scope enforcement

```bash
git add src/db.rs src/web/mod.rs src/web/admin_auth.rs tests/app_tests.rs
git commit -m "安全：隔离授权账号对话范围"
```

## Task 6: Add super-admin authorization APIs and maintenance CLI

**Files:**

- Create: `src/bin/mjy-admin-account.rs`
- Modify: `src/admin_auth.rs`
- Modify: `src/web/admin_auth.rs`
- Modify: `src/web/mod.rs`
- Modify: `tests/app_tests.rs`

### Step 1: Write failing API and CLI-domain tests

Add these integration tests:

- `super_admin_can_create_list_disable_enable_and_reset_viewer`
- `authorization_create_generates_password_once_without_persisting_plaintext`
- `authorization_rejects_duplicate_device_binding_with_conflict`
- `viewer_cannot_call_authorization_management_api`
- `maintenance_upsert_bootstraps_myjadmin_without_overwriting_existing_account`

Assert the create/reset response contains `generated_password` of 24 characters, the list response never contains `password_hash` or `generated_password`, and querying all three auth tables finds no generated plaintext.

### Step 2: Run focused tests and confirm routes are absent

Run:

```bash
cargo test super_admin_can_create_list_disable_enable_and_reset_viewer -- --nocapture
cargo test authorization_rejects_duplicate_device_binding_with_conflict -- --nocapture
```

Expected: `404` because authorization routes do not exist.

### Step 3: Implement exact authorization endpoints

Register:

```rust
.route("/api/admin/authorizations", get(admin_auth::list_authorizations).post(admin_auth::create_authorization))
.route("/api/admin/authorizations/{account_id}", put(admin_auth::update_authorization))
.route("/api/admin/authorizations/{account_id}/reset-password", post(admin_auth::reset_authorization_password))
```

Use request/response contracts:

```json
{"username":"store-001","device_ids":["DOLL-0001"]}
```

```json
{"enabled":false,"device_ids":["DOLL-0001"]}
```

```json
{"account":{"account_id":"6d6106db-0af4-4e2f-96cf-e778e9f41372","username":"store-001","role":"device_viewer","enabled":true,"device_ids":["DOLL-0001"]},"generated_password":"N4y8Qm2Vw7Ks5Zp9Xc3Ht6Br"}
```

Generate 24 characters from an unambiguous alphabet using the cryptographically seeded `rand::rng()` thread generator; hash before starting the storage transaction and expose plaintext only in the successful create/reset response value. Do not include it in tracing fields or error context.

Reject attempts to create another super admin through the HTTP endpoint. Reject disabling, resetting, or changing device bindings for the current super-admin account. Map an existing device binding to `409 {"error":"device_already_bound"}`.

### Step 4: Implement the maintenance binary

The binary accepts only non-secret arguments:

```text
mjy-admin-account ensure-super --username myjadmin
```

It reads one password line from standard input, rejects empty input, connects with `DATABASE_URL`, runs `db::init` and `admin_auth::init_schema`, then:

- creates `myjadmin` as enabled `super_admin` if absent and prints `created`;
- prints `exists` without changing its hash or `session_version` when already present.

The password is never accepted as an argument. Do not echo stdin, and drop the owned plaintext immediately after hashing.

### Step 5: Run focused tests and compile both binaries

Run:

```bash
cargo test super_admin_can_create_list_disable_enable_and_reset_viewer -- --nocapture
cargo test authorization_create_generates_password_once_without_persisting_plaintext -- --nocapture
cargo test authorization_rejects_duplicate_device_binding_with_conflict -- --nocapture
cargo test viewer_cannot_call_authorization_management_api -- --nocapture
cargo test maintenance_upsert_bootstraps_myjadmin_without_overwriting_existing_account -- --nocapture
cargo build --bins
```

Expected: all tests pass and both `mjy-voice-shop-rs` and `mjy-admin-account` compile.

### Step 6: Commit account administration

```bash
git add src/bin/mjy-admin-account.rs src/admin_auth.rs src/web/admin_auth.rs src/web/mod.rs tests/app_tests.rs
git commit -m "功能：新增授权账号管理"
```

## Task 7: Add login/session handling and role-aware admin navigation

**Files:**

- Create: `static/admin-login.html`
- Create: `static/admin-auth.js`
- Modify: `static/styles.css`
- Modify: `static/admin.js`
- Modify: `static/admin.html`
- Modify: `static/admin-capability.html`
- Modify: `static/admin-conversations.html`
- Modify: `static/admin-devices.html`
- Modify: `static/admin-miniprogram-c.html`
- Modify: `static/admin-order-mcp.html`
- Modify: `static/admin-orders.html`
- Modify: `static/admin-products.html`
- Modify: `static/admin-prompts.html`
- Modify: `static/admin-voice.html`
- Modify: `scripts/ui-acceptance.mjs`
- Modify: `tests/app_tests.rs`

### Step 1: Extend UI acceptance with failing login and viewer cases

Add Playwright cases that serve controlled auth responses and verify:

- a public request for `/admin.html` without a session reaches `/admin-login.html?next=%2Fadmin.html`;
- the login form posts JSON, never writes password to `localStorage` or `sessionStorage`, and returns to `next`;
- a viewer opening any admin page is redirected to `/admin-conversations.html`;
- a viewer sees only “历史对话” and “退出登录” navigation actions;
- `401` from a conversation API redirects to login with the current URL as `next`;
- desktop and mobile screenshots have no horizontal overflow.

Add Rust integration assertions for HTML behavior because middleware—not JavaScript—owns the security redirect.

### Step 2: Run UI and HTML boundary tests

Run:

```bash
npm run ui:check
cargo test unauthenticated_admin_html_redirects_to_login -- --nocapture
cargo test viewer_admin_html_redirects_to_conversations -- --nocapture
```

Expected: login page and HTML redirects are missing.

### Step 3: Implement middleware HTML classification

Treat `/admin.html` and `/admin-*.html` as protected admin HTML, except `/admin-login.html`. For unauthenticated public HTML requests, return `303 See Other` with:

```text
/admin-login.html?next=%2Fadmin-conversations.html%3Fpage%3D2
```

Only accept a `next` value beginning with `/admin` and containing no scheme or authority. For a viewer requesting a protected page other than conversations, return `303 /admin-conversations.html` before serving bytes.

### Step 4: Build the shared browser auth controller

`static/admin-auth.js` exports `adminSessionReady` and performs one `/api/admin/auth/me` request. It must:

- redirect `401` to login with safe `next`;
- redirect a viewer away from disallowed pages;
- mark the document with `data-admin-role`;
- hide `[data-super-admin-only]` for viewers;
- wire `[data-admin-logout]` to `POST /api/admin/auth/logout` with JSON headers and same-origin credentials;
- expose `window.adminFetch` so `static/admin.js` uses one handler for `401`, `403`, and JSON errors.

Load it with `type="module"` before page-specific behavior on every admin page. Add an “授权管理” menu item marked `data-super-admin-only` and an “退出登录” button to the shared repeated menu markup.

Do not modify or remove the internal-access notice fix already present in the dirty `static/admin.js`; integrate auth around it.

### Step 5: Implement the independent login page

Build a compact responsive form with username, password, submit state, uniform `账号或密码错误` message, and explicit rate-limit message. Submit:

```javascript
await fetch('/api/admin/auth/login', {
  method: 'POST',
  credentials: 'same-origin',
  headers: {'content-type': 'application/json'},
  body: JSON.stringify({username, password}),
});
```

Clear the password field after every failed attempt. Do not persist either input. If `/api/admin/auth/me` already succeeds, immediately route to the safe `next` URL or the role's default page.

### Step 6: Run browser and server tests

Run:

```bash
node --check static/admin-auth.js
node --check static/admin.js
npm run ui:check
cargo test unauthenticated_admin_html_redirects_to_login -- --nocapture
cargo test viewer_admin_html_redirects_to_conversations -- --nocapture
```

Expected: all pass at desktop, tablet, and mobile widths.

### Step 7: Commit login and navigation

```bash
git add static/admin-login.html static/admin-auth.js static/styles.css static/admin.js static/admin.html static/admin-capability.html static/admin-conversations.html static/admin-devices.html static/admin-miniprogram-c.html static/admin-order-mcp.html static/admin-orders.html static/admin-products.html static/admin-prompts.html static/admin-voice.html scripts/ui-acceptance.mjs tests/app_tests.rs
git commit -m "功能：新增后台登录与角色导航"
```

## Task 8: Add the super-admin authorization management page

**Files:**

- Create: `static/admin-authorizations.html`
- Create: `static/admin-authorizations.js`
- Modify: `static/styles.css`
- Modify: `scripts/ui-acceptance.mjs`

### Step 1: Add failing Playwright authorization workflows

Mock the authorization API and cover:

- list rows with username, enabled state, and assigned devices;
- create account with one or multiple available devices;
- one-time password dialog after create;
- reset password confirmation and one-time password dialog;
- disable/enable account;
- replace device bindings;
- `409 device_already_bound` inline error;
- no plaintext password remains in DOM after closing the dialog;
- viewer never sees or can remain on this page;
- no desktop/mobile overflow.

### Step 2: Run the UI test and confirm the page is absent

Run:

```bash
npm run ui:check
```

Expected: authorization page workflow fails because the page and script do not exist.

### Step 3: Implement accessible authorization management

Create a page using the existing admin shell and styles. Fetch account/device data after `adminSessionReady`. Use native form controls with labels, a multi-select/checklist for devices, and explicit confirmation before reset or disable.

Display generated passwords only in a modal region with:

- a warning that it is shown once;
- a copy button using `navigator.clipboard.writeText`;
- a close button that first replaces the text node with an empty string, then removes the modal;
- no console logging or browser storage.

Use `window.adminFetch` for every request and reload the list after a successful mutation. Disable submit controls while a request is pending to prevent duplicate account creation.

### Step 4: Run static and UI checks

Run:

```bash
node --check static/admin-authorizations.js
npm run ui:check
```

Expected: all authorization workflows and responsive checks pass.

### Step 5: Commit the page

```bash
git add static/admin-authorizations.html static/admin-authorizations.js static/styles.css scripts/ui-acceptance.mjs
git commit -m "功能：新增授权管理页面"
```

## Task 9: Harden deployment, bootstrap `myjadmin`, and preserve rollback data

**Files:**

- Modify: `scripts/deploy-jd.sh`
- Modify: `.env.example`
- Modify: `docs/接口接入说明.md`
- Modify: `docs/规划迭代记录.md`

### Step 1: Add a local deployment contract check

Add a `--check` mode to `scripts/deploy-jd.sh` that performs no SSH or file mutation and asserts the script contains these guarantees:

- source archive excludes `.env` by default;
- a dated rollback directory receives database, binary, static, and `.env` copies before install;
- `ADMIN_SESSION_SECRET` is generated only if missing;
- both release binaries are installed;
- `myjadmin` is bootstrapped only when absent;
- initial password is printed once after successful health verification;
- rollback path is printed.

Document and test the check with:

```bash
scripts/deploy-jd.sh --check
```

Expected before implementation: exits non-zero because the current script lacks the contract.

### Step 2: Change packaging and backup behavior

Default `COPY_ENV=0`. Before stopping the service, create:

```bash
backup_dir="/opt/mjy-voice-shop-rs-backups/${timestamp}"
```

Copy these if present, preserving modes:

- `${APP_DIR}/mjy-voice-shop-rs`
- `${APP_DIR}/mjy-admin-account`
- `${APP_DIR}/static`
- `${APP_DIR}/mjy_voice_shop.db`
- `${APP_DIR}/.env`

Never remove or replace `${APP_DIR}/mjy_voice_shop.db` during install. Install the CLI from `target/release/mjy-admin-account` alongside the service binary.

### Step 3: Generate secrets without leaking them

If `.env` lacks `ADMIN_SESSION_SECRET`, append a 48-byte base64 secret generated on the server with file mode `0600`. Do not print it.

Generate the initial admin password with `openssl rand -base64 24`, pass it to the CLI over stdin, and capture only the CLI status word:

```bash
bootstrap_result="$(printf '%s\n' "$initial_admin_password" | \
  DATABASE_URL="sqlite://${APP_DIR}/mjy_voice_shop.db" \
  "${APP_DIR}/mjy-admin-account" ensure-super --username myjadmin)"
```

If the result is `exists`, immediately unset the generated value and print no password. If it is `created`, retain it only until the service health check passes, then print username and password once and unset it.

### Step 4: Update operating documentation

In `.env.example`, document `ADMIN_SESSION_SECRET` as required for a public bind without supplying a real value.

In `docs/接口接入说明.md`, add login URL, session duration, viewer scope behavior, loopback bypass, and the maintenance command's stdin rule. In `docs/规划迭代记录.md`, record the additive database migration and rollback behavior. Preserve unrelated user edits already present in both documents and stage only the intended hunks.

### Step 5: Run deployment contract and shell checks

Run:

```bash
bash -n scripts/deploy-jd.sh
scripts/deploy-jd.sh --check
rg -n "ADMIN_SESSION_SECRET|mjy-admin-account|mjy_voice_shop.db|backup_dir" scripts/deploy-jd.sh .env.example docs/接口接入说明.md docs/规划迭代记录.md
```

Expected: shell syntax and contract check pass; no concrete production secret or generated password appears in tracked files.

### Step 6: Commit release hardening

```bash
git add scripts/deploy-jd.sh .env.example
git add -p docs/接口接入说明.md docs/规划迭代记录.md
git diff --cached --check
git commit -m "发布：安全初始化后台认证"
```

## Task 10: Complete regression, security review, and production release gate

**Files:**

- Modify only if a verification failure requires a scoped fix.

### Step 1: Run formatting and static validation

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
node --check static/admin.js
node --check static/admin-auth.js
node --check static/admin-authorizations.js
bash -n scripts/deploy-jd.sh
```

Expected: all commands exit zero.

### Step 2: Run the complete automated suite

Run:

```bash
cargo test --all-targets
npm run audio:check
npm run time:check
npm run ui:check
npm run voice:check
npm run sdk:check
npm run miniprogram:check
scripts/deploy-jd.sh --check
```

Expected: all commands exit zero. The existing layout regression `noticeInsideShell` must remain green.

### Step 3: Perform a targeted secret and authorization review

Run:

```bash
rg -n "password|generated_password|SESSION_COOKIE_NAME|ADMIN_SESSION_SECRET" src static scripts tests
rg -n "list_conversation|conversation_visible|list_conversation_messages|list_conversation_events|list_mock_order_payloads_by_conversation" src
git diff --check
git status --short
```

Manually confirm:

- no password or cookie is logged;
- every public conversation read receives a server-derived scope;
- detail visibility is checked before messages, events, or orders are loaded;
- public spoofed headers cannot activate loopback bypass;
- the dirty parent-project files are not staged.

### Step 4: Create a release candidate commit only if verification fixes were needed

Stage only the scoped fix files, inspect `git diff --cached`, and commit:

```bash
git commit -m "修复：完成后台认证发布验收"
```

If no files changed, do not create an empty commit.

### Step 5: Request explicit production release approval

Report the complete command evidence, current commit, database migration behavior, and rollback strategy. Do not run the production deploy until the user explicitly approves this release candidate.

### Step 6: Deploy after explicit approval and verify externally

Use the `safe-deploy-release` skill for the production action. Run the repository deploy script, record the printed rollback path, and retain the one-time `myjadmin` password for direct delivery to the user only.

Verify on the server and through the public hostname:

```bash
systemctl is-active mjy-voice-shop-rs.service
curl -fsS http://127.0.0.1:8787/api/health
```

Then verify:

1. Public unauthenticated admin HTML redirects to login.
2. Public unauthenticated admin API returns `401 login_required`.
3. `myjadmin` logs in, sees legacy plus all device conversations, and can manage accounts.
4. Two temporary viewer accounts bound to different devices see only their own lists/details.
5. Cross-device detail returns `404`.
6. Disabling a viewer invalidates its existing cookie immediately.
7. Public experience page, device config/auth, and voice connection still work.
8. Production row counts for conversations, messages, events, orders, products, and devices are not lower than the pre-release snapshot.

Delete or disable only the temporary acceptance accounts created by this step through the authorization API; retain their audit entries. Do not alter production conversations.

### Step 7: Deliver release evidence

Report:

- deployed commit;
- service and health status;
- pre/post database counts;
- role/isolation acceptance results;
- rollback directory;
- `myjadmin` initial password only if the account was newly created.

Never repeat the initial password in subsequent logs or documents.
