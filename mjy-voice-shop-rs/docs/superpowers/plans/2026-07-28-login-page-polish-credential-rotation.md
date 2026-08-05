# Login Page Polish and Credential Rotation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a centered, polished administrator login page, change the only administrator username to `myjadmin`, and prepare a safe 24-character slash-free password rotation path.

**Architecture:** Keep the existing static HTML/CSS login flow and Rust session architecture. Centralize the fixed administrator username and random password generation in `admin_auth`, let the password CLI expose generation to the deployment script, and gate production rotation behind an explicit deployment flag. No device authorization, conversation, order, or voice protocol changes are included.

**Tech Stack:** Rust 2021, Axum, Argon2id, static HTML/CSS/JavaScript, Playwright UI acceptance, Bash deployment script.

---

### Task 1: Fix the Single Administrator Identity and Safe Password Generator

**Files:**
- Modify: `src/admin_auth.rs`
- Modify: `src/main.rs`
- Modify: `src/bin/mjy-admin-password.rs`
- Modify: `tests/app_tests.rs`
- Modify: `src/web/admin.rs`
- Modify: `src/web/mod.rs`
- Modify: `.env.example`

- [ ] **Step 1: Write failing identity and password-format tests**

Add these assertions to the existing `src/admin_auth.rs` test module:

```rust
#[test]
fn only_myjadmin_is_accepted_as_the_single_admin_username() {
    let hash = hash_password("correct-pass").unwrap();
    assert!(AdminConfig::new("myjadmin", hash.clone()).is_ok());
    assert!(AdminConfig::new("admin", hash).is_err());
}

#[test]
fn generated_admin_password_is_24_character_lowercase_hex() {
    for _ in 0..100 {
        let password = generate_admin_password();
        assert_eq!(password.len(), 24);
        assert!(password.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        assert!(!password.contains('/'));
    }
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test only_myjadmin_is_accepted_as_the_single_admin_username
cargo test generated_admin_password_is_24_character_lowercase_hex
```

Expected: the username test fails because only `admin` is accepted; the generator test fails to compile because `generate_admin_password` does not exist.

- [ ] **Step 3: Implement the fixed username and generator**

Add to `src/admin_auth.rs`:

```rust
pub const ADMIN_USERNAME: &str = "myjadmin";

pub fn generate_admin_password() -> String {
    let mut bytes = [0_u8; 12];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
```

Change the `AdminConfig::new` guard to:

```rust
if username != ADMIN_USERNAME {
    bail!("ADMIN_USERNAME must be {ADMIN_USERNAME}");
}
```

Change `src/main.rs` to use the same constant as its default:

```rust
let admin_username = std::env::var("ADMIN_USERNAME")
    .unwrap_or_else(|_| mjy_voice_shop_rs::admin_auth::ADMIN_USERNAME.to_string());
```

Update `src/bin/mjy-admin-password.rs` so `hash` keeps reading stdin and `generate` prints one generated password:

```rust
match std::env::args().nth(1).as_deref() {
    Some("hash") => {
        let mut password = String::new();
        io::stdin().read_to_string(&mut password)?;
        let password = password.trim_end_matches(['\r', '\n']);
        println!("{}", hash_password(password)?);
    }
    Some("generate") => println!("{}", generate_admin_password()),
    _ => bail!("usage: mjy-admin-password <hash|generate>"),
}
```

Change all test `AdminConfig::new("admin", ...)` fixtures and successful login JSON payloads to `myjadmin`, while leaving one negative login assertion for `admin`. Update `.env.example` to `ADMIN_USERNAME=myjadmin`.

- [ ] **Step 4: Verify GREEN and CLI output**

Run:

```bash
cargo test admin_auth::tests
cargo test public_admin_login_and_device_authorization_management
password="$(cargo run --quiet --bin mjy-admin-password -- generate)"
test "${#password}" -eq 24
test -z "${password//[0-9a-f]/}"
```

Expected: all tests pass and the generated password contains exactly 24 lowercase hexadecimal characters.

