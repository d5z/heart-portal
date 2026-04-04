//! Core event types — the vocabulary of stimuli that can enter a being's consciousness.
//! These types are locked: adding a new event variant is a deliberate design decision.

use serde::{Deserialize, Serialize};

/// Priority determines event processing order.
/// Lower number = higher priority = processed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EventPriority {
    /// SOS — process immediately, may interrupt current work.
    Critical = 0,
    /// User message — high priority, the being's primary channel.
    High = 1,
    /// Cortex hint, background thought — medium priority.
    Medium = 2,
    /// Cron tick, sensor reading, internal timer — low priority.
    Low = 3,
}

/// Something that wants Core's attention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CoreEvent {
    /// User message from a channel (loom, API, etc.)
    Message {
        channel_id: String,
        content: String,
        // Optional: reply channel for response routing
        // reply_tx will be added when event loop is implemented (non-Serialize)
    },
    /// Sensor alert from Sensorium (via IPC).
    SensorAlert {
        sense_name: String,
        value: String,
        threshold: String,
        severity: String,
    },
    /// Cortex has a background thought or suggestion.
    CortexHint {
        kind: String,
        content: String,
    },
    /// Cron trigger.
    CronTick {
        cron_id: String,
    },
    /// Internal timer (idle thinking, periodic health).
    InternalTimer {
        name: String,
    },
    /// Graceful shutdown.
    Shutdown,
}

impl CoreEvent {
    /// Get the priority of this event.
    pub fn priority(&self) -> EventPriority {
        match self {
            Self::SensorAlert { severity, .. } => {
                if severity == "critical" {
                    EventPriority::Critical
                } else {
                    EventPriority::Medium
                }
            }
            Self::Shutdown => EventPriority::Critical,
            Self::Message { .. } => EventPriority::High,
            Self::CortexHint { .. } => EventPriority::Medium,
            Self::CronTick { .. } | Self::InternalTimer { .. } => EventPriority::Low,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_priority_order() {
        // Critical < High < Medium < Low
        assert!(EventPriority::Critical < EventPriority::High);
        assert!(EventPriority::High < EventPriority::Medium);
        assert!(EventPriority::Medium < EventPriority::Low);
    }

    #[test]
    fn test_message_priority() {
        let event = CoreEvent::Message {
            channel_id: "loom".to_string(),
            content: "hello".to_string(),
        };
        assert_eq!(event.priority(), EventPriority::High);
    }

    #[test]
    fn test_sensor_alert_critical() {
        let event = CoreEvent::SensorAlert {
            sense_name: "temp".to_string(),
            value: "95".to_string(),
            threshold: "80".to_string(),
            severity: "critical".to_string(),
        };
        assert_eq!(event.priority(), EventPriority::Critical);
    }

    #[test]
    fn test_sensor_alert_warning() {
        let event = CoreEvent::SensorAlert {
            sense_name: "disk".to_string(),
            value: "85".to_string(),
            threshold: "80".to_string(),
            severity: "warning".to_string(),
        };
        assert_eq!(event.priority(), EventPriority::Medium);
    }

    #[test]
    fn test_cron_low() {
        let event = CoreEvent::CronTick {
            cron_id: "memory_pipeline".to_string(),
        };
        assert_eq!(event.priority(), EventPriority::Low);
    }

    #[test]
    fn test_shutdown_critical() {
        let event = CoreEvent::Shutdown;
        assert_eq!(event.priority(), EventPriority::Critical);
    }

    #[test]
    fn test_internal_timer_low() {
        let event = CoreEvent::InternalTimer {
            name: "idle_thinking".to_string(),
        };
        assert_eq!(event.priority(), EventPriority::Low);
    }

    #[test]
    fn test_cortex_hint_medium() {
        let event = CoreEvent::CortexHint {
            kind: "association".to_string(),
            content: "remember the dream about water".to_string(),
        };
        assert_eq!(event.priority(), EventPriority::Medium);
    }

    #[test]
    fn test_serialization() {
        let event = CoreEvent::Message {
            channel_id: "api".to_string(),
            content: "test message".to_string(),
        };
        
        // Should serialize and deserialize correctly
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: CoreEvent = serde_json::from_str(&json).unwrap();
        
        match (&event, &deserialized) {
            (
                CoreEvent::Message { channel_id: c1, content: m1 },
                CoreEvent::Message { channel_id: c2, content: m2 },
            ) => {
                assert_eq!(c1, c2);
                assert_eq!(m1, m2);
            }
            _ => panic!("Serialization failed"),
        }
    }

    #[test]
    fn test_priority_serialization() {
        let priority = EventPriority::Critical;
        let json = serde_json::to_string(&priority).unwrap();
        let deserialized: EventPriority = serde_json::from_str(&json).unwrap();
        assert_eq!(priority, deserialized);
    }
}