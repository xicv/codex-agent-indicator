use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, Color};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StateKind {
    Idle,
    Working,
    Approval,
    Requested,
    Done,
    Error,
}

impl fmt::Display for StateKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Approval => "approval",
            Self::Requested => "requested",
            Self::Done => "done",
            Self::Error => "error",
        };
        formatter.write_str(value)
    }
}

impl FromStr for StateKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "idle" | "clear" | "off" => Ok(Self::Idle),
            "working" | "running" | "busy" => Ok(Self::Working),
            "approval" | "approve" | "permission" => Ok(Self::Approval),
            "requested" | "request" | "input" | "waiting" => Ok(Self::Requested),
            "done" | "complete" | "completed" => Ok(Self::Done),
            "error" | "failed" | "failure" => Ok(Self::Error),
            _ => bail!(
                "unknown state {value:?}; use idle, working, approval, requested, done, or error"
            ),
        }
    }
}

#[derive(Clone, Debug)]
struct Slot {
    session_id: String,
    state: StateKind,
    updated_at: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightingChange {
    pub key: u8,
    pub color: Color,
}

#[derive(Debug)]
pub struct Engine {
    slots: Vec<Option<Slot>>,
    waiting: VecDeque<Slot>,
}

#[derive(Debug, Deserialize)]
pub struct RestoredSlot {
    pub slot: usize,
    pub session_id: String,
    pub state: StateKind,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RestoredQueuedSession {
    pub session_id: String,
    pub state: StateKind,
    pub updated_at: u64,
}

impl Engine {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            slots: vec![None; max_sessions],
            waiting: VecDeque::new(),
        }
    }

    pub fn restore(
        max_sessions: usize,
        slots: Vec<RestoredSlot>,
        queued_sessions: Vec<RestoredQueuedSession>,
    ) -> Self {
        let mut engine = Self::new(max_sessions);
        let mut restored_sessions = HashSet::new();
        for restored in slots {
            let Some(index) = restored.slot.checked_sub(1) else {
                continue;
            };
            if index >= max_sessions
                || restored.session_id.is_empty()
                || restored.state == StateKind::Idle
                || engine.slots[index].is_some()
                || !restored_sessions.insert(restored.session_id.clone())
            {
                continue;
            }
            engine.slots[index] = Some(Slot {
                session_id: restored.session_id,
                state: restored.state,
                updated_at: restored.updated_at,
            });
        }
        for restored in queued_sessions {
            if restored.session_id.is_empty()
                || restored.state == StateKind::Idle
                || !restored_sessions.insert(restored.session_id.clone())
            {
                continue;
            }
            engine.waiting.push_back(Slot {
                session_id: restored.session_id,
                state: restored.state,
                updated_at: restored.updated_at,
            });
        }
        while let Some(slot_index) = engine.select_slot() {
            let Some(waiting) = engine.waiting.pop_front() else {
                break;
            };
            engine.slots[slot_index] = Some(waiting);
        }
        engine
    }

    pub fn transition(
        &mut self,
        session_id: &str,
        state: StateKind,
        now: u64,
        config: &AppConfig,
    ) -> Vec<LightingChange> {
        if state == StateKind::Idle {
            return self.clear_session(session_id, config);
        }

        if let Some(waiting) = self
            .waiting
            .iter_mut()
            .find(|waiting| waiting.session_id == session_id)
        {
            waiting.state = state;
            waiting.updated_at = now;
            return Vec::new();
        }

        let Some(slot_index) = self
            .slots
            .iter()
            .position(|slot| {
                slot.as_ref()
                    .is_some_and(|slot| slot.session_id == session_id)
            })
            .or_else(|| self.select_slot())
        else {
            self.waiting.push_back(Slot {
                session_id: session_id.to_string(),
                state,
                updated_at: now,
            });
            return Vec::new();
        };

        let unchanged = self.slots[slot_index]
            .as_ref()
            .is_some_and(|slot| slot.session_id == session_id && slot.state == state);
        self.slots[slot_index] = Some(Slot {
            session_id: session_id.to_string(),
            state,
            updated_at: now,
        });

        if unchanged {
            Vec::new()
        } else {
            vec![LightingChange {
                key: config.device.slot_keys[slot_index],
                color: config.color_for(state),
            }]
        }
    }

    pub fn reconcile(
        &mut self,
        session_id: &str,
        state: StateKind,
        occurred_at: u64,
        config: &AppConfig,
    ) -> Vec<LightingChange> {
        let updated_at = self
            .slots
            .iter()
            .filter_map(Option::as_ref)
            .chain(self.waiting.iter())
            .find(|slot| slot.session_id == session_id)
            .map_or(occurred_at, |slot| slot.updated_at.max(occurred_at));
        self.transition(session_id, state, updated_at, config)
    }

    pub fn clear_session(
        &mut self,
        session_id: &str,
        config: &AppConfig,
    ) -> Vec<LightingChange> {
        if let Some(waiting_index) = self
            .waiting
            .iter()
            .position(|waiting| waiting.session_id == session_id)
        {
            self.waiting.remove(waiting_index);
            return Vec::new();
        }

        let Some(slot_index) = self.slots.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|slot| slot.session_id == session_id)
        }) else {
            return Vec::new();
        };

        self.release_slot(slot_index, config)
    }

    pub fn clear_all(&mut self, config: &AppConfig) -> Vec<LightingChange> {
        self.waiting.clear();
        let mut changes = Vec::new();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.take().is_some() {
                changes.push(LightingChange {
                    key: config.device.slot_keys[index],
                    color: config.lighting.background,
                });
            }
        }
        changes
    }

    pub fn repaint(&self, config: &AppConfig) -> Vec<LightingChange> {
        self.slots
            .iter()
            .enumerate()
            .map(|(index, slot)| LightingChange {
                key: config.device.slot_keys[index],
                color: slot
                    .as_ref()
                    .map(|slot| config.color_for(slot.state))
                    .unwrap_or(config.lighting.background),
            })
            .collect()
    }

    pub fn active_lighting(
        &self,
        brightness_percent: u8,
        config: &AppConfig,
    ) -> Vec<LightingChange> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.as_ref().map(|slot| LightingChange {
                    key: config.device.slot_keys[index],
                    color: config
                        .color_for(slot.state)
                        .scale_percent(brightness_percent),
                })
            })
            .collect()
    }

    pub fn snapshot(&self, config: &AppConfig) -> Vec<SlotSnapshot> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.as_ref().map(|slot| SlotSnapshot {
                    slot: index + 1,
                    key: format!("G{}", index + 1),
                    key_address: config.device.slot_keys[index],
                    session_id: slot.session_id.clone(),
                    state: slot.state,
                    color: config.color_for(slot.state),
                    updated_at: slot.updated_at,
                    expires_at: None,
                })
            })
            .collect()
    }

    pub fn queued_snapshot(&self) -> Vec<RestoredQueuedSession> {
        self.waiting
            .iter()
            .map(|waiting| RestoredQueuedSession {
                session_id: waiting.session_id.clone(),
                state: waiting.state,
                updated_at: waiting.updated_at,
            })
            .collect()
    }

    pub fn tracked_session_ids(&self) -> Vec<String> {
        self.slots
            .iter()
            .filter_map(Option::as_ref)
            .map(|slot| slot.session_id.clone())
            .chain(
                self.waiting
                    .iter()
                    .map(|waiting| waiting.session_id.clone()),
            )
            .collect()
    }

    pub fn has_active_slots(&self) -> bool {
        self.slots.iter().any(Option::is_some)
    }

    pub fn session_for_g_key(&self, g_key: usize) -> Option<&str> {
        let slot_index = g_key.checked_sub(1)?;
        self.slots
            .get(slot_index)?
            .as_ref()
            .map(|slot| slot.session_id.as_str())
    }

    pub fn acknowledge_g_key(
        &mut self,
        g_key: usize,
        config: &AppConfig,
    ) -> Vec<LightingChange> {
        let Some(slot_index) = g_key.checked_sub(1) else {
            return Vec::new();
        };
        let Some(slot) = self.slots.get(slot_index).and_then(Option::as_ref) else {
            return Vec::new();
        };
        if slot.state == StateKind::Working {
            return Vec::new();
        }

        self.release_slot(slot_index, config)
    }

    fn release_slot(
        &mut self,
        slot_index: usize,
        config: &AppConfig,
    ) -> Vec<LightingChange> {
        self.slots[slot_index] = self.waiting.pop_front();
        vec![LightingChange {
            key: config.device.slot_keys[slot_index],
            color: self.slots[slot_index]
                .as_ref()
                .map(|slot| config.color_for(slot.state))
                .unwrap_or(config.lighting.background),
        }]
    }

    fn select_slot(&self) -> Option<usize> {
        self.slots.iter().position(Option::is_none)
    }
}

