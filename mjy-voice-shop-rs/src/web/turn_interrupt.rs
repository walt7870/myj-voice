use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use tokio::sync::{watch, Mutex};

const RECENT_TURN_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptStatus {
    Interrupted,
    AlreadyInterrupted,
    AlreadyFinished,
    ConversationMismatch,
    UnknownTurn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegisterError {
    TurnAlreadyActive,
    TurnRecentlyFinished,
}

#[derive(Clone, Default)]
pub struct TurnInterruptRegistry {
    inner: Arc<Mutex<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    active: HashMap<String, ActiveTurn>,
    recent: VecDeque<RecentTurn>,
}

struct ActiveTurn {
    conversation_id: String,
    interrupted: bool,
    cancellation: watch::Sender<bool>,
}

struct RecentTurn {
    conversation_id: String,
    turn_id: String,
}

impl TurnInterruptRegistry {
    pub async fn register(
        &self,
        conversation_id: &str,
        turn_id: &str,
    ) -> Result<watch::Receiver<bool>, RegisterError> {
        let mut state = self.inner.lock().await;
        if state.active.contains_key(turn_id) {
            return Err(RegisterError::TurnAlreadyActive);
        }
        if state.recent.iter().any(|turn| turn.turn_id == turn_id) {
            return Err(RegisterError::TurnRecentlyFinished);
        }

        let (cancellation, receiver) = watch::channel(false);
        state.active.insert(
            turn_id.to_string(),
            ActiveTurn {
                conversation_id: conversation_id.to_string(),
                interrupted: false,
                cancellation,
            },
        );
        Ok(receiver)
    }

    pub async fn interrupt(&self, conversation_id: &str, turn_id: &str) -> InterruptStatus {
        let mut state = self.inner.lock().await;
        if let Some(turn) = state.active.get_mut(turn_id) {
            if turn.conversation_id != conversation_id {
                return InterruptStatus::ConversationMismatch;
            }
            if turn.interrupted {
                return InterruptStatus::AlreadyInterrupted;
            }

            turn.interrupted = true;
            let _ = turn.cancellation.send(true);
            return InterruptStatus::Interrupted;
        }

        if let Some(turn) = state
            .recent
            .iter()
            .rev()
            .find(|turn| turn.turn_id == turn_id)
        {
            return if turn.conversation_id == conversation_id {
                InterruptStatus::AlreadyFinished
            } else {
                InterruptStatus::ConversationMismatch
            };
        }

        InterruptStatus::UnknownTurn
    }

    pub async fn finish(&self, conversation_id: &str, turn_id: &str) {
        let mut state = self.inner.lock().await;
        let belongs_to_conversation = state
            .active
            .get(turn_id)
            .is_some_and(|turn| turn.conversation_id == conversation_id);
        if !belongs_to_conversation {
            return;
        }

        state.active.remove(turn_id);
        state.recent.push_back(RecentTurn {
            conversation_id: conversation_id.to_string(),
            turn_id: turn_id.to_string(),
        });
        while state.recent.len() > RECENT_TURN_LIMIT {
            state.recent.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InterruptStatus, RegisterError, TurnInterruptRegistry};

    #[tokio::test]
    async fn first_interrupt_is_observable_by_the_registered_turn() {
        let registry = TurnInterruptRegistry::default();
        let mut cancellation = registry.register("conversation-a", "turn-1").await.unwrap();

        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::Interrupted
        );
        cancellation.changed().await.unwrap();
        assert!(*cancellation.borrow());
    }

    #[tokio::test]
    async fn repeated_interrupt_is_idempotent() {
        let registry = TurnInterruptRegistry::default();
        registry.register("conversation-a", "turn-1").await.unwrap();

        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::Interrupted
        );
        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::AlreadyInterrupted
        );
    }

    #[tokio::test]
    async fn interrupt_rejects_a_different_conversation() {
        let registry = TurnInterruptRegistry::default();
        let cancellation = registry.register("conversation-a", "turn-1").await.unwrap();

        assert_eq!(
            registry.interrupt("conversation-b", "turn-1").await,
            InterruptStatus::ConversationMismatch
        );
        assert!(!*cancellation.borrow());
    }

    #[tokio::test]
    async fn finished_turn_is_reported_as_already_finished() {
        let registry = TurnInterruptRegistry::default();
        registry.register("conversation-a", "turn-1").await.unwrap();

        registry.finish("conversation-a", "turn-1").await;

        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::AlreadyFinished
        );
        assert_eq!(
            registry.interrupt("conversation-b", "turn-1").await,
            InterruptStatus::ConversationMismatch
        );
    }

    #[tokio::test]
    async fn unknown_turn_is_reported() {
        let registry = TurnInterruptRegistry::default();

        assert_eq!(
            registry.interrupt("conversation-a", "missing-turn").await,
            InterruptStatus::UnknownTurn
        );
    }

    #[tokio::test]
    async fn recent_turns_evict_the_oldest_entry_after_sixty_four_finishes() {
        let registry = TurnInterruptRegistry::default();

        for index in 0..=64 {
            let turn_id = format!("turn-{index}");
            registry.register("conversation-a", &turn_id).await.unwrap();
            registry.finish("conversation-a", &turn_id).await;
        }

        assert_eq!(
            registry.interrupt("conversation-a", "turn-0").await,
            InterruptStatus::UnknownTurn
        );
        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::AlreadyFinished
        );
        assert_eq!(
            registry.interrupt("conversation-a", "turn-64").await,
            InterruptStatus::AlreadyFinished
        );

        let reused = registry.register("conversation-a", "turn-0").await.unwrap();
        assert_eq!(
            registry.interrupt("conversation-a", "turn-0").await,
            InterruptStatus::Interrupted
        );
        assert!(*reused.borrow());
    }

    #[tokio::test]
    async fn duplicate_active_turn_registration_is_rejected() {
        let registry = TurnInterruptRegistry::default();
        let first = registry.register("conversation-a", "turn-1").await.unwrap();

        assert!(matches!(
            registry.register("conversation-a", "turn-1").await,
            Err(RegisterError::TurnAlreadyActive)
        ));
        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::Interrupted
        );
        assert!(*first.borrow());
    }

    #[tokio::test]
    async fn recently_finished_turn_id_is_rejected_without_changing_terminal_state() {
        let registry = TurnInterruptRegistry::default();
        let mut old = registry.register("conversation-a", "turn-1").await.unwrap();
        registry.finish("conversation-a", "turn-1").await;
        assert!(old.changed().await.is_err());

        assert!(matches!(
            registry.register("conversation-a", "turn-1").await,
            Err(RegisterError::TurnRecentlyFinished)
        ));
        assert!(matches!(
            registry.register("conversation-b", "turn-1").await,
            Err(RegisterError::TurnRecentlyFinished)
        ));
        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::AlreadyFinished
        );
        registry.finish("conversation-a", "turn-1").await;

        assert!(!*old.borrow());
        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::AlreadyFinished
        );
        assert_eq!(
            registry.interrupt("conversation-b", "turn-1").await,
            InterruptStatus::ConversationMismatch
        );
    }

    #[tokio::test]
    async fn finish_from_a_different_conversation_is_a_safe_no_op() {
        let registry = TurnInterruptRegistry::default();
        let cancellation = registry.register("conversation-a", "turn-1").await.unwrap();

        registry.finish("conversation-b", "turn-1").await;

        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::Interrupted
        );
        assert!(*cancellation.borrow());
    }

    #[tokio::test]
    async fn repeated_finish_is_a_safe_no_op() {
        let registry = TurnInterruptRegistry::default();
        registry.register("conversation-a", "turn-1").await.unwrap();

        registry.finish("conversation-a", "turn-1").await;
        registry.finish("conversation-a", "turn-1").await;

        assert_eq!(
            registry.interrupt("conversation-a", "turn-1").await,
            InterruptStatus::AlreadyFinished
        );
    }
}
