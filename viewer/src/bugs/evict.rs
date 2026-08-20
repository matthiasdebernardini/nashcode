//! Eviction: what goes when a project is full.
//!
//! A cap on stored events is not optional — a bucket bill grows without one — but
//! *which* events go decides whether the feature is still useful at the cap. Dropping
//! the oldest is the obvious rule and the wrong one: one loop logging ten thousand
//! copies of the same timeout then evicts the entire history of every other issue in
//! the project, and the page a person actually needs is the one that empties.
//!
//! So this is Bugsink's shape, and the two halves of its score are worth stating
//! plainly because neither is obvious on its own:
//!
//! - **Volume.** An event's irrelevance is fixed when it is stored, from how many
//!   events its issue already had: `nonzero_leading_bits(round(r · count · 2))`, where
//!   `nonzero_leading_bits` counts the binary digits up to the last `1`. The more an
//!   issue has, the less the next one adds — but a big issue still keeps more events
//!   than a small one, because the score is a logarithm of the count and not the count.
//! - **Age.** Computed when eviction runs, as `log₄(hours + 1)`. Base four is
//!   Bugsink's empirical choice and the reason it works is worth keeping: it makes one
//!   week going on for one month cost about what a doubling of an issue's volume costs,
//!   so age and volume trade against each other instead of one always winning.
//!
//! The two are added, the most irrelevant go first, and **`keep` rows never go at all**.
//! That last rule is where this parts company with Bugsink, which will eventually evict
//! even its protected events rather than fail to reach its target. Here the first-seen
//! event and every regression trigger are the evidence that the issue page exists to
//! show, and an issue whose first event is gone cannot answer "when did this start".
//! Reaching the cap with nothing left to take is a project that needs a bigger cap, and
//! that is a thing to say out loud rather than to solve by deleting the answer.

use std::sync::Arc;

use object_store::{ObjectStore, ObjectStoreExt};
use rusqlite::params;

use crate::db::{Db, DbResult};

/// How many events one project may keep. The bucket keeps the raw envelopes either
/// way; this is the cap on the per-event objects and the index rows over them.
pub const MAX_STORED_EVENTS: i64 = 10_000;

/// The most one pass will take. Bugsink's number, and its reasoning holds here: a
/// delete costs a millisecond or four on a slow box, so an unbounded pass is a sweep
/// that holds the write lock for a minute. Being a little over the cap for another
/// minute costs nothing.
pub const MAX_BATCH: usize = 500;

/// The smallest useful pass, as a share of the cap. Evicting one event per pass when a
/// project is one over would run the whole sort for a single row, for ever.
const MIN_BATCH_SHARE: i64 = 20;

/// How many events to take, given where the project is.
///
/// Never fewer than the overshoot, so a pass always makes progress; never fewer than a
/// twentieth of the cap, so a project sitting on the line is cleared with headroom
/// instead of one row at a time; never more than [`MAX_BATCH`].
pub fn batch_size(stored: i64, cap: i64) -> usize {
    if stored <= cap {
        return 0;
    }
    let overshoot = stored - cap;
    let floor = (cap / MIN_BATCH_SHARE).max(1);
    // Both operands are positive here, so the conversion cannot fail; the ceiling is
    // what decides the answer either way.
    usize::try_from(overshoot.max(floor)).unwrap_or(MAX_BATCH).min(MAX_BATCH)
}

/// The non-roundness of a number in binary: the digits up to and including the last
/// `1`. `0b100000` is 1, `0b101000` is 3, `0b110001` is 6, and zero is nothing.
pub fn nonzero_leading_bits(value: u64) -> i64 {
    if value == 0 {
        return 0;
    }
    i64::from(u64::BITS - value.leading_zeros() - value.trailing_zeros())
}

/// The irrelevance an event is born with, from how crowded its issue already is.
///
/// Bugsink randomises the multiplier so that a count hovering around one value does not
/// hand out the same score every time — which matters, because a project that fills and
/// is evicted and fills again hovers by construction. This derives the multiplier from
/// the event id instead of from a random source. It breaks the same ties for the same
/// reason (two events at the same count have different ids) and it is reproducible,
/// which is worth a great deal in a function whose output decides what gets deleted:
/// the same event scores the same in a test, on a rerun, and after a reindex.
pub fn item_irrelevance(event_id: &str, stored_in_issue: i64) -> i64 {
    let count = stored_in_issue.max(1) as f64;
    let scaled = (unit(event_id) * count * 2.0).round();
    nonzero_leading_bits(scaled.max(0.0) as u64)
}

/// A number in `[0, 1)` from an event id. FNV-1a and then a finalizer: a few
/// instructions, no dependency, and the one property wanted is that ids which differ
/// slightly do not land together.
///
/// The finalizer is not decoration. FNV-1a's last step multiplies by a prime under
/// 2^41, so a change in the final byte reaches the top bits barely or not at all — and
/// event ids are hex strings that differ in exactly their last byte far more often than
/// chance would suggest. Taking the top bits off raw FNV handed sixty-four consecutive
/// ids the same score, which is the tie the randomisation exists to break. The
/// avalanche step below mixes every bit into every other.
fn unit(event_id: &str) -> f64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in event_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^= hash >> 33;
    // 53 bits is what an f64 holds exactly, so the division is not a lie.
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

/// The irrelevance age adds: `log₄(hours + 1)`, so a brand new event adds nothing.
pub fn age_irrelevance(hours: i64) -> f64 {
    ((hours.max(0) as f64) + 1.0).log(4.0)
}

/// Hours since the epoch. Bugsink's unit, and it is the right one: a minute is finer
/// than any decision here needs and a day is coarser than a burst.
fn epoch_hours(stamp: &str) -> Option<i64> {
    time::OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|value| value.unix_timestamp().div_euclid(3600))
}

/// One event weighed for eviction.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    id: i64,
    event_id: String,
    issue_id: i64,
    object_key: String,
    received_at: String,
    total: f64,
}

/// Take a project back under its cap. Returns how many events went.
///
/// **The row and the object go together.** The row first: an object with no row is
/// invisible and costs only storage, while a row with no object is a dead link on the
/// issue page and an error where a person expected an answer. So a pass interrupted
/// halfway leaks bytes rather than breaking a page.
///
/// **And a tombstone goes with both.** Deleting the row is not enough to make eviction
/// stick, because the reindex path reads `bugs_envelopes` and not `bugs_events`: the
/// payload is still in an envelope object, and re-digesting it would file the event
/// again and move the issue's lifetime counter a second time. The tombstone is what
/// makes the second sighting a duplicate. See [`SCHEMA`].
pub async fn evict(
    db: &Db,
    store: &Arc<dyn ObjectStore>,
    project_id: i64,
    cap: i64,
) -> Result<usize, String> {
    let stored = stored_events(db, project_id).map_err(|error| error.to_string())?;
    let wanted = batch_size(stored, cap);
    if wanted == 0 {
        return Ok(0);
    }

    let now_hours = time::OffsetDateTime::now_utc().unix_timestamp().div_euclid(3600);
    let mut candidates = weigh(db, project_id, now_hours).map_err(|error| error.to_string())?;
    if candidates.is_empty() {
        // Everything left is a first-seen event or a regression trigger. Saying so is
        // the whole point: the cap is too small for this project, and silently deleting
        // the evidence would hide that for ever.
        tracing::warn!(
            project_id,
            stored,
            cap,
            "bugs: over the event cap with nothing evictable left; every remaining event is a \
             first-seen or a regression trigger"
        );
        return Ok(0);
    }

    // Most irrelevant first; among equals, the older one goes. Sorting the whole
    // candidate set rather than walking a falling threshold the way Bugsink does gets
    // the same events in a stricter order, and a project's candidate set is at most a
    // cap's worth of small rows.
    candidates.sort_by(|left, right| {
        right
            .total
            .partial_cmp(&left.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.received_at.cmp(&right.received_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.truncate(wanted);

    retire(db, project_id, &candidates).map_err(|error| error.to_string())?;

    for candidate in &candidates {
        let path = object_store::path::Path::from(candidate.object_key.as_str());
        if let Err(error) = store.delete(&path).await {
            // The row is already gone, so this is a leaked object and nothing more.
            // Failing the pass over it would leave the project over its cap for ever.
            tracing::warn!(%error, key = candidate.object_key, "bugs: cannot delete an evicted event object");
        }
    }

    tracing::info!(project_id, evicted = candidates.len(), stored, "bugs: evicted events over the cap");
    Ok(candidates.len())
}

/// How many events a project has stored right now.
pub fn stored_events(db: &Db, project_id: i64) -> DbResult<i64> {
    db.with(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM bugs_events WHERE project_id = ?1",
            params![project_id],
            |row| row.get(0),
        )
    })
}

/// Every evictable event of a project, scored.
fn weigh(db: &Db, project_id: i64, now_hours: i64) -> DbResult<Vec<Candidate>> {
    db.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT id, event_id, issue_id, object_key, received_at, irrelevance
             FROM bugs_events WHERE project_id = ?1 AND keep = 0",
        )?;
        let rows = statement.query_map(params![project_id], |row| {
            let received_at: String = row.get(4)?;
            let item: i64 = row.get(5)?;
            let hours = epoch_hours(&received_at).map_or(0, |then| now_hours - then);
            Ok(Candidate {
                id: row.get(0)?,
                event_id: row.get(1)?,
                issue_id: row.get(2)?,
                object_key: row.get(3)?,
                total: item as f64 + age_irrelevance(hours),
                received_at,
            })
        })?;
        rows.collect()
    })
}

/// Tombstone the events and drop their index rows, in one transaction. The objects
/// follow.
///
/// The tombstone and the delete have to be the same transaction. A row deleted without
/// its tombstone is a row a reindex puts straight back, with the issue counter moving
/// again — which is the exact failure the tombstones exist to prevent, and it would be
/// reachable by a crash between two transactions.
fn retire(db: &Db, project_id: i64, candidates: &[Candidate]) -> DbResult<usize> {
    let evicted_at = crate::db::now();
    db.with(|conn| {
        let tx = conn.unchecked_transaction()?;
        let mut deleted = 0;
        {
            let mut tombstone = tx.prepare(
                "INSERT OR IGNORE INTO bugs_evicted_events
                   (project_id, event_id, issue_id, evicted_at)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut drop_row = tx.prepare("DELETE FROM bugs_events WHERE id = ?1")?;
            for candidate in candidates {
                tombstone.execute(params![
                    project_id,
                    candidate.event_id,
                    candidate.issue_id,
                    evicted_at,
                ])?;
                deleted += drop_row.execute(params![candidate.id])?;
            }
        }
        tx.commit()?;
        Ok(deleted)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_roundness_counts_the_digits_up_to_the_last_one() {
        // Bugsink's own worked examples, in binary.
        assert_eq!(nonzero_leading_bits(0b10_0000), 1);
        assert_eq!(nonzero_leading_bits(0b10_1000), 3);
        assert_eq!(nonzero_leading_bits(0b11_0001), 6);
        assert_eq!(nonzero_leading_bits(0), 0);
        assert_eq!(nonzero_leading_bits(1), 1);
    }

    #[test]
    fn a_crowded_issue_makes_its_next_event_less_relevant_but_only_logarithmically() {
        // The point of the score: an issue with ten thousand events does not score a
        // thousand times an issue with ten. It scores about ten more, because the score
        // counts binary digits and a thousandfold is ten of those.
        let at = |count: i64| {
            let scores: Vec<i64> =
                (0..400).map(|n| item_irrelevance(&format!("{n:032x}"), count)).collect();
            scores.iter().sum::<i64>() as f64 / scores.len() as f64
        };
        let small = at(10);
        let large = at(10_000);
        let growth = large - small;
        assert!(
            (8.0..12.0).contains(&growth),
            "a thousandfold in volume moved the score by {growth}, not by about ten \
             ({small} to {large})"
        );
    }

    #[test]
    fn the_same_event_always_scores_the_same_and_neighbours_do_not_all_tie() {
        let id = "deadbeef".repeat(4);
        assert_eq!(item_irrelevance(&id, 500), item_irrelevance(&id, 500));

        // Consecutive ids are the hard case and the common one: an id that differs only
        // in its last byte must not score the same as its neighbour, or the tie-break
        // the whole randomisation exists for does not happen.
        let spread: std::collections::BTreeSet<i64> =
            (0..64).map(|n| item_irrelevance(&format!("{n:032x}"), 1000)).collect();
        assert!(spread.len() > 3, "sixty-four neighbouring ids scored {spread:?}");
    }

    #[test]
    fn age_costs_a_logarithm_of_hours_and_a_new_event_costs_nothing() {
        assert_eq!(age_irrelevance(0), 0.0);
        assert!((age_irrelevance(3) - 1.0).abs() < 1e-9, "four hours is one rung");
        // A year old is about six and a half, which is Bugsink's stated calibration —
        // comparable to a busy issue's volume score, so a year-old event that was the
        // most relevant one still survives.
        let year = age_irrelevance(24 * 365);
        assert!((6.0..7.0).contains(&year), "{year}");
    }

    #[test]
    fn a_pass_is_bounded_at_both_ends() {
        assert_eq!(batch_size(100, 10_000), 0, "under the cap is nothing to do");
        assert_eq!(batch_size(10_000, 10_000), 0, "at the cap is not over it");
        // One over the cap still clears a useful slice rather than one row.
        assert_eq!(batch_size(10_001, 10_000), 500);
        // A huge overshoot is capped, so one pass never holds the write lock for ever.
        assert_eq!(batch_size(1_000_000, 10_000), MAX_BATCH);
        // A tiny cap keeps the floor at least one.
        assert_eq!(batch_size(11, 10), 1);
    }
}
