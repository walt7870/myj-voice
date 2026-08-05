# Multi-Order Refund Selection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow one conversation to create multiple independent orders and require a persisted, explicit target confirmation before refunding when more than one active order exists.

**Architecture:** Replace the single `latest_active_conversation_order` assumption with an event-and-order-row aggregate that returns every active order newest first. Derive each new shopping draft from messages after the latest successful `order_created` event, and persist multi-order refund selection in the existing event log. Order-affecting analysis completes before reply generation so spoken success is grounded in the actual API result.

**Tech Stack:** Rust, Axum, Tokio, SQLx/SQLite, Serde JSON, existing `turn_events` event log, Rust integration tests.

---

## File map

- Modify staged online baseline `src/web/mod.rs`: active-order aggregation, draft boundary, refund-selection state machine, grounded reply instructions, intent precedence.
- Modify staged online baseline `tests/app_tests.rs`: end-to-end text-chat coverage for multiple orders and refund selection.
- Modify staged online baseline `src/config.rs` only if the online role prompt still says an existing order blocks another order; keep static wording aligned with the deterministic state machine.
- Use existing `src/db.rs` readers (`list_conversation_messages`, `list_conversation_events`, `list_mock_order_payloads_by_conversation`); no schema migration is needed.
- Update local `docs/superpowers/specs/2026-07-29-multi-order-refund-selection-design.md` only if implementation reveals a design correction.

### Task 1: Establish an isolated copy of the jd production source

**Files:**
- Read: `jd:/opt/mjy-voice-shop-rs-src/**`
- Create: temporary isolated staging directory outside the dirty worktree

- [ ] **Step 1: Verify local user changes remain untouched**

Run:

```bash
git -C /Users/niu/Documents/公司/项目/美宜佳/新玩偶需求 status --short
```

Expected: the existing modified/untracked files are listed; do not stage, restore, or overwrite them.

- [ ] **Step 2: Verify the jd source and service revision**

Run:

```bash
ssh jd 'test -f /opt/mjy-voice-shop-rs-src/src/web/mod.rs && test -f /opt/mjy-voice-shop-rs-src/tests/app_tests.rs && systemctl is-active mjy-voice-shop-rs'
```

Expected: output ends with `active`.

- [ ] **Step 3: Copy the online source into an isolated staging directory**

Run:

```bash
release_stage=$(mktemp -d /tmp/mjy-multi-order.XXXXXX)
rsync -a --delete jd:/opt/mjy-voice-shop-rs-src/ "$release_stage/"
test -f "$release_stage/src/web/mod.rs"
cd "$release_stage"
if [ ! -d .git ]; then
  git init
  git add --all
  git commit -m "chore: snapshot jd production source"
fi
git tag -f jd-production-baseline
printf '%s\n' "$release_stage"
```

Expected: prints a unique `/tmp/mjy-multi-order.*` directory. Keep this exact path for all following tasks.

- [ ] **Step 4: Record the baseline checksums**

Run:

```bash
shasum -a 256 "$release_stage/src/web/mod.rs" "$release_stage/tests/app_tests.rs" "$release_stage/src/config.rs"
```

Expected: three hashes are recorded in the execution notes before editing.

### Task 2: Prove and fix independent draft boundaries for repeated orders

**Files:**
- Modify: staged `tests/app_tests.rs`
- Modify: staged `src/web/mod.rs` around `analyze_turn`

- [ ] **Step 1: Add a failing two-order integration test**

Add a test helper and test that post four messages into one conversation and inspect both persisted orders:

```rust
async fn post_text(app: &Router, conversation_id: &str, text: &str) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text": text}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn repeated_confirmations_create_distinct_orders_without_old_items() {
    let state = test_state().await;
    let pool = state.pool.clone();
    let app = router(state);
    let conversation_id = new_conversation_id(&app).await;

    post_text(&app, &conversation_id, "买一瓶可乐").await;
    let first = post_text(&app, &conversation_id, "确认下单").await;
    post_text(&app, &conversation_id, "买一瓶牛奶").await;
    let second = post_text(&app, &conversation_id, "对的").await;

    let first_id = created_order_id(&first);
    let second_id = created_order_id(&second);
    assert_ne!(first_id, second_id);

    let orders = db::list_mock_order_payloads_by_conversation(&pool, &conversation_id)
        .await
        .unwrap();
    assert_eq!(orders.len(), 2);
    let second_items = orders[0].payload["items"].as_array().unwrap();
    assert!(second_items.iter().any(|item| item["name"] == "牛奶"));
    assert!(!second_items.iter().any(|item| item["name"] == "可乐"));
}
```

If the online test suite lacks `new_conversation_id` or `created_order_id`, add these exact helpers beside `post_text`:

```rust
async fn new_conversation_id(app: &Router) -> String {
    let response = app
        .clone()
        .oneshot(Request::builder().method("POST").uri("/api/conversations/new").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    body["conversation_id"].as_str().unwrap().to_string()
}

fn created_order_id(body: &Value) -> String {
    body["events"].as_array().unwrap().iter()
        .find(|event| event["event_type"] == "order_created")
        .and_then(|event| event["payload"]["saleOrderId"].as_str().or_else(|| event["payload"]["order_id"].as_str()))
        .expect("expected order_created event")
        .to_string()
}
```

- [ ] **Step 2: Run the new test and verify the current bug**

Run:

```bash
cargo test repeated_confirmations_create_distinct_orders_without_old_items -- --nocapture
```

Expected: FAIL because the second response lacks `order_created`, or because only one order is persisted.

- [ ] **Step 3: Add a current-draft boundary helper**

Add this helper near `analyze_turn`; it excludes all user messages through the turn that last created an order:

```rust
async fn current_order_draft_text(
    state: &AppState,
    conversation_id: &str,
    fallback: &str,
) -> Result<String, ApiError> {
    let events = db::list_conversation_events(&state.pool, conversation_id).await?;
    let Some(boundary_turn_id) = events.iter().rev()
        .find(|event| event.event_type == "order_created")
        .map(|event| event.turn_id.as_str())
    else {
        return Ok(fallback.to_string());
    };

    let messages = db::list_conversation_messages(&state.pool, conversation_id).await?;
    let boundary_index = messages.iter().rposition(|message| message.turn_id == boundary_turn_id);
    let text = messages.iter()
        .enumerate()
        .filter(|(index, message)| {
            message.role == "user" && boundary_index.map_or(true, |boundary| *index > boundary)
        })
        .map(|(_, message)| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(text)
}
```

- [ ] **Step 4: Remove the existing-order block from new-order submission**

At the start of `analyze_turn`, derive matches from the bounded text and allow confirmation whenever the current draft is non-empty:

```rust
let draft_text = current_order_draft_text(state, conversation_id, round_user_text).await?;
let matches = match_products(&draft_text, &products);
let active_orders = active_conversation_orders(state, conversation_id).await?;
let should_submit_order = pending_refund.is_none()
    && !should_refund_order
    && !should_end_conversation
    && is_order_confirmation_intent(latest_user_text)
    && !matches.is_empty();
```

Remove every `active_order.is_none()` guard from `order_draft` and `should_submit_order`. Do not change the explicit confirmation allow/deny phrases.

- [ ] **Step 5: Run the targeted test**

Run:

```bash
cargo test repeated_confirmations_create_distinct_orders_without_old_items -- --nocapture
```

Expected: PASS with two different order IDs and no first-order item in the second order.

- [ ] **Step 6: Commit the independent-order change in staging**

Run:

```bash
git add src/web/mod.rs tests/app_tests.rs
git commit -m "fix: create independent orders in one conversation"
```

Expected: one commit containing only the draft-boundary implementation and its test.

