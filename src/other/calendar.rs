//! Calendar lookup is macOS only (EventKit); elsewhere meetings keep their default title.
// ponytail: no calendar on Windows/Linux; an ICS URL setting could fill this in.

pub struct Event {
    pub title: String,
    pub attendees: Vec<String>,
}

pub fn event_at(_start: f64, _end: f64) -> Option<Event> {
    None
}
