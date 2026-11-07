mod herdr;
mod picker;
mod session;
mod store;
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
    let view = session.view()?;
    let mut picker = Picker::new(view.items, focus.as_deref(), &view.open_sections);
    picker.set_undoable(view.undoable);

    let mut terminal = ratatui::init();
    let started = Instant::now();
    let mut refreshed = Instant::now();
    let outcome = loop {
        let tick = usize::try_from(started.elapsed().as_millis() / 125).unwrap_or(0);
        if let Err(error) = terminal.draw(|frame| ui::draw(frame, &mut picker, tick, &pin_icon)) {
            break Err(error);
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => {}
            Ok(false) => {
                if refreshed.elapsed() >= Duration::from_secs(1) {
                    refreshed = Instant::now();
                    show(&mut picker, session.view());
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
            Effect::Close => break Ok(None),
            Effect::Focus(pane) => break Ok(Some(pane)),
            Effect::Rename { key, title } => show(&mut picker, session.rename(&key, title)),
            Effect::ToggleSection(section) => show(&mut picker, session.toggle_section(section)),
            Effect::Apply { key, action } => {
                let from = picker.selected_row().unwrap_or(0);
                match session.act(&key, action) {
                    Ok(Outcome::Reopened) => break Ok(None),
                    Ok(Outcome::Refused(refusal)) => {
                        show(&mut picker, session.view());
                        picker.notify(refusal);
                    }
                    Ok(Outcome::Done {
                        key,
                        notice,
                        wake_failed,
                    }) => {
                        show(&mut picker, session.view());
                        if let Some(notice) = notice {
                            picker.inform(notice);
                        }
                        if matches!(action, Action::ToggleSettle | Action::Snooze(_)) {
                            picker.move_on_from(&key, from);
                        }
                        if let Some(error) = wake_failed {
                            picker.notify(format!(
                                "snooze saved, but its wake timer failed: {error}"
                            ));
                        }
                    }
                    Err(error) => picker.notify(format!("herdr: {error}")),
                }
                refreshed = Instant::now();
            }
            Effect::Undo => {
                match session.undo() {
                    Ok(Outcome::Reopened) => break Ok(None),
                    Ok(Outcome::Refused(refusal)) => {
                        show(&mut picker, session.view());
                        picker.notify(refusal);
                    }
                    Ok(Outcome::Done {
                        notice,
                        wake_failed,
                        ..
                    }) => {
                        show(&mut picker, session.view());
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
                refreshed = Instant::now();
            }
        }
    };
    ratatui::restore();
    match outcome? {
        Some(pane) => session.focus(&pane),
        None => Ok(()),
    }
}

fn show(picker: &mut Picker, view: io::Result<View>) {
    match view {
        Ok(view) => {
            picker.refresh(view.items, &view.open_sections);
            picker.set_undoable(view.undoable);
        }
        Err(error) => picker.notify(format!("herdr: {error}")),
    }
}
