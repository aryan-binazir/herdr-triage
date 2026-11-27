mod herdr;
mod mcp;
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Local;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use serde_json::Value;
use signal_hook::consts::{SIGHUP, SIGTERM};

use herdr::Herdr;
use picker::{Effect, Picker};
use session::{Outcome, Session, View};
use triage::Action;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("sync") => {
            mcp::link_for_clients();
            Session::connect().and_then(|mut session| session.sync())
        }
        Some("startup") => {
            mcp::link_for_clients();
            Session::connect().and_then(|mut session| session.startup())
        }
        Some("next") => Session::connect()
            .and_then(|mut session| session.next(invoked_from(&invocation_context()))),
        Some("open") => Herdr::connect()
            .and_then(|mut herdr| herdr.open_picker(invoked_from(&invocation_context()))),
        Some("picker") => run_picker(),
        Some("mcp") if args.get(1).is_some_and(|arg| arg == "connect") => mcp::connect(),
        Some("mcp") => mcp::serve(
            mcp::Env::of_process(),
            io::stdin().lock(),
            io::stdout().lock(),
        ),
        Some("wake-at") => match args.get(1).and_then(|at| at.parse().ok()) {
            Some(until) => wakers::wake_at(until),
            None => Err(io::Error::other("wake-at needs a unix time")),
        },
        _ => Err(io::Error::other(
            "usage: herdr-triage <open|sync|startup|next|mcp [connect]|wake-at TIME>",
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
    // Herdr hangs up a closing pane before it kills it: finish the action in hand, then leave.
    let hung_up = Arc::new(AtomicBool::new(false));
    for signal in [SIGHUP, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&hung_up))?;
    }
    let mut session = Session::connect()?;
    let focus = std::env::var("HERDR_TRIAGE_FOCUS").ok();
    let pin_icon = session::pin_icon();
    let mut shown = Shown {
        herdr: session.view()?,
        t3: None,
    };
    let t3 = t3::Worker::start(&store::state_root());
    let mut picker = Picker::new(shown.items(), focus.as_deref(), &shown.herdr.open_sections);
    picker.set_undo(shown.herdr.undo.clone());

    let mut terminal = ratatui::init();
    let started = Instant::now();
    let mut refreshed = Instant::now();
    let outcome = loop {
        if hung_up.load(Ordering::Relaxed) {
            break Ok(Leave::Close);
        }
        for update in t3.updates.try_iter() {
            match update {
                t3::Update::Threads(snapshot) => {
                    shown.t3 = snapshot;
                    picker.refresh(shown.items(), &shown.herdr.open_sections);
                }
                t3::Update::Done { op, result, .. } => picker.t3_done(op, result),
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
            Effect::NewAgent(agent) => break Ok(Leave::NewAgent(agent)),
            Effect::T3 { thread, op } => {
                if t3.request(&thread, op) {
                    picker.asked_t3(&thread);
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
                let leave = act(&mut session, &mut picker, &mut shown, Some((&key, action)));
                refreshed = Instant::now();
                if let Some(leave) = leave {
                    break Ok(leave);
                }
            }
            Effect::Undo => {
                let leave = act(&mut session, &mut picker, &mut shown, None);
                refreshed = Instant::now();
                if let Some(leave) = leave {
                    break Ok(leave);
                }
            }
        }
    };
    ratatui::restore();
    leave(&mut session, outcome?)
}

fn leave(session: &mut Session, leave: Leave) -> io::Result<()> {
    match leave {
        Leave::Close => Ok(()),
        Leave::Focus(pane) => session.focus(&pane),
        Leave::OpenT3(link) => {
            t3::open(&link).or_else(|error| session.notify(&format!("Could not open T3: {error}")))
        }
        Leave::NewAgent(agent) => session.start_agent(&agent).or_else(|error| {
            session.notify(&format!("Could not start {}: {error}", agent.command))
        }),
    }
}

/// Applies `action` to the thread shown as `key`, or undoes the last change when `None`.
fn act(
    session: &mut Session,
    picker: &mut Picker,
    shown: &mut Shown,
    action: Option<(&str, Action)>,
) -> Option<Leave> {
    let from = picker.selected_row().unwrap_or(0);
    let outcome = match action {
        Some((key, action)) => session.act(key, action),
        None => session.undo(),
    };
    match outcome {
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
            if let Some(notice) = notice {
                picker.inform(notice);
            }
            show(picker, shown, session.view());
            let snooze = if let Some((_, action)) = action {
                if matches!(
                    action,
                    Action::ToggleSettle
                        | Action::SettleKeepingOpen
                        | Action::CloseForGood { .. }
                        | Action::Snooze(_)
                ) {
                    picker.move_on_from(&key, from);
                }
                "saved"
            } else {
                picker.inform("undone".into());
                "restored"
            };
            if let Some(error) = wake_failed {
                picker.notify(format!(
                    "snooze {snooze}, but its wake timer failed: {error}"
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
    NewAgent(picker::NewAgent),
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
            picker.set_undo(shown.herdr.undo.clone());
        }
        Err(error) => picker.notify(format!("herdr: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use super::*;
    use crate::fake_herdr::{self, FakeHerdr};
    use crate::store::Store;

    const NOW: i64 = 1_800_000_000;

    fn picker_on(fake: &FakeHerdr) -> (Session, Picker, Shown) {
        let mut session = Session::new(
            Herdr::at(&fake.socket).unwrap(),
            Store::at(fake.state_dir()),
            || NOW,
            |_| Ok(()),
        );
        let shown = Shown {
            herdr: session.toggle_section(triage::Section::Working).unwrap(),
            t3: None,
        };
        let picker = Picker::new(shown.items(), Some("w1:p1"), &shown.herdr.open_sections);
        (session, picker, shown)
    }

    fn settle_selected(session: &mut Session, picker: &mut Picker, shown: &mut Shown) {
        let press = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        let Effect::Apply { key, action } = picker.key(press, NOW, &chrono::Utc) else {
            panic!("s should settle the selected row");
        };
        assert!(act(session, picker, shown, Some((&key, action))).is_none());
    }

    fn pending(picker: &Picker) -> Vec<(&str, bool)> {
        let items = picker.items().iter();
        items
            .map(|item| (item.row.key.as_str(), item.row.settle_pending))
            .collect()
    }

    #[test]
    fn settling_a_working_agent_herdr_rekeyed_since_the_picker_showed_it_says_so() {
        let fake = FakeHerdr::start(vec![
            fake_herdr::agent("w1:p1", "working", 3),
            fake_herdr::agent("w1:p2", "idle", 4),
        ]);
        let (mut session, mut picker, mut shown) = picker_on(&fake);
        fake.agents.lock().unwrap()[0]["agent_session"] = json!({
            "source": "herdr:claude", "agent": "claude", "kind": "id", "value": "id-p1"
        });
        settle_selected(&mut session, &mut picker, &mut shown);
        let rekeyed = "session:claude:id:id-p1";
        assert!(pending(&picker).contains(&(rekeyed, true)));
        assert_eq!(picker.message(), Some("settles when its turn ends"));

        settle_selected(&mut session, &mut picker, &mut shown);
        assert!(pending(&picker).contains(&(rekeyed, false)));
        assert_eq!(picker.message(), Some("won't settle when its turn ends"));
    }

    #[test]
    fn settling_again_after_a_queued_settle_landed_unsettles_without_claiming_to_call_it_off() {
        let fake = FakeHerdr::start(vec![
            fake_herdr::agent("w1:p1", "working", 3),
            fake_herdr::agent("w1:p2", "idle", 4),
        ]);
        let (mut session, mut picker, mut shown) = picker_on(&fake);
        settle_selected(&mut session, &mut picker, &mut shown);
        assert_eq!(picker.message(), Some("settles when its turn ends"));
        {
            let mut agents = fake.agents.lock().unwrap();
            agents[0]["agent_status"] = json!("done");
            agents[0]["state_change_seq"] = json!(5);
        }
        settle_selected(&mut session, &mut picker, &mut shown);
        let item = picker
            .items()
            .iter()
            .find(|item| item.pane_id() == Some("w1:p1"));
        let row = &item.unwrap().row;
        assert_eq!((row.park, row.settle_pending), (None, false));
        assert_eq!(picker.message(), None);
    }

    #[test]
    fn a_picker_that_could_not_refresh_after_an_action_says_so_over_its_notice() {
        let fake = FakeHerdr::start(vec![
            fake_herdr::agent("w1:p1", "working", 3),
            fake_herdr::agent("w1:p2", "idle", 4),
        ]);
        let (mut session, mut picker, mut shown) = picker_on(&fake);
        *fake.fail_method.lock().unwrap() = Some("tab.list".into());
        settle_selected(&mut session, &mut picker, &mut shown);
        assert!(picker.message().is_some_and(|m| m.starts_with("herdr: ")));
        assert!(picker.message_is_error());
    }
}
