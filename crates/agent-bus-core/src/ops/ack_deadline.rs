//! Ack deadline tracking operations wrapping [`crate::redis_bus`].
//!
//! When a message is posted with `request_ack = true`, a deadline record is
//! stored in Redis.  The ack window is priority-dependent (60/120/300 s), and
//! the record is retained for [`redis_bus::ACK_DEADLINE_RETENTION_SECS`] past
//! the deadline so a missed ack stays listable (#166).  This module provides
//! the ops-layer interface for creating, clearing, and querying those
//! deadlines.

use crate::error::Result;
use chrono::Utc;

use crate::models::{AckDeadline, ack_deadline_seconds};
use crate::redis_bus;
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Create deadline
// ---------------------------------------------------------------------------

/// Store an ack deadline for a message that has `request_ack = true`.
///
/// The deadline timestamp and ack window are computed from the message
/// priority; the stored record outlives the deadline (see
/// [`redis_bus::ack_deadline_key_ttl`]).
///
/// # Errors
///
/// Returns an error if the Redis connection or SET EX command fails.
pub fn store_ack_deadline(
    conn: &mut redis::Connection,
    message_id: &str,
    recipient: &str,
    priority: &str,
) -> Result<AckDeadline> {
    let ttl = ack_deadline_seconds(priority);
    let deadline_at = Utc::now() + chrono::Duration::seconds(i64::try_from(ttl).unwrap_or(300));

    let deadline = AckDeadline {
        message_id: message_id.to_owned(),
        recipient: recipient.to_owned(),
        deadline_at: deadline_at.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
        escalation_level: 0,
        escalated_to: None,
    };

    redis_bus::track_ack_deadline(conn, &deadline, ttl)?;
    Ok(deadline)
}

/// Remove an ack deadline (called when the ack is received).
///
/// # Errors
///
/// Returns an error if the Redis DEL command fails.
pub fn clear_deadline(conn: &mut redis::Connection, message_id: &str) -> Result<()> {
    redis_bus::clear_ack_deadline(conn, message_id)
}

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

/// List all ack deadlines that are past their `deadline_at` timestamp.
///
/// A record stays listed from its deadline until it is acked (which clears it)
/// or [`redis_bus::ACK_DEADLINE_RETENTION_SECS`] elapse; after that the key has
/// expired and the record is gone.
///
/// # Errors
///
/// Returns an error if the Redis connection fails.
pub fn check_overdue_acks(settings: &Settings) -> Result<Vec<AckDeadline>> {
    let mut conn = redis_bus::connect(settings)?;
    redis_bus::check_overdue_acks(&mut conn)
}

/// List all outstanding ack deadlines (both overdue and not yet due).
///
/// # Errors
///
/// Returns an error if the Redis connection fails.
pub fn list_ack_deadlines(settings: &Settings) -> Result<Vec<AckDeadline>> {
    let mut conn = redis_bus::connect(settings)?;
    redis_bus::list_ack_deadlines(&mut conn)
}