- [ ] **Step 5: Commit the credential code**

```bash
git add src/admin_auth.rs src/main.rs src/bin/mjy-admin-password.rs src/web/admin.rs src/web/mod.rs tests/app_tests.rs .env.example
git commit -m "功能：更新单管理员凭据规则"
```

### Task 2: Build the Centered Refined Login Page

**Files:**
- Modify: `static/admin-login.html`
- Modify: `static/styles.css`
- Modify: `scripts/ui-acceptance.mjs`

- [ ] **Step 1: Add failing UI contract checks**

Read `static/admin-login.html` in `scripts/ui-acceptance.mjs` and fail if it does not contain the new username and structure:

```javascript
const loginSource = await fs.readFile(path.join(ROOT, "static/admin-login.html"), "utf8");
if (!loginSource.includes('value="myjadmin"')
  || !loginSource.includes('class="auth-intro"')
  || !loginSource.includes('class="auth-security-note"')) {
  addFailure("admin-login-contract", "static", "登录页用户名或居中卡片结构未更新");
}
```

Add `authCard: rectOf(".auth-card")` and `loginUsername` to the per-page audit, then assert on the login page:

```javascript
if (target.id === "admin-login") {
  const centerX = audit.authCard.left + audit.authCard.width / 2;
  const centerY = audit.authCard.top + audit.authCard.height / 2;
  if (Math.abs(centerX - viewport.width / 2) > 3
    || Math.abs(centerY - viewport.height / 2) > 40
    || audit.loginUsername !== "myjadmin") {
    addFailure(target.id, viewport.id, "登录卡片未居中或默认用户名错误");
  }
}
```

- [ ] **Step 2: Run UI acceptance and verify RED**

Run against the existing local or temporary verification server:

```bash
npm run ui:check
```

Expected: `admin-login-contract` fails because the new structure and username are absent.

- [ ] **Step 3: Implement restrained markup and styling**

Replace only the contents of `<body>` before the existing script in `static/admin-login.html` with this structure, preserving the existing JavaScript behavior:

```html
<body class="admin-page auth-page">
  <div class="auth-atmosphere" aria-hidden="true"><i></i><i></i></div>
  <main class="auth-card">
    <header class="auth-intro">
      <div class="brand-mark">管</div>
      <div><span class="auth-kicker">管理中心</span><h1>欢迎回来</h1></div>
    </header>
    <p class="auth-description">登录后管理设备授权与沟通记录</p>
    <form id="loginForm">
      <label for="username"><span>管理员账号</span><input id="username" name="username" value="myjadmin" autocomplete="username" required /></label>
      <label for="password"><span>登录密码</span><input id="password" name="password" type="password" autocomplete="current-password" placeholder="请输入密码" required /></label>
      <button id="loginButton" type="submit"><span>进入管理后台</span></button>
      <p id="loginMessage" class="form-message" role="status"></p>
    </form>
    <p class="auth-security-note"><span aria-hidden="true">●</span> 请仅在受信任的设备上登录</p>
  </main>
```

Replace the compact `.auth-page` block in `static/styles.css` with these scoped rules. Keep the existing authorization-management rules below this block separate:

```css
.auth-page {
  min-height: 100svh;
  display: grid;
  place-items: center;
  position: relative;
  overflow: hidden;
  padding: 32px 20px;
  background:
    linear-gradient(rgba(77, 99, 145, .045) 1px, transparent 1px),
    linear-gradient(90deg, rgba(77, 99, 145, .045) 1px, transparent 1px),
    #f3f6fb;
  background-size: 32px 32px, 32px 32px, auto;
}

.auth-atmosphere { position: absolute; inset: 0; pointer-events: none; }
.auth-atmosphere i { position: absolute; width: 420px; aspect-ratio: 1; border-radius: 50%; filter: blur(2px); opacity: .55; }
.auth-atmosphere i:first-child { top: -210px; left: -150px; background: radial-gradient(circle, rgba(52, 92, 255, .2), transparent 68%); }
.auth-atmosphere i:last-child { right: -180px; bottom: -230px; background: radial-gradient(circle, rgba(39, 178, 165, .16), transparent 68%); }

.auth-card {
  position: relative;
  z-index: 1;
  width: min(440px, 100%);
  padding: 38px;
  border: 1px solid rgba(210, 219, 235, .92);
  border-radius: 22px;
  background: rgba(255, 255, 255, .96);
  box-shadow: 0 28px 70px rgba(35, 49, 78, .14), 0 4px 14px rgba(35, 49, 78, .06);
  backdrop-filter: blur(16px);
}

.auth-intro { display: flex; align-items: center; gap: 15px; }
.auth-intro .brand-mark { flex: 0 0 auto; box-shadow: 0 10px 26px rgba(52, 92, 255, .24); }
.auth-kicker { display: block; margin-bottom: 3px; color: #71809c; font-size: 12px; font-weight: 800; letter-spacing: .18em; }
.auth-card h1 { margin: 0; color: #192134; font-size: 28px; line-height: 1.2; letter-spacing: -.02em; }
.auth-description { margin: 18px 0 28px; color: #778198; font-size: 14px; line-height: 1.7; }
.auth-card form { display: grid; gap: 17px; }
.auth-card label { display: grid; gap: 8px; color: #343e54; font-size: 14px; font-weight: 800; }
.auth-card input {
  width: 100%;
  min-width: 0;
  height: 48px;
  border: 1px solid #d8dfec;
  border-radius: 11px;
  padding: 0 14px;
  background: #fbfcfe;
  color: #20283a;
  font: inherit;
  transition: border-color .18s ease, box-shadow .18s ease, background .18s ease;
}
.auth-card input:hover { border-color: #b9c5d9; background: #fff; }
.auth-card input:focus-visible { border-color: #345cff; outline: 0; background: #fff; box-shadow: 0 0 0 4px rgba(52, 92, 255, .12); }
.auth-card button {
  min-height: 48px;
  margin-top: 3px;
  border: 0;
  border-radius: 11px;
  padding: 0 18px;
  background: #345cff;
  color: #fff;
  font-size: 15px;
  font-weight: 850;
  box-shadow: 0 12px 24px rgba(52, 92, 255, .24);
  cursor: pointer;
  transition: transform .18s ease, background .18s ease, box-shadow .18s ease;
}
.auth-card button:hover:not(:disabled) { transform: translateY(-1px); background: #294ee8; box-shadow: 0 15px 28px rgba(52, 92, 255, .28); }
.auth-card button:focus-visible { outline: 3px solid rgba(52, 92, 255, .25); outline-offset: 3px; }
.auth-card button:disabled { cursor: wait; opacity: .66; box-shadow: none; }
.form-message { min-height: 22px; margin: -3px 0 0; color: #bd3347; font-size: 13px; line-height: 1.5; }
.auth-security-note { display: flex; align-items: center; justify-content: center; gap: 7px; margin: 20px 0 0; color: #8a94a8; font-size: 12px; }
.auth-security-note span { color: #31a58f; font-size: 8px; }

@media (max-width: 520px) {
  .auth-page { padding: 20px 14px; }
  .auth-card { padding: 28px 24px; border-radius: 18px; }
  .auth-card h1 { font-size: 25px; }
  .auth-description { margin-bottom: 24px; }
}
```

- [ ] **Step 4: Verify desktop, tablet, and mobile rendering**

Run:

```bash
npm run ui:check
```

Expected: UI acceptance passes for 1440×1000, 900×1500, and 390×844. Inspect `ui-report/screenshots/admin-login-desktop.png` and `ui-report/screenshots/admin-login-mobile.png` for actual centered layout and visible controls.

- [ ] **Step 5: Commit the login UI**

```bash
git add static/admin-login.html static/styles.css scripts/ui-acceptance.mjs
git commit -m "优化：精简居中管理员登录页"
```

### Task 3: Prepare Explicit Production Credential Rotation