#[derive(Debug, Serialize)]
pub struct SlotSnapshot {
    pub slot: usize,
    pub key: String,
    pub key_address: u8,
    pub session_id: String,
    pub state: StateKind,
    pub color: Color,
    pub updated_at: u64,
    pub expires_at: Option<u64>,
}

#[cfg(test)]
mod tests {
    use crate::config::{AppConfig, Color};

    use super::{Engine, RestoredQueuedSession, RestoredSlot, StateKind};

    #[test]
    fn assigns_and_reuses_a_slot() {
        let config = AppConfig::default();
        let mut engine = Engine::new(5);
        let first = engine.transition("a", StateKind::Working, 10, &config);
        assert_eq!(first[0].key, 0xb4);

        assert!(
            engine
                .transition("a", StateKind::Working, 11, &config)
                .is_empty()
        );
        let changed = engine.transition("a", StateKind::Approval, 12, &config);
        assert_eq!(changed[0].key, 0xb4);
        assert_eq!(engine.snapshot(&config)[0].state, StateKind::Approval);
    }

    #[test]
    fn sixth_working_task_does_not_remap_existing_g_keys() {
        let config = AppConfig::default();
        let mut engine = Engine::new(5);
        for index in 1..=5 {
            engine.transition(
                &format!("task-{index}"),
                StateKind::Working,
                index,
                &config,
            );
        }
        let before = engine
            .snapshot(&config)
            .into_iter()
            .map(|slot| (slot.key, slot.session_id))
            .collect::<Vec<_>>();

        let changes = engine.transition("task-6", StateKind::Working, 6, &config);

        let after = engine
            .snapshot(&config)
            .into_iter()
            .map(|slot| (slot.key, slot.session_id))
            .collect::<Vec<_>>();
        assert!(changes.is_empty());
        assert_eq!(after, before);
    }

    #[test]
    fn oldest_waiting_task_is_promoted_when_a_g_key_becomes_free() {
        let config = AppConfig::default();
        let mut engine = Engine::new(5);
        for index in 1..=7 {
            engine.transition(
                &format!("task-{index}"),
                StateKind::Working,
                index,
                &config,
            );
        }

        let changes = engine.clear_session("task-3", &config);

        assert_eq!(
            changes,
            [super::LightingChange {
                key: config.device.slot_keys[2],
                color: config.color_for(StateKind::Working),
            }]
        );
        let snapshot = engine.snapshot(&config);
        assert_eq!(snapshot[2].session_id, "task-6");
        assert!(!snapshot.iter().any(|slot| slot.session_id == "task-7"));
    }

    #[test]
    fn waiting_task_uses_its_latest_state_when_promoted() {
        let mut config = AppConfig::default();
        config.behavior.max_sessions = 1;
        config.device.slot_keys.truncate(1);
        let mut engine = Engine::new(1);
        engine.transition("visible", StateKind::Working, 10, &config);
        engine.transition("waiting", StateKind::Working, 20, &config);
        engine.transition("waiting", StateKind::Requested, 30, &config);

        let changes = engine.clear_session("visible", &config);

        assert_eq!(
            changes,
            [super::LightingChange {
                key: config.device.slot_keys[0],
                color: config.color_for(StateKind::Requested),
            }]
        );
        let promoted = &engine.snapshot(&config)[0];
        assert_eq!(promoted.session_id, "waiting");
        assert_eq!(promoted.state, StateKind::Requested);
        assert_eq!(promoted.updated_at, 30);
    }