### Task 3: Aggregate every active order by order ID

**Files:**
- Modify: staged `src/web/mod.rs` around `ActiveConversationOrder`
- Modify: staged `tests/app_tests.rs`

- [ ] **Step 1: Add a failing regression test for refunding one of two orders**

Create two orders, refund the newer order through `/api/orders/refund`, and assert the older order is still returned by the active-order behavior through a subsequent explicit refund request. The request must directly refund the remaining single order and emit `order_refunded`.

```rust
#[tokio::test]
async fn refunding_newest_order_leaves_older_order_active() {
    let state = test_state().await;
    let app = router(state);
    let conversation_id = new_conversation_id(&app).await;
    post_text(&app, &conversation_id, "买一瓶可乐").await;
    post_text(&app, &conversation_id, "确认下单").await;
    post_text(&app, &conversation_id, "买一瓶牛奶").await;
    let second = post_text(&app, &conversation_id, "确认下单").await;
    let second_id = created_order_id(&second);

    let response = app.clone().oneshot(
        Request::builder().method("POST").uri("/api/orders/refund")
            .header("content-type", "application/json")
            .body(Body::from(json!({"saleOrderId": second_id, "reason": "测试"}).to_string())).unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = post_text(&app, &conversation_id, "我要退单").await;
    assert!(body["events"].as_array().unwrap().iter().any(|event| event["event_type"] == "order_refunded"));
}
```

- [ ] **Step 2: Run the regression test and verify it fails**

Run:

```bash
cargo test refunding_newest_order_leaves_older_order_active -- --nocapture
```

Expected: FAIL under the old reverse-event scan that returns `None` after the latest refund event.

- [ ] **Step 3: Replace the single-order type and lookup**

Define an order model that carries stable sorting and spoken summary data:

```rust
#[derive(Debug, Clone)]
struct ActiveConversationOrder {
    order_id: String,
    payload: Value,
    created_at: String,
}

fn active_order_summary(order: &ActiveConversationOrder) -> String {
    let items = order.payload.get("items")
        .or_else(|| order.payload.get("data").and_then(|data| data.get("items")))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join("、"))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "该商品".to_string());
    let tail = order.order_id.chars().rev().take(4).collect::<String>().chars().rev().collect::<String>();
    format!("{}订单（尾号{}）", items, tail)
}
```

Add `HashMap` to the existing `std::collections` import and implement the aggregate as follows. Local rows override event payloads because they contain the newest mock refund status:

```rust
async fn active_conversation_orders(
    state: &AppState,
    conversation_id: &str,
) -> Result<Vec<ActiveConversationOrder>, ApiError> {
    let mut by_id = HashMap::<String, ActiveConversationOrder>::new();
    for event in db::list_conversation_events(&state.pool, conversation_id).await? {
        match event.event_type.as_str() {
            "order_created" => {
                if let Some(order_id) = order_id_from_payload(&event.payload) {
                    if is_closed_order_payload(&event.payload) {
                        by_id.remove(&order_id);
                    } else {
                        by_id.insert(order_id.clone(), ActiveConversationOrder {
                            order_id,
                            payload: event.payload,
                            created_at: event.created_at,
                        });
                    }
                }
            }
            "order_refunded" => {
                if let Some(order_id) = order_id_from_payload(&event.payload) {
                    by_id.remove(&order_id);
                }
            }
            _ => {}
        }
    }

    for row in db::list_mock_order_payloads_by_conversation(&state.pool, conversation_id).await? {
        let order_id = order_id_from_payload(&row.payload).unwrap_or(row.order_id);
        if is_closed_order_payload(&row.payload) {
            by_id.remove(&order_id);
        } else {
            by_id.insert(order_id.clone(), ActiveConversationOrder {
                order_id,
                payload: row.payload,
                created_at: row.created_at,
            });
        }
    }

    let mut orders = by_id.into_values().collect::<Vec<_>>();
    orders.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(orders)
}
```

Replace all callers of `latest_active_conversation_order` with this vector.

- [ ] **Step 4: Run both order tests**

Run:

```bash
cargo test repeated_confirmations_create_distinct_orders_without_old_items -- --nocapture
cargo test refunding_newest_order_leaves_older_order_active -- --nocapture
```

Expected: both PASS.

- [ ] **Step 5: Commit the active-order aggregation change in staging**

Run:

```bash
git add src/web/mod.rs tests/app_tests.rs
git commit -m "fix: aggregate active orders by order id"
```

Expected: one commit containing the per-order aggregate and its regression test.

### Task 4: Persist and execute multi-order refund selection

**Files:**
- Modify: staged `src/web/mod.rs`
- Modify: staged `tests/app_tests.rs`

- [ ] **Step 1: Add failing tests for default target, confirmation, and denial**

Use two active orders and assert:

```rust
let request = post_text(&app, &conversation_id, "我要退单").await;
assert!(has_event(&request, "refund_selection_requested"));
assert!(!has_event(&request, "order_refund_started"));

let confirm = post_text(&app, &conversation_id, "是的").await;
assert!(has_event(&confirm, "refund_selection_confirmed"));
assert!(has_event(&confirm, "order_refunded"));
assert!(!has_event(&confirm, "order_created"));
```

In a separate conversation, send “我要退单” then “不是”; assert `refund_selection_cancelled` exists and neither turn contains `order_refunded`.

Add this exact test helper:

```rust
fn has_event(body: &Value, event_type: &str) -> bool {
    body["events"].as_array().unwrap().iter()
        .any(|event| event["event_type"] == event_type)
}
```

- [ ] **Step 2: Run the new refund-selection tests and verify they fail**

Run:

```bash
cargo test multi_order_refund -- --nocapture
```

Expected: FAIL because the old logic immediately refunds the latest order and has no persisted selection.

- [ ] **Step 3: Add the persisted pending-selection reader**

```rust
#[derive(Debug, Clone)]
struct PendingRefundSelection {
    order_id: String,
    summary: String,
}

async fn pending_refund_selection(
    state: &AppState,
    conversation_id: &str,
) -> Result<Option<PendingRefundSelection>, ApiError> {
    let events = db::list_conversation_events(&state.pool, conversation_id).await?;
    for event in events.into_iter().rev() {
        match event.event_type.as_str() {
            "refund_selection_requested" | "refund_selection_changed" => {
                let Some(order_id) = event.payload.get("saleOrderId").and_then(Value::as_str) else { return Ok(None); };
                return Ok(Some(PendingRefundSelection {
                    order_id: order_id.to_string(),
                    summary: event.payload.get("summary").and_then(Value::as_str).unwrap_or("该订单").to_string(),
                }));
            }
            "refund_selection_cancelled" | "refund_selection_confirmed" | "order_refunded" => return Ok(None),
            _ => {}
        }
    }
    Ok(None)
}
```

- [ ] **Step 4: Add selection and confirmation helpers**

Implement the selection helpers with these bodies:

```rust
fn is_refund_confirmation_denial(text: &str) -> bool {
    let normalized = normalize_interrupt_text(text);
    ["不是", "不对", "不要这单", "换一个", "取消退单"]
        .iter()
        .any(|phrase| normalized.contains(phrase))
}

fn order_item_names(payload: &Value) -> Vec<&str> {
    payload.get("items")
        .or_else(|| payload.get("data").and_then(|data| data.get("items")))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .collect()
}

fn explicitly_selected_refund_target<'a>(
    text: &str,
    orders: &'a [ActiveConversationOrder],
) -> Option<&'a ActiveConversationOrder> {
    let normalized = normalize_interrupt_text(text);
    orders.iter().find(|order| {
        let tail = order.order_id.chars().rev().take(4).collect::<String>()
            .chars().rev().collect::<String>();
        let normalized_order_id = normalize_interrupt_text(&order.order_id);
        normalized.contains(normalized_order_id.as_str())
            || (!tail.is_empty() && normalized.contains(tail.as_str()))
            || order_item_names(&order.payload)
                .iter()
                .map(|name| normalize_interrupt_text(name))
                .any(|name| normalized.contains(name.as_str()))
    })
}

fn select_refund_target<'a>(
    text: &str,
    orders: &'a [ActiveConversationOrder],
) -> &'a ActiveConversationOrder {
    explicitly_selected_refund_target(text, orders).unwrap_or(&orders[0])
}

fn is_targeted_refund_intent(text: &str, orders: &[ActiveConversationOrder]) -> bool {
    let normalized = normalize_interrupt_text(text);
    explicitly_selected_refund_target(text, orders).is_some()
        && (normalized.contains("退")
            || normalized.contains("退款")
            || normalized.contains("取消"))
}

fn refund_selection_payload(
    order: &ActiveConversationOrder,
    orders: &[ActiveConversationOrder],
) -> Value {
    json!({
        "saleOrderId": order.order_id.as_str(),
        "summary": active_order_summary(order),
        "created_at": order.created_at.as_str(),
        "candidateOrderIds": orders.iter().map(|candidate| candidate.order_id.as_str()).collect::<Vec<_>>()
    })
}
```

The helper order is full order ID/order-ID tail/item name, then `orders[0]`. If multiple orders match the same item, vector ordering deliberately selects the newest. Do not reuse the general conversation-end matcher for confirmation denial.

- [ ] **Step 5: Give pending refund state highest intent precedence**

Add this decision type and function so pending refund state is evaluated before normal order confirmation:

```rust
#[derive(Debug, Clone)]
enum RefundDecision {
    None,
    NotFound,
    Direct(ActiveConversationOrder),
    Request(ActiveConversationOrder),
    Confirm(ActiveConversationOrder),
    Cancel,
    Change(ActiveConversationOrder),
    Await(PendingRefundSelection),
}

fn decide_refund(
    latest_user_text: &str,
    pending_refund: Option<PendingRefundSelection>,
    active_orders: &[ActiveConversationOrder],
) -> RefundDecision {
    if let Some(pending) = pending_refund {
        let current = active_orders.iter().find(|order| order.order_id == pending.order_id);
        let Some(current) = current else {
            return RefundDecision::NotFound;
        };
        if is_order_confirmation_intent(latest_user_text) {
            return RefundDecision::Confirm(current.clone());
        } else if is_refund_confirmation_denial(latest_user_text) {
            return RefundDecision::Cancel;
        } else if let Some(target) = explicitly_selected_refund_target(latest_user_text, active_orders) {
            if target.order_id != current.order_id {
                return RefundDecision::Change(target.clone());
            }
        } else {
            return RefundDecision::Await(pending);
        }
        return RefundDecision::Await(pending);
    }

    if !is_explicit_order_refund_intent(latest_user_text)
        && !is_targeted_refund_intent(latest_user_text, active_orders)
    {
        return RefundDecision::None;
    }
    match active_orders {
        [] => RefundDecision::NotFound,
        [only] => RefundDecision::Direct(only.clone()),
        many => RefundDecision::Request(select_refund_target(latest_user_text, many).clone()),
    }
}
```

In `analyze_turn`, map `Request` to `refund_selection_requested`, `Change` to `refund_selection_changed`, `Cancel` to `refund_selection_cancelled`, and `Confirm` to `refund_selection_confirmed` followed by the existing refund call. `Await` reuses its stored summary for the reply directive without an API call. `Direct` uses the existing single-order refund path. `NotFound` emits `refund_order_not_found`. Only `RefundDecision::None` is allowed to continue into draft/order-confirmation intent handling.

Before calling `refund_submitted_order`, re-read active orders and ensure the target is still active. Treat an already closed target as an idempotent completed result and do not call the external interface again.