**Files:**
- Modify: `scripts/deploy-jd.sh`

- [ ] **Step 1: Add a failing deployment contract check**

Run these checks before changing the script:

```bash
rg -q 'ROTATE_ADMIN_CREDENTIALS' scripts/deploy-jd.sh
rg -q 'mjy-admin-password" generate' scripts/deploy-jd.sh
rg -q 'ADMIN_USERNAME=myjadmin' scripts/deploy-jd.sh
```

Expected: all three checks fail against the current script.

- [ ] **Step 2: Add an explicit rotation flag and safe generator**

Add locally:

```bash
ROTATE_ADMIN_CREDENTIALS="${ROTATE_ADMIN_CREDENTIALS:-0}"
```

Pass it into the remote environment in the existing `ssh` command. On the server, always replace `ADMIN_USERNAME` with `myjadmin`. Generate and replace the password hash only when the current hash is missing/invalid or the explicit flag is `1`:

```bash
printf '\n%s\n' 'ADMIN_USERNAME=myjadmin' | $SUDO tee -a "${APP_DIR}/.env" >/dev/null
if [[ "$ROTATE_ADMIN_CREDENTIALS" == "1" ]] \
  || ! $SUDO grep -q '^ADMIN_PASSWORD_HASH=\$argon2id\$' "${APP_DIR}/.env"; then
  initial_admin_password="$("${APP_DIR}/mjy-admin-password" generate)"
  admin_password_hash="$(printf '%s' "$initial_admin_password" | "${APP_DIR}/mjy-admin-password" hash)"
  $SUDO sed -i '/^ADMIN_PASSWORD_HASH=/d' "${APP_DIR}/.env"
  printf 'ADMIN_PASSWORD_HASH=%s\n' "$admin_password_hash" | $SUDO tee -a "${APP_DIR}/.env" >/dev/null
fi
```

Change the successful output label to `INITIAL_ADMIN_USERNAME=myjadmin`. Keep the default flag at `0`, keep `.env` backup/rollback behavior, and never copy a local `.env` to production.

- [ ] **Step 3: Verify the deployment contract**

Run:

```bash
bash -n scripts/deploy-jd.sh
rg -q 'ROTATE_ADMIN_CREDENTIALS' scripts/deploy-jd.sh
rg -q 'mjy-admin-password" generate' scripts/deploy-jd.sh
rg -q 'ADMIN_USERNAME=myjadmin' scripts/deploy-jd.sh
if rg -n 'openssl rand -base64|COPY_ENV' scripts/deploy-jd.sh; then exit 1; fi
```

Expected: syntax and all positive checks pass; forbidden base64 generation and environment-copy paths are absent.

- [ ] **Step 4: Commit the release preparation**

```bash
git add scripts/deploy-jd.sh
git commit -m "发布：支持显式轮换管理员凭据"
```

### Task 4: Full Regression and Production-Ready Handoff

**Files:**
- Verify only; no production mutation in this task.

- [ ] **Step 1: Run source and backend verification**

```bash
cargo fmt --all -- --check
cargo test --all-targets
git diff --check
```

Expected: all Rust tests pass and the worktree has no formatting errors.

- [ ] **Step 2: Run UI and voice verification**

```bash
npm run ui:check
npm run voice:check
```

Expected: UI and voice acceptance pass; login screenshots show the card centered on desktop and mobile.

- [ ] **Step 3: Verify device compatibility boundaries**

```bash
cargo test configured_non_demo_device_can_auth_and_upgrade_through_public_proxy
cargo test disabled_device_cannot_reconnect_with_preissued_token
```

Expected: configured devices can still authenticate and upgrade; disabled devices remain rejected.

- [ ] **Step 4: Record the clean handoff state**

```bash
git status --short
git log --oneline -4
```

Expected: clean worktree with separate commits for credentials, UI, and deployment preparation. Stop here and request explicit production-deployment approval before running `ROTATE_ADMIN_CREDENTIALS=1 scripts/deploy-jd.sh`.