    #[test]
    fn removed_waiting_task_is_never_promoted() {
        let mut config = AppConfig::default();
        config.behavior.max_sessions = 1;
        config.device.slot_keys.truncate(1);
        let mut engine = Engine::new(1);
        engine.transition("visible", StateKind::Working, 10, &config);
        engine.transition("removed", StateKind::Working, 20, &config);
        engine.transition("next", StateKind::Working, 30, &config);

        assert!(engine.clear_session("removed", &config).is_empty());
        engine.clear_session("visible", &config);

        assert_eq!(engine.session_for_g_key(1), Some("next"));
        assert!(
            !engine
                .tracked_session_ids()
                .iter()
                .any(|session_id| session_id == "removed")
        );
    }

    #[test]
    fn preserves_existing_working_and_terminal_slots_when_full() {
        let mut config = AppConfig::default();
        config.behavior.max_sessions = 2;
        config.device.slot_keys.truncate(2);
        let mut engine = Engine::new(2);
        engine.transition("working", StateKind::Working, 1, &config);
        engine.transition("done", StateKind::Done, 2, &config);
        engine.transition("new", StateKind::Working, 3, &config);

        let snapshot = engine.snapshot(&config);
        assert!(snapshot.iter().any(|slot| slot.session_id == "done"));
        assert!(snapshot.iter().any(|slot| slot.session_id == "working"));
        assert!(!snapshot.iter().any(|slot| slot.session_id == "new"));
    }

    #[test]
    fn leaves_new_task_unassigned_when_every_slot_needs_acknowledgement() {
        let mut config = AppConfig::default();
        config.behavior.max_sessions = 2;
        config.device.slot_keys.truncate(2);
        let mut engine = Engine::new(2);
        engine.transition("done", StateKind::Done, 1, &config);
        engine.transition("error", StateKind::Error, 2, &config);

        assert!(
            engine
                .transition("new", StateKind::Working, 3, &config)
                .is_empty()
        );
        let snapshot = engine.snapshot(&config);
        assert!(snapshot.iter().any(|slot| slot.session_id == "done"));
        assert!(snapshot.iter().any(|slot| slot.session_id == "error"));
    }

    #[test]
    fn keeps_terminal_state_until_user_acknowledges_it() {
        let config = AppConfig::default();
        let mut engine = Engine::new(5);
        engine.transition("a", StateKind::Done, 100, &config);
        let snapshot = engine.snapshot(&config);
        assert_eq!(snapshot[0].state, StateKind::Done);
        assert_eq!(snapshot[0].expires_at, None);
    }

    #[test]
    fn acknowledging_a_finished_g_key_clears_its_indicator() {
        let config = AppConfig::default();
        let mut engine = Engine::new(5);
        engine.transition("finished-task", StateKind::Done, 100, &config);

        let changes = engine.acknowledge_g_key(1, &config);

        assert_eq!(changes[0].key, 0xb4);
        assert_eq!(changes[0].color, config.lighting.background);
        assert!(engine.snapshot(&config).is_empty());
    }

    #[test]
    fn opening_a_working_g_key_keeps_its_live_indicator() {
        let config = AppConfig::default();
        let mut engine = Engine::new(5);
        engine.transition("working-task", StateKind::Working, 100, &config);

        assert!(engine.acknowledge_g_key(1, &config).is_empty());
        assert_eq!(engine.snapshot(&config)[0].state, StateKind::Working);
    }

    #[test]
    fn builds_a_scaled_frame_for_active_g_keys_only() {
        let config = AppConfig::default();
        let mut engine = Engine::new(5);
        engine.transition("working", StateKind::Working, 10, &config);
        engine.transition("approval", StateKind::Approval, 11, &config);

        let frame = engine.active_lighting(5, &config);

        assert_eq!(frame.len(), 2);
        assert_eq!(frame[0].key, 0xb4);
        assert_eq!(
            frame[0].color,
            Color {
                red: 0,
                green: 6,
                blue: 12,
            }
        );
        assert_eq!(frame[1].key, 0xb5);
        assert_eq!(
            frame[1].color,
            Color {
                red: 12,
                green: 7,
                blue: 0,
            }
        );
    }

    #[test]
    fn resolves_only_occupied_g_key_slots_to_sessions() {
        let config = AppConfig::default();
        let mut engine = Engine::new(5);
        engine.transition("task-one", StateKind::Working, 10, &config);
        engine.transition("task-two", StateKind::Approval, 11, &config);

        assert_eq!(engine.session_for_g_key(1), Some("task-one"));
        assert_eq!(engine.session_for_g_key(2), Some("task-two"));
        assert_eq!(engine.session_for_g_key(3), None);
        assert_eq!(engine.session_for_g_key(0), None);
        assert_eq!(engine.session_for_g_key(6), None);
    }

    #[test]
    fn restores_persisted_slots_without_accepting_duplicates_or_invalid_entries() {
        let config = AppConfig::default();
        let engine = Engine::restore(
            5,
            vec![
                RestoredSlot {
                    slot: 2,
                    session_id: "working-task".to_owned(),
                    state: StateKind::Working,
                    updated_at: 20,
                },
                RestoredSlot {
                    slot: 3,
                    session_id: "working-task".to_owned(),
                    state: StateKind::Done,
                    updated_at: 21,
                },
                RestoredSlot {
                    slot: 6,
                    session_id: "out-of-range".to_owned(),
                    state: StateKind::Error,
                    updated_at: 22,
                },
                RestoredSlot {
                    slot: 4,
                    session_id: String::new(),
                    state: StateKind::Approval,
                    updated_at: 23,
                },
            ],
            Vec::new(),
        );

        let snapshot = engine.snapshot(&config);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].slot, 2);
        assert_eq!(snapshot[0].session_id, "working-task");
        assert_eq!(snapshot[0].state, StateKind::Working);
    }

    #[test]
    fn restores_waiting_tasks_in_fifo_order() {
        let mut config = AppConfig::default();
        config.behavior.max_sessions = 1;
        config.device.slot_keys.truncate(1);
        let mut engine = Engine::restore(
            1,
            vec![RestoredSlot {
                slot: 1,
                session_id: "visible".to_owned(),
                state: StateKind::Working,
                updated_at: 10,
            }],
            vec![
                RestoredQueuedSession {
                    session_id: "waiting-first".to_owned(),
                    state: StateKind::Done,
                    updated_at: 20,
                },
                RestoredQueuedSession {
                    session_id: "waiting-second".to_owned(),
                    state: StateKind::Requested,
                    updated_at: 30,
                },
            ],
        );

        engine.clear_session("visible", &config);
        assert_eq!(
            engine.session_for_g_key(1),
            Some("waiting-first")
        );
        engine.acknowledge_g_key(1, &config);
        assert_eq!(
            engine.session_for_g_key(1),
            Some("waiting-second")
        );
    }

    #[test]
    fn reconciliation_changes_truth_without_regressing_event_recency() {
        let config = AppConfig::default();
        let mut engine = Engine::new(5);
        engine.transition("task", StateKind::Working, 200, &config);

        engine.reconcile("task", StateKind::Done, 150, &config);

        let snapshot = engine.snapshot(&config);
        assert_eq!(snapshot[0].state, StateKind::Done);
        assert_eq!(snapshot[0].updated_at, 200);
    }

    #[test]
    fn waiting_reconciliation_changes_truth_without_regressing_event_recency() {
        let mut config = AppConfig::default();
        config.behavior.max_sessions = 1;
        config.device.slot_keys.truncate(1);
        let mut engine = Engine::new(1);
        engine.transition("visible", StateKind::Working, 100, &config);
        engine.transition("waiting", StateKind::Working, 200, &config);

        engine.reconcile("waiting", StateKind::Done, 150, &config);
        engine.clear_session("visible", &config);

        let snapshot = engine.snapshot(&config);
        assert_eq!(snapshot[0].state, StateKind::Done);
        assert_eq!(snapshot[0].updated_at, 200);
    }
}
