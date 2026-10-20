//! The calendar event a recording belongs to (EventKit): its title and who was invited.

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_event_kit::{EKAuthorizationStatus, EKEntityType, EKEvent, EKEventStore, EKParticipantType};
use objc2_foundation::{NSDate, NSError};
use std::{sync::mpsc, time::Duration};

pub struct Event {
    pub title: String,
    /// Invited people, without the calendar's owner.
    pub attendees: Vec<String>,
}

/// The timed event overlapping `start..end` (unix seconds) the most; None without calendar access.
/// Inside merkwerk.app the first call asks for access and waits up to a minute for the answer;
/// elsewhere (a terminal) it never asks, since the terminal doesn't declare calendar usage.
pub fn event_at(start: f64, end: f64) -> Option<Event> {
    match unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Event) } {
        EKAuthorizationStatus::FullAccess => {}
        EKAuthorizationStatus::NotDetermined if in_app_bundle() => {
            let (tx, rx) = mpsc::channel();
            let done = RcBlock::new(move |granted: Bool, _: *mut NSError| {
                let _ = tx.send(granted.as_bool());
            });
            unsafe { EKEventStore::new().requestFullAccessToEventsWithCompletion(RcBlock::as_ptr(&done)) };
            if !rx.recv_timeout(Duration::from_secs(60)).unwrap_or(false) {
                return None;
            }
        }
        _ => return None,
    }
    // A store made before access was granted stays empty, hence a fresh one.
    let store = unsafe { EKEventStore::new() };
    let events = unsafe {
        let pred = store.predicateForEventsWithStartDate_endDate_calendars(
            &NSDate::dateWithTimeIntervalSince1970(start),
            &NSDate::dateWithTimeIntervalSince1970(end),
            None,
        );
        store.eventsMatchingPredicate(&pred)
    };
    let overlap = |e: &EKEvent| unsafe {
        e.endDate().timeIntervalSince1970().min(end) - e.startDate().timeIntervalSince1970().max(start)
    };
    let attendees = |e: &EKEvent| -> Vec<String> {
        unsafe { e.attendees() }
            .map(|all| {
                all.iter()
                    .filter(|p| unsafe {
                        !p.isCurrentUser()
                            && matches!(
                                p.participantType(),
                                EKParticipantType::Person | EKParticipantType::Unknown
                            )
                    })
                    .filter_map(|p| unsafe { p.name() }.map(|n| n.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    // Only events with someone else invited: a focus block or reminder overlapping a call is not
    // the call.
    let (best, attendees) = events
        .iter()
        .filter(|e| !unsafe { e.isAllDay() })
        .map(|e| {
            let a = attendees(&e);
            (e, a)
        })
        .filter(|(_, a)| !a.is_empty())
        .max_by(|a, b| overlap(&a.0).total_cmp(&overlap(&b.0)))?;
    Some(Event {
        title: unsafe { best.title() }.to_string(),
        attendees,
    })
}

fn in_app_bundle() -> bool {
    std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
}
