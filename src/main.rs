mod herdr;
mod picker;
mod session;
mod store;
mod t3;
mod triage;
mod ui;
mod wakers;

#[cfg(test)]
#[path = "../tests/support/fake_herdr.rs"]
mod fake_herdr;

use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use chrono::Local;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use serde_json::Value;

use herdr::Herdr;
use picker::{Effect, Picker};
use session::{Outcome, Session, View};
use triage::Action;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("sync") => Session::connect().and_then(|mut session| session.sync()),
        Some("startup") => Session::connect().and_then(|mut session| session.startup()),
        Some("next") => Session::connect()
            .and_then(|mut session| session.next(invoked_from(&invocation_context()))),
        Some("open") => Herdr::connect()
            .and_then(|mut herdr| herdr.open_picker(invoked_from(&invocation_context()))),
        Some("picker") => run_picker(),
        Some("wake-at") => match args.get(1).and_then(|at| at.parse().ok()) {
            Some(until) => wakers::wake_at(until),
            None => Err(io::Error::other("wake-at needs a unix time")),
        },
        _ => Err(io::Error::other(
            "usage: herdr-triage <open|sync|startup|next|wake-at TIME>",
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("herdr-triage: {error}");
            ExitCode::FAILURE
        }
    }
}

fn invocation_context() -> Value {
    std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn invoked_from(context: &Value) -> Option<&str> {
    context["focused_pane_id"].as_str()
}

fn run_picker() -> io::Result<()> {
    let mut session = Session::connect()?;
    let focus = std::env::var("HERDR_TRIAGE_FOCUS").ok();
    let pin_icon = session::pin_icon();
    let mut shown = Shown {
        herdr: session.view()?,
        t3: None,
    };
    let t3 = t3::Worker::start(&store::state_root());
    let mut t3_sent = Vec::new();
    let mut picker = Picker::new(shown.items(), focus.as_deref(), &shown.herdr.open_sections);
    picker.set_undoable(shown.herdr.undoable);

    let mut terminal = ratatui::init();
    let started = Instant::now();
    let mut refreshed = Instant::now();
    let outcome = loop {
        for update in t3.updates.try_iter() {
            match update {
                t3::Update::Threads(snapshot) => {
                    shown.t3 = snapshot;
                    picker.refresh(shown.items(), &shown.herdr.open_sections);
                }
                t3::Update::Done { op, result, .. } => {
                    let sent = (!t3_sent.is_empty()).then(|| t3_sent.remove(0));
                    t3_done(&mut picker, op, result, sent.as_ref());
                }
            }
        }
        let tick = usize::try_from(started.elapsed().as_millis() / 125).unwrap_or(0);
        if let Err(error) = terminal.draw(|frame| ui::draw(frame, &mut picker, tick, &pin_icon)) {
            break Err(error);
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => {}
            Ok(false) => {
                if refreshed.elapsed() >= Duration::from_secs(1) {
                    refreshed = Instant::now();
                    show(&mut picker, &mut shown, session.view());
                }
                continue;
            }
            Err(error) => break Err(error),
        }
        let key = match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => key,
            Ok(_) => continue,
            Err(error) => break Err(error),
        };
        match picker.key(key, Local::now().timestamp(), &Local) {
            Effect::None => {}
            Effect::Close => break Ok(Leave::Close),
            Effect::Focus(pane) => break Ok(Leave::Focus(pane)),
            Effect::OpenT3(link) => break Ok(Leave::OpenT3(link)),
            Effect::T3 { thread, op } => {
                if t3.request(&thread, op) {
                    t3_sent.push(picker.sent(&t3::row_key(&thread)));
                    picker.inform("asking T3…".into());
                } else {
                    picker.notify("T3 isn't installed here".into());
                }
            }
            Effect::Rename { key, title } => {
                show(&mut picker, &mut shown, session.rename(&key, title));
            }
            Effect::ToggleSection(section) => {
                show(&mut picker, &mut shown, session.toggle_section(section));
            }
            Effect::Apply { key, action } => {
                let leave = apply(&mut session, &mut picker, &mut shown, &key, action);
                refreshed = Instant::now();
                if let Some(leave) = leave {
                    break Ok(leave);
                }
            }
            Effect::Undo => {
                let leave = undo(&mut session, &mut picker, &mut shown);
                refreshed = Instant::now();
                if let Some(leave) = leave {
                    break Ok(leave);
                }
            }
        }
    };
    ratatui::restore();
    match outcome? {
        Leave::Close => Ok(()),
        Leave::Focus(pane) => session.focus(&pane),
        Leave::OpenT3(link) => {
            t3::open(&link).or_else(|error| session.notify(&format!("Could not open T3: {error}")))
        }
    }
}

/// Shows how a T3 command went; a settle or snooze T3 made moves the cursor on, as for Herdr's.
fn t3_done(
    picker: &mut Picker,
    op: t3::Op,
    result: Result<t3::Done, String>,
    sent: Option<&picker::Sent>,
) {
    match result {
        Ok(t3::Done::Applied) => {
            picker.inform("done in T3".into());
            if let Some(sent) =
                sent.filter(|_| matches!(op, t3::Op::Settle | t3::Op::Snooze { .. }))
            {
                picker.move_on_after(sent);
            }
        }
        Ok(t3::Done::AtTurnEnd) => picker.inform("settles in T3 when its turn ends".into()),
        Err(error) => picker.notify(error),
    }
}

fn apply(
    session: &mut Session,
    picker: &mut Picker,
    shown: &mut Shown,
    key: &str,
    action: Action,
) -> Option<Leave> {
    let from = picker.selected_row().unwrap_or(0);
    match session.act(key, action) {
        Ok(Outcome::Reopened) => return Some(Leave::Close),
        Ok(Outcome::Refused(refusal)) => {
            show(picker, shown, session.view());
            picker.notify(refusal);
        }
        Ok(Outcome::Done {
            key,
            notice,
            wake_failed,
        }) => {
            show(picker, shown, session.view());
            if let Some(notice) = notice {
                picker.inform(notice);
            }
            if matches!(action, Action::ToggleSettle | Action::Snooze(_)) {
                picker.move_on_from(&key, from);
            }
            if let Some(error) = wake_failed {
                picker.notify(format!("snooze saved, but its wake timer failed: {error}"));
            }
        }
        Err(error) => picker.notify(format!("herdr: {error}")),
    }
    None
}

fn undo(session: &mut Session, picker: &mut Picker, shown: &mut Shown) -> Option<Leave> {
    match session.undo() {
        Ok(Outcome::Reopened) => return Some(Leave::Close),
        Ok(Outcome::Refused(refusal)) => {
            show(picker, shown, session.view());
            picker.notify(refusal);
        }
        Ok(Outcome::Done {
            notice,
            wake_failed,
            ..
        }) => {
            show(picker, shown, session.view());
            if let Some(notice) = notice {
                picker.inform(notice);
            }
            picker.inform("undone".into());
            if let Some(error) = wake_failed {
                picker.notify(format!(
                    "snooze restored, but its wake timer failed: {error}"
                ));
            }
        }
        Err(error) => picker.notify(format!("herdr: {error}")),
    }
    None
}

enum Leave {
    Close,
    Focus(String),
    OpenT3(String),
}

struct Shown {
    herdr: View,
    t3: Option<t3::Snapshot>,
}

impl Shown {
    fn items(&self) -> Vec<picker::Item> {
        let t3 = self
            .t3
            .as_ref()
            .map(|snapshot| t3::items(snapshot, Local::now().timestamp()))
            .unwrap_or_default();
        picker::combine(self.herdr.items.clone(), t3)
    }
}

fn show(picker: &mut Picker, shown: &mut Shown, view: io::Result<View>) {
    match view {
        Ok(view) => {
            shown.herdr = view;
            picker.refresh(shown.items(), &shown.herdr.open_sections);
            picker.set_undoable(shown.herdr.undoable);
        }
        Err(error) => picker.notify(format!("herdr: {error}")),
    }
}