- [ ] **Step 6: Run all refund-selection tests**

Run:

```bash
cargo test multi_order_refund -- --nocapture
cargo test explicit_cancel_after_created_order_refunds_and_ends_conversation -- --nocapture
```

Expected: multi-order tests PASS, and the existing one-order direct-refund test remains PASS.

- [ ] **Step 7: Add restart and target-switch tests**

Use one `SqlitePool` to create the two orders and `refund_selection_requested`, then construct a fresh `AppState` with the same pool to simulate service-state replacement. Confirm through the fresh router and assert `order_refunded`. In a second test, request a refund, answer with the older order's item name, assert `refund_selection_changed` without a refund, then answer “确认” and assert only that older order is refunded.

Also add these focused assertions:

- no active orders + “我要退单” emits `refund_order_not_found` and never `order_refund_started`;
- two milk orders + “我要退单” places the newest order ID in `refund_selection_requested`;
- two different orders + “退可乐那单” selects the coke order but does not refund until the following confirmation;
- sending a second “确认” after a completed selection does not create another `order_refund_started` for the same order;
- mocked order/refund failure produces an error directive and never produces success wording.

- [ ] **Step 8: Run the persisted-state tests**

Run:

```bash
cargo test refund_selection_survives_state_replacement -- --nocapture
cargo test changing_refund_target_requires_another_confirmation -- --nocapture
```

Expected: both PASS.

- [ ] **Step 9: Commit the refund-selection state machine in staging**

Run:

```bash
git add src/web/mod.rs tests/app_tests.rs
git commit -m "feat: confirm refund target for multiple orders"
```

Expected: one commit containing the persisted selection state machine and all multi-order refund tests.

### Task 5: Ground all spoken order results in completed analysis

**Files:**
- Modify: staged `src/web/mod.rs` around `process_voice_turn`, `order_context_prompt`, and `mock_reply`
- Modify: staged `tests/app_tests.rs`
- Optionally modify: staged `src/config.rs`

- [ ] **Step 1: Add failing reply-grounding assertions**

For the multi-order refund request, assert the assistant message contains the newest order summary and a confirmation question, but not “已退单”. For its confirmation turn, assert the assistant says the order was refunded. For a repeated successful order confirmation, assert the assistant response references the newly returned order ID rather than the previous ID.

- [ ] **Step 2: Run the grounding tests and verify they fail**

Run:

```bash
cargo test grounded_order_reply -- --nocapture
```

Expected: FAIL because current reply generation runs concurrently with order analysis and `mock_reply` is based only on raw user text.

- [ ] **Step 3: Return a reply directive with analysis events**

Change the analysis return type to:

```rust
struct TurnAnalysis {
    events: Vec<StreamEvent>,
    reply_instruction: Option<String>,
    mock_reply: Option<String>,
}
```

Populate exact outcome-specific wording:

```rust
// order_created
format!("订单已经真实下发，订单号：{}。只告知下发成功和该订单号。", order_id)

// refund_selection_requested / changed
format!("尚未执行退单。请只询问：确认退{}吗？", active_order_summary(target))

// order_refunded
format!("订单 {} 已真实退单成功。只简短告知处理成功。", order_id)

// failures
format!("订单操作失败：{}。必须明确告知失败，不得说已下发或已退单。", message)
```

- [ ] **Step 4: Await business analysis before starting LLM/TTS**

In `process_voice_turn`, replace the spawned analysis task with a direct awaited call before constructing `ChatMessage`s. Append `reply_instruction` as the final system message. In mock-provider mode, use `mock_reply` from the analysis outcome when present. Keep emitting the returned analysis events through the existing stream after `voice_done` if client ordering compatibility requires it.

- [ ] **Step 5: Align the static role prompt**

If `src/config.rs` still says “订单已下发后不要重复确认下单”, replace that clause with: “每次新的购买需求经用户明确确认后都应创建独立订单；订单是否成功以业务事件为准。多笔有效订单退单时必须先确认目标。” Do not weaken the existing protection against “退下/退出/裸词退单”.

- [ ] **Step 6: Run grounding and existing intent tests**

Run:

```bash
cargo test grounded_order_reply -- --nocapture
cargo test explicit_refund_requests_are_accepted -- --nocapture
cargo test ambiguous_or_bare_refund_words_are_rejected -- --nocapture
```

Expected: all PASS.

- [ ] **Step 7: Commit grounded order replies in staging**

Run:

```bash
git add src/web/mod.rs src/config.rs tests/app_tests.rs
git commit -m "fix: ground order replies in completed actions"
```

Expected: one commit; omit `src/config.rs` from `git add` if its online wording already meets the required contract.

### Task 6: Full verification, deploy to jd, and run acceptance

**Files:**
- Read: staged changes only
- Deploy with: staged `scripts/deploy-jd.sh`
- Back up: `/opt/mjy-voice-shop-rs` and `/opt/mjy-voice-shop-rs-src`

- [ ] **Step 1: Format and inspect the exact diff**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
git diff --check jd-production-baseline..HEAD -- src/web/mod.rs src/config.rs tests/app_tests.rs
git diff --stat jd-production-baseline..HEAD -- src/web/mod.rs src/config.rs tests/app_tests.rs
```

Expected: no formatting or whitespace errors; only intended files differ.

- [ ] **Step 2: Run the complete Rust test suite**

Run:

```bash
cargo test --all-targets
```

Expected: all tests PASS with zero failures.

- [ ] **Step 3: Build the release artifact**

Run:

```bash
cargo build --release
```

Expected: exit code 0 and `target/release/mjy-voice-shop-rs` exists.

- [ ] **Step 4: Create a dated jd rollback backup**

Run:

```bash
ssh jd 'backup=/opt/mjy-voice-shop-rs/backups/release-before-multi-order-$(date -u +%Y%m%dT%H%M%SZ); mkdir -p "$backup"; cp -a /opt/mjy-voice-shop-rs-src "$backup/source"; cp -a /opt/mjy-voice-shop-rs/mjy-voice-shop-rs "$backup/"; printf "%s\n" "$backup"'
```

Expected: prints one explicit backup directory under `/opt/mjy-voice-shop-rs/backups/`.

- [ ] **Step 5: Deploy without overwriting the online environment**

Run from the isolated staging directory:

```bash
COPY_ENV=0 bash scripts/deploy-jd.sh
```

Expected: deploy script completes, restarts the service, and reports healthy. Do not copy a local `.env`.

- [ ] **Step 6: Verify service health and startup logs**

Run:

```bash
ssh jd 'systemctl is-active mjy-voice-shop-rs; journalctl -u mjy-voice-shop-rs -n 80 --no-pager'
curl -k -fsS https://www.niuwancheng.cn/mjy-voice-shop/api/health
```

Expected: status is `active`, logs contain no panic/error loop, and the public health endpoint returns HTTP 200 with an OK payload.

- [ ] **Step 7: Run production acceptance through the existing authenticated client path**

Create one fresh conversation and execute:

1. “买一瓶可乐” → “确认下单”;
2. “买一瓶牛奶” → “对的”;
3. verify two different order IDs and distinct items in the admin API;
4. “我要退单” and verify no refund call yet, with a question about the milk order;
5. “是的” and verify only the milk order is refunded;
6. verify the coke order remains active.

Expected: admin data and event chain contain two `order_created`, one `refund_selection_requested`, one `refund_selection_confirmed`, and one target-specific `order_refunded`.

- [ ] **Step 8: Preserve the exact deployed source**

After acceptance, verify `/opt/mjy-voice-shop-rs-src` matches the staged source checksums for changed files and record the release time, service PID, backup path, test totals, and acceptance conversation ID in the completion report.
