use std::cmp::Reverse;
use std::collections::BTreeSet;

use chrono::TimeZone;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::t3::Op as T3Op;
use crate::triage::{Action, Display, Park, Refusal, Row, Section, SnoozePreset, snooze_presets};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Pane(String),
    Closed,
    T3 { thread: String, link: String },
}

#[derive(Debug, Clone)]
pub struct Item {
    pub row: Row,
    pub target: Target,
    pub location: String,
    pub kind: String,
    pub search: String,
    pub status: String,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Close,
    Focus(String),
    OpenT3(String),
    T3 { thread: String, op: T3Op },
    Apply { key: String, action: Action },
    Rename { key: String, title: Option<String> },
    Undo,
    ToggleSection(Section),
    NewAgent(NewAgent),
}

/// The agents `n` offers, by the digit beside each, and the command that starts it.
pub const NEW_AGENTS: [&str; 4] = ["claude", "codex", "opencode", "pi"];

/// An agent to start in a new tab of `workspace_id` (the current workspace if `None`), or in a
/// new worktree of it on `branch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgent {
    pub workspace_id: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub command: &'static str,
}

#[derive(Debug, Clone)]
pub struct NewAgentPrompt {
    pub workspace_id: Option<String>,
    pub label: String,
    cwd: Option<String>,
    pub branch: Option<String>,
    /// The branch being typed for a new worktree.
    pub typing: Option<String>,
}

const T3_READ: &str = "T3 marks a thread read when you open it";
const T3_ONLY: &str = "rename or forget T3 threads in T3";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Navigate,
    Search,
}

fn without_last_word(text: &str) -> &str {
    let kept = text.trim_end();
    let cut = kept
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(at, c)| at + c.len_utf8());
    &kept[..cut]
}

#[derive(Debug, Clone)]
pub struct RenamePrompt {
    pub key: String,
    pane_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct ForgetPrompt {
    pub key: String,
    pub label: String,
}

/// Asks before a settle closes an agent that has no session to resume.
#[derive(Debug, Clone)]
pub struct ClosePrompt {
    pub key: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct SnoozePrompt {
    pub key: String,
    t3_thread: Option<String>,
    pub label: String,
    /// Each preset and whether it was offered when the prompt opened.
    pub presets: Vec<(SnoozePreset, bool)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Agent(usize),
    Section { section: Section, count: usize },
}

pub enum ListRow<'a> {
    Agent(&'a Item),
    Section {
        section: Section,
        count: usize,
        open: bool,
    },
}

pub struct Picker {
    items: Vec<Item>,
    open_sections: BTreeSet<Section>,
    mode: Mode,
    filter: String,
    entries: Vec<Entry>,
    matched: usize,
    selected: usize,
    snooze: Option<SnoozePrompt>,
    forget: Option<ForgetPrompt>,
    close: Option<ClosePrompt>,
    rename: Option<RenamePrompt>,
    new_agent: Option<NewAgentPrompt>,
    undo: Option<(&'static str, String)>,
    message: Option<String>,
    message_is_error: bool,
    offset: usize,
    rows_on_screen: usize,
    presses: u64,
}

#[derive(Debug)]
pub struct Sent {
    key: String,
    from: usize,
    presses: u64,
}

impl Picker {
    pub fn new(
        items: Vec<Item>,
        focus_pane: Option<&str>,
        open_sections: &BTreeSet<Section>,
    ) -> Self {
        let mut picker = Self {
            items: Vec::new(),
            open_sections: open_sections.clone(),
            mode: Mode::Navigate,
            filter: String::new(),
            entries: Vec::new(),
            matched: 0,
            selected: 0,
            snooze: None,
            forget: None,
            close: None,
            rename: None,
            new_agent: None,
            undo: None,
            message: None,
            message_is_error: false,
            offset: 0,
            rows_on_screen: 10,
            presses: 0,
        };
        let start = focus_pane
            .and_then(|pane| items.iter().position(|item| item.pane_id() == Some(pane)))
            .or_else(|| items.iter().position(|item| item.focused));
        picker.items = items;
        picker.refilter();
        if let Some(start) = start {
            picker.selected = picker.entry_showing(start).unwrap_or(0);
        }
        picker
    }

    fn entry_showing(&self, index: usize) -> Option<usize> {
        self.entries
            .iter()
            .position(|&entry| entry == Entry::Agent(index))
            .or_else(|| {
                let section = self.items[index].section()?;
                self.entries.iter().position(
                    |entry| matches!(entry, Entry::Section { section: s, .. } if *s == section),
                )
            })
    }

    pub fn refresh(&mut self, items: Vec<Item>, open_sections: &BTreeSet<Section>) {
        let selected = self.entries.get(self.selected).map(|&entry| match entry {
            Entry::Agent(i) => Ok(self.items[i].row.key.clone()),
            Entry::Section { section, .. } => Err(section),
        });
        self.items = items;
        // Herdr rekeys a thread once it learns its session; the pane stays the same.
        if let Some(prompt) = &mut self.rename
            && !self.items.iter().any(|item| item.row.key == prompt.key)
            && let Some(item) = self.items.iter().find(|item| {
                item.pane_id().is_some() && item.pane_id() == prompt.pane_id.as_deref()
            })
        {
            prompt.key.clone_from(&item.row.key);
        }
        self.open_sections.clone_from(open_sections);
        self.refilter();
        let at = match selected {
            Some(Ok(key)) => self
                .items
                .iter()
                .position(|item| item.row.key == key)
                .and_then(|i| self.entry_showing(i)),
            Some(Err(section)) => self.entries.iter().position(
                |entry| matches!(entry, Entry::Section { section: s, .. } if *s == section),
            ),
            None => None,
        };
        if let Some(at) = at {
            self.selected = at;
        }
    }

    pub fn key(&mut self, key: KeyEvent, now: i64, tz: &impl TimeZone) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'g')) {
            return Effect::Close;
        }
        self.presses += 1;
        if !self.prompt_open() {
            self.message = None;
        }
        if self.rename.is_some() {
            return self.rename_key(key, ctrl);
        }
        if self.new_agent.is_some() {
            return self.new_agent_key(key, ctrl);
        }
        if let Some(prompt) = self.close.take() {
            let action = match key.code {
                KeyCode::Char('y') => Action::CloseForGood,
                KeyCode::Char('n') => Action::ToggleSettle,
                _ => return Effect::None,
            };
            return Effect::Apply {
                key: prompt.key,
                action,
            };
        }
        if let Some(prompt) = self.forget.take() {
            return match key.code {
                KeyCode::Char('y') => Effect::Apply {
                    key: prompt.key,
                    action: Action::Forget,
                },
                _ => Effect::None,
            };
        }
        if let Some(prompt) = &self.snooze {
            let choice = match key.code {
                KeyCode::Char(c @ '1'..='9') => c as usize - '1' as usize,
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.snooze = None;
                    return Effect::None;
                }
                _ => return Effect::None,
            };
            let Some(&(preset, offered)) = prompt.presets.get(choice) else {
                return Effect::None;
            };
            let until = preset.until(now, tz);
            if !offered || until <= now {
                self.snooze = None;
                self.notify(format!("{} is too soon", preset.label()));
                return Effect::None;
            }
            let effect = match &prompt.t3_thread {
                Some(thread) => Effect::T3 {
                    thread: thread.clone(),
                    op: T3Op::Snooze { until },
                },
                None => Effect::Apply {
                    key: prompt.key.clone(),
                    action: Action::Snooze(until),
                },
            };
            self.snooze = None;
            return effect;
        }
        if key.code == KeyCode::Enter {
            return match self.entries.get(self.selected) {
                Some(Entry::Agent(i)) => self.open(*i),
                Some(Entry::Section { .. }) if !self.filter.is_empty() => Effect::None,
                Some(Entry::Section { section, .. }) => Effect::ToggleSection(*section),
                None => Effect::None,
            };
        }
        match self.mode {
            Mode::Navigate => self.navigate_key(key, ctrl, now, tz),
            Mode::Search => {
                self.search_key(key, ctrl);
                Effect::None
            }
        }
    }

    fn navigate_key(&mut self, key: KeyEvent, ctrl: bool, now: i64, tz: &impl TimeZone) -> Effect {
        if let Some(effect) = self.t3_key(key, ctrl, now, tz) {
            return effect;
        }
        let apply = |picker: &Self, action| {
            picker
                .selected()
                .map_or(Effect::None, |item| Effect::Apply {
                    key: item.row.key.clone(),
                    action,
                })
        };
        match key.code {
            KeyCode::Esc if !self.filter.is_empty() => self.set_filter(String::new()),
            KeyCode::Esc | KeyCode::Char('q') => return Effect::Close,
            KeyCode::Char('n' | 'j') if ctrl || key.code == KeyCode::Char('j') => self.step(1),
            KeyCode::Char('p' | 'k') if ctrl || key.code == KeyCode::Char('k') => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Up => self.step(-1),
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Char(c @ '0'..='9') if !ctrl => {
                let row = (c as usize - '0' as usize + 9) % 10;
                return self
                    .agents_on_screen()
                    .get(row)
                    .map_or(Effect::None, |&i| self.open(i));
            }
            KeyCode::Char('s') => {
                if let Some(item) = self.selected()
                    && item.row.can_close_for_good()
                {
                    self.close = Some(ClosePrompt {
                        key: item.row.key.clone(),
                        label: item.row.label().to_owned(),
                    });
                    return Effect::None;
                }
                return apply(self, Action::ToggleSettle);
            }
            KeyCode::Char('u') => return apply(self, Action::ToggleUnread),
            KeyCode::Char('p') => return apply(self, Action::TogglePin),
            KeyCode::Char('U') => return Effect::Undo,
            KeyCode::Char('r') => {
                if let Some(item) = self.selected() {
                    self.rename = Some(RenamePrompt {
                        key: item.row.key.clone(),
                        pane_id: item.pane_id().map(str::to_owned),
                        text: item.row.label().to_owned(),
                    });
                }
            }
            KeyCode::Char('n') => self.new_agent = Some(self.prompt_for_new_agent()),
            KeyCode::Char('D') => {
                let Some(item) = self.selected() else {
                    return Effect::None;
                };
                match item.row.refusal(Action::Forget) {
                    Some(Refusal(why)) => self.notify(why),
                    None => {
                        self.forget = Some(ForgetPrompt {
                            key: item.row.key.clone(),
                            label: item.row.label().to_owned(),
                        });
                    }
                }
            }
            KeyCode::Char('z') => {
                let Some(item) = self.selected() else {
                    return Effect::None;
                };
                if matches!(item.row.park, Some(Park::Snoozed { .. })) {
                    return apply(self, Action::Wake);
                }
                if let Some(Refusal(why)) = item.row.refusal(Action::Snooze(now)) {
                    self.notify(why);
                    return Effect::None;
                }
                self.snooze = Some(SnoozePrompt {
                    key: item.row.key.clone(),
                    t3_thread: None,
                    label: item.row.label().to_owned(),
                    presets: snooze_presets(now, tz),
                });
            }
            _ => {}
        }
        Effect::None
    }

    fn t3_key(
        &mut self,
        key: KeyEvent,
        ctrl: bool,
        now: i64,
        tz: &impl TimeZone,
    ) -> Option<Effect> {
        let item = self.selected()?;
        let Target::T3 { thread, .. } = &item.target else {
            return None;
        };
        let thread = thread.clone();
        let parked = item.row.park;
        let op = match key.code {
            KeyCode::Char('s') if parked == Some(Park::Settled) => T3Op::Unsettle,
            KeyCode::Char('s') => T3Op::Settle,
            KeyCode::Char('z') if matches!(parked, Some(Park::Snoozed { .. })) => T3Op::Unsnooze,
            KeyCode::Char('z') => {
                self.snooze = Some(SnoozePrompt {
                    key: item.row.key.clone(),
                    t3_thread: Some(thread),
                    label: item.row.label().to_owned(),
                    presets: snooze_presets(now, tz),
                });
                return Some(Effect::None);
            }
            KeyCode::Char('p') if !ctrl && item.row.pinned => T3Op::Unpin,
            KeyCode::Char('p') if !ctrl => T3Op::Pin,
            KeyCode::Char('u') if item.row.display.unseen() => {
                self.notify(T3_READ.into());
                return Some(Effect::None);
            }
            KeyCode::Char('u') => T3Op::MarkUnread,
            KeyCode::Char('r' | 'D') => {
                self.notify(T3_ONLY.into());
                return Some(Effect::None);
            }
            _ => return None,
        };
        Some(Effect::T3 { thread, op })
    }

    fn search_key(&mut self, key: KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Navigate,
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Char('p' | 'k') if ctrl => self.step(-1),
            KeyCode::Char('n' | 'j') if ctrl => self.step(1),
            KeyCode::Char('u') if ctrl => self.set_filter(String::new()),
            KeyCode::Char('w') if ctrl => {
                self.set_filter(without_last_word(&self.filter).to_owned());
            }
            KeyCode::Backspace => {
                let mut filter = self.filter.clone();
                filter.pop();
                self.set_filter(filter);
            }
            KeyCode::Char(c) if !ctrl => {
                let filter = format!("{}{c}", self.filter);
                self.set_filter(filter);
            }
            _ => {}
        }
    }

    fn open(&self, index: usize) -> Effect {
        let item = &self.items[index];
        match &item.target {
            Target::Pane(pane) => Effect::Focus(pane.clone()),
            Target::Closed => Effect::Apply {
                key: item.row.key.clone(),
                action: Action::Reopen,
            },
            Target::T3 { link, .. } => Effect::OpenT3(link.clone()),
        }
    }

    pub fn notify(&mut self, message: String) {
        self.message = Some(message);
        self.message_is_error = true;
    }

    /// Shows a notice that is not an error.
    pub fn inform(&mut self, message: String) {
        self.message = Some(message);
        self.message_is_error = false;
    }

    fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        let best = self.refilter();
        self.selected = best
            .and_then(|i| self.entries.iter().position(|&e| e == Entry::Agent(i)))
            .unwrap_or(0);
    }

    /// Rebuilds the rows and returns the best match of a non-empty search.
    fn refilter(&mut self) -> Option<usize> {
        let matches: Vec<usize> = if self.filter.is_empty() {
            (0..self.items.len()).collect()
        } else {
            let mut scored: Vec<(i64, usize)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| fuzzy_score(&self.filter, &item.search).map(|s| (s, i)))
                .collect();
            scored.sort_by_key(|&(score, i)| (std::cmp::Reverse(score), i));
            scored.into_iter().map(|(_, i)| i).collect()
        };
        self.matched = matches.len();
        let items = &self.items;
        let in_section = |section: Option<Section>| -> Vec<usize> {
            matches
                .iter()
                .copied()
                .filter(|&i| items[i].section() == section)
                .collect()
        };
        let mut entries: Vec<Entry> = in_section(None).into_iter().map(Entry::Agent).collect();
        for section in [Section::Working, Section::Snoozed, Section::Settled] {
            let members = in_section(Some(section));
            if members.is_empty() {
                continue;
            }
            entries.push(Entry::Section {
                section,
                count: members.len(),
            });
            if !self.filter.is_empty() || self.open_sections.contains(&section) {
                entries.extend(members.into_iter().map(Entry::Agent));
            }
        }
        self.entries = entries;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        matches.first().copied().filter(|_| !self.filter.is_empty())
    }

    fn agents_on_screen(&self) -> Vec<usize> {
        self.entries
            .iter()
            .skip(self.offset)
            .take(self.rows_on_screen)
            .filter_map(|entry| match entry {
                Entry::Agent(i) => Some(*i),
                Entry::Section { .. } => None,
            })
            .collect()
    }

    fn step(&mut self, delta: isize) {
        let len = self.entries.len();
        if len > 0 {
            self.selected = (self.selected.cast_signed() + delta)
                .rem_euclid(len.cast_signed())
                .cast_unsigned();
        }
    }

    pub fn selected(&self) -> Option<&Item> {
        match self.entries.get(self.selected)? {
            Entry::Agent(i) => Some(&self.items[*i]),
            Entry::Section { .. } => None,
        }
    }

    pub fn set_viewport(&mut self, offset: usize, rows: usize) {
        self.offset = offset;
        self.rows_on_screen = rows;
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    /// After `key` was parked, selects the first live row that needs you most, else stays at
    /// the row it left, `from`.
    pub fn sent(&self, key: &str) -> Sent {
        Sent {
            key: key.to_owned(),
            from: self.selected_row().unwrap_or(0),
            presses: self.presses,
        }
    }

    pub fn move_on_after(&mut self, sent: &Sent) {
        if self.presses == sent.presses {
            self.move_on_from(&sent.key, sent.from);
        }
    }

    fn prompt_open(&self) -> bool {
        self.snooze.is_some()
            || self.rename.is_some()
            || self.forget.is_some()
            || self.close.is_some()
            || self.new_agent.is_some()
    }

    pub fn move_on_from(&mut self, key: &str, from: usize) {
        let still_listed_unparked = self
            .items
            .iter()
            .any(|item| item.row.key == key && item.row.park.is_none());
        if still_listed_unparked {
            return;
        }
        let needs_you = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(at, &entry)| match entry {
                Entry::Agent(i) => Some((self.items[i].row.attention()?, at)),
                Entry::Section { .. } => None,
            })
            .min();
        self.selected = needs_you.map_or_else(
            || from.min(self.entries.len().saturating_sub(1)),
            |(_, at)| at,
        );
    }

    pub fn selected_row(&self) -> Option<usize> {
        (!self.entries.is_empty()).then_some(self.selected)
    }

    pub fn rows(&self) -> Vec<ListRow<'_>> {
        self.entries
            .iter()
            .map(|&entry| match entry {
                Entry::Agent(i) => ListRow::Agent(&self.items[i]),
                Entry::Section { section, count } => ListRow::Section {
                    section,
                    count,
                    open: !self.filter.is_empty() || self.open_sections.contains(&section),
                },
            })
            .collect()
    }

    pub fn matched(&self) -> usize {
        self.matched
    }

    pub fn total(&self) -> usize {
        self.items.len()
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Starts where the selected thread lives: its Herdr workspace and directory, or the current
    /// workspace for a T3 thread or no selection.
    fn prompt_for_new_agent(&self) -> NewAgentPrompt {
        let item = self
            .selected()
            .filter(|item| !matches!(item.target, Target::T3 { .. }));
        let thread = item.map(|item| &item.row.thread);
        let workspace_id = thread.and_then(|thread| thread.workspace_id.clone());
        let label = match (item, &workspace_id) {
            (Some(item), Some(_)) => {
                (item.row.thread.workspace_label.clone()).unwrap_or_else(|| item.location.clone())
            }
            _ => "this workspace".into(),
        };
        NewAgentPrompt {
            label,
            cwd: thread.and_then(|thread| thread.cwd.clone()),
            workspace_id,
            branch: None,
            typing: None,
        }
    }

    fn new_agent_key(&mut self, key: KeyEvent, ctrl: bool) -> Effect {
        let Some(prompt) = &mut self.new_agent else {
            return Effect::None;
        };
        if let Some(text) = &mut prompt.typing {
            match key.code {
                KeyCode::Enter => {
                    let branch = text.trim().to_owned();
                    prompt.branch = Some(branch).filter(|branch| !branch.is_empty());
                    prompt.typing = None;
                }
                KeyCode::Esc => prompt.typing = None,
                KeyCode::Backspace => drop(text.pop()),
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char(c) if !ctrl && !c.is_whitespace() => text.push(c),
                _ => {}
            }
            return Effect::None;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='9') => {
                let Some(&command) = NEW_AGENTS.get(c as usize - '1' as usize) else {
                    return Effect::None;
                };
                let Some(prompt) = self.new_agent.take() else {
                    return Effect::None;
                };
                Effect::NewAgent(NewAgent {
                    workspace_id: prompt.workspace_id,
                    cwd: prompt.cwd,
                    branch: prompt.branch,
                    command,
                })
            }
            KeyCode::Char('w') => {
                prompt.typing = Some(prompt.branch.clone().unwrap_or_default());
                Effect::None
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.new_agent = None;
                Effect::None
            }
            _ => Effect::None,
        }
    }

    pub fn new_agent_prompt(&self) -> Option<&NewAgentPrompt> {
        self.new_agent.as_ref()
    }

    fn rename_key(&mut self, key: KeyEvent, ctrl: bool) -> Effect {
        let Some(prompt) = &mut self.rename else {
            return Effect::None;
        };
        match key.code {
            KeyCode::Enter => {
                let key = prompt.key.clone();
                let title = Some(prompt.text.trim().to_owned()).filter(|t| !t.is_empty());
                self.rename = None;
                return Effect::Rename { key, title };
            }
            KeyCode::Esc => self.rename = None,
            KeyCode::Backspace => {
                prompt.text.pop();
            }
            KeyCode::Char('u') if ctrl => prompt.text.clear(),
            KeyCode::Char('w') if ctrl => {
                let kept = without_last_word(&prompt.text).len();
                prompt.text.truncate(kept);
            }
            KeyCode::Char(c) if !ctrl => prompt.text.push(c),
            _ => {}
        }
        Effect::None
    }

    pub fn set_undo(&mut self, undo: Option<(&'static str, String)>) {
        self.undo = undo;
    }

    /// What `U` would undo: its verb and thread, e.g. `settle` and `docs pass`.
    pub fn undo(&self) -> Option<(&'static str, &str)> {
        self.undo
            .as_ref()
            .map(|(verb, label)| (*verb, label.as_str()))
    }

    pub fn rename_prompt(&self) -> Option<&RenamePrompt> {
        self.rename.as_ref()
    }

    pub fn forget_prompt(&self) -> Option<&ForgetPrompt> {
        self.forget.as_ref()
    }

    pub fn close_prompt(&self) -> Option<&ClosePrompt> {
        self.close.as_ref()
    }

    pub fn snooze_prompt(&self) -> Option<&SnoozePrompt> {
        self.snooze.as_ref()
    }

    pub fn message_is_error(&self) -> bool {
        self.message_is_error
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

impl Item {
    pub fn pane_id(&self) -> Option<&str> {
        match &self.target {
            Target::Pane(pane) => Some(pane),
            Target::Closed | Target::T3 { .. } => None,
        }
    }

    pub fn section(&self) -> Option<Section> {
        match (&self.target, self.row.park) {
            (Target::T3 { .. }, Some(park)) => Some(park.section()),
            (Target::T3 { .. }, None)
                if self.row.display == Display::Working && !self.row.pinned =>
            {
                Some(Section::Working)
            }
            (Target::T3 { .. }, None) => None,
            _ => self.row.section(),
        }
    }
}

#[cfg(test)]
impl Item {
    pub(crate) fn projected(fixture: &crate::triage::Fixture, now: i64) -> Vec<Item> {
        fixture
            .rows()
            .into_iter()
            .map(|(row, pane_id)| Item {
                target: pane_id.map_or(Target::Closed, Target::Pane),
                location: "acme-api".into(),
                kind: "claude".into(),
                search: row.label().to_owned(),
                status: row.status_text(now),
                focused: false,
                row,
            })
            .collect()
    }
}

/// Merges T3's threads into Herdr's ordered rows by rank, then wake time, then recency, keeping
/// Herdr's own order among its rows.
pub fn combine(herdr: Vec<Item>, mut t3: Vec<Item>) -> Vec<Item> {
    fn place(item: &Item) -> (crate::triage::Rank, i64, Reverse<i64>) {
        let wake = match item.row.park {
            Some(Park::Snoozed { until }) => until,
            _ => 0,
        };
        (
            item.row.rank(),
            wake,
            Reverse(item.row.changed_at.unwrap_or(0)),
        )
    }
    t3.sort_by_key(place);
    let mut items = Vec::with_capacity(herdr.len() + t3.len());
    let mut t3 = t3.into_iter().peekable();
    for row in herdr {
        while let Some(thread) = t3.next_if(|thread| place(thread) < place(&row)) {
            items.push(thread);
        }
        items.push(row);
    }
    items.extend(t3);
    items
}

pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i64> {
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut at = 0usize;
    let mut previous: Option<usize> = None;
    for wanted in needle.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (at..hay.len()).find(|&i| hay[i] == wanted)?;
        score += 1;
        if previous == Some(found.wrapping_sub(1)) {
            score += 2;
        }
        if found == 0 || !hay[found - 1].is_alphanumeric() {
            score += 3;
        }
        previous = Some(found);
        at = found + 1;
    }
    Some(score * 100 - i64::try_from(hay.len()).unwrap_or(i64::MAX / 200))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::triage::{Action, Fixture, Section, Status};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    const NOW: i64 = 1_793_710_800;

    fn listed(fixture: &Fixture) -> Vec<Item> {
        Item::projected(fixture, NOW)
    }

    fn three(first: Status) -> Fixture {
        Fixture::new(
            NOW,
            &[
                ("p1", "api refactor", first),
                ("p2", "fix flaky test", Status::Done),
                ("p3", "terraform plan", Status::Idle),
            ],
        )
        .act("p1", Action::TogglePin)
    }

    fn items() -> Vec<Item> {
        listed(&three(Status::Working))
    }

    fn closed_sections() -> BTreeSet<Section> {
        BTreeSet::new()
    }

    fn shelf() -> Vec<Item> {
        let fixture = Fixture::new(
            NOW,
            &[
                ("p1", "api refactor", Status::Working),
                ("p2", "bench run", Status::Idle),
                ("p3", "release notes", Status::Idle),
                ("p4", "old spike", Status::Idle),
            ],
        )
        .act("p1", Action::TogglePin)
        .act("p2", Action::Snooze(NOW + 60))
        .act("p3", Action::ToggleSettle)
        .act("p4", Action::ToggleSettle);
        listed(&fixture)
    }

    fn with_closed() -> Vec<Item> {
        let fixture = Fixture::new(
            NOW,
            &[
                ("p1", "api refactor", Status::Working),
                ("p2", "old spike", Status::Idle),
            ],
        )
        .act("p1", Action::TogglePin)
        .act("p2", Action::ToggleUnread)
        .close("p2")
        .act("p2", Action::ToggleUnread);
        listed(&fixture)
    }

    fn layout(picker: &Picker) -> Vec<String> {
        picker
            .rows()
            .iter()
            .map(|row| match row {
                ListRow::Agent(item) => item.pane_id().unwrap_or_default().to_owned(),
                ListRow::Section {
                    section,
                    count,
                    open,
                } => format!("{section:?}:{count}:{open}"),
            })
            .collect()
    }

    fn press(picker: &mut Picker, code: KeyCode) -> Effect {
        picker.key(KeyEvent::new(code, KeyModifiers::NONE), NOW, &chrono::Utc)
    }

    #[test]
    fn opens_on_the_agent_it_was_invoked_from() {
        let picker = Picker::new(items(), Some("p3"), &closed_sections());
        assert_eq!(
            picker.selected().map(|i| i.pane_id().unwrap_or_default()),
            Some("p3")
        );
        let picker = Picker::new(items(), None, &closed_sections());
        assert_eq!(
            picker.selected().map(|i| i.pane_id().unwrap_or_default()),
            Some("p1")
        );
    }

    #[test]
    fn digits_open_rows_while_navigating_and_type_while_searching() {
        let mut picker = Picker::new(items(), None, &closed_sections());
        assert_eq!(
            press(&mut picker, KeyCode::Char('2')),
            Effect::Focus("p2".into())
        );
        assert_eq!(press(&mut picker, KeyCode::Char('9')), Effect::None);

        press(&mut picker, KeyCode::Char('/'));
        assert_eq!(press(&mut picker, KeyCode::Char('2')), Effect::None);
        assert_eq!(picker.filter(), "2");
    }

    #[test]
    fn triage_keys_act_on_the_selected_agent() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        let apply = |action| Effect::Apply {
            key: "k-p2".into(),
            action,
        };
        assert_eq!(
            press(&mut picker, KeyCode::Char('s')),
            apply(Action::ToggleSettle)
        );
        assert_eq!(
            press(&mut picker, KeyCode::Char('u')),
            apply(Action::ToggleUnread)
        );
        assert_eq!(
            press(&mut picker, KeyCode::Char('p')),
            apply(Action::TogglePin)
        );
    }

    #[test]
    fn snooze_prompts_for_a_preset_and_keeps_its_target_while_rows_move() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        assert_eq!(press(&mut picker, KeyCode::Char('z')), Effect::None);
        assert!(picker.snooze_prompt().is_some());
        assert_eq!(press(&mut picker, KeyCode::Char('j')), Effect::None);
        assert!(
            picker.snooze_prompt().is_some(),
            "stray keys keep the prompt open"
        );

        let mut without_target = items();
        without_target.remove(1);
        picker.refresh(without_target, &closed_sections());
        assert_ne!(
            picker.selected().map(|i| i.pane_id().unwrap_or_default()),
            Some("p2")
        );

        let later = NOW + 600;
        let confirm = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(
            picker.key(confirm, later, &chrono::Utc),
            Effect::Apply {
                key: "k-p2".into(),
                action: Action::Snooze(later + 3_600)
            }
        );
        assert!(picker.snooze_prompt().is_none());
    }

    #[test]
    fn snooze_on_a_snoozed_agent_wakes_it_and_on_a_busy_one_asks_when() {
        let list = listed(&three(Status::Working).act("p3", Action::Snooze(NOW + 60)));
        let mut picker = Picker::new(list, Some("p3"), &BTreeSet::from([Section::Snoozed]));
        assert_eq!(
            press(&mut picker, KeyCode::Char('z')),
            Effect::Apply {
                key: "k-p3".into(),
                action: Action::Wake
            }
        );

        let blocked = listed(&three(Status::Blocked));
        let mut picker = Picker::new(blocked, Some("p1"), &closed_sections());
        assert_eq!(press(&mut picker, KeyCode::Char('z')), Effect::None);
        assert!(
            picker.snooze_prompt().is_some(),
            "a blocked agent can be snoozed"
        );

        let mut picker = Picker::new(items(), Some("p1"), &closed_sections());
        press(&mut picker, KeyCode::Char('z'));
        assert!(
            picker.snooze_prompt().is_some(),
            "a working agent can be snoozed"
        );
    }

    #[test]
    fn a_snooze_digit_means_the_preset_that_was_shown() {
        let shown_at = NOW + 4 * 3_600 + 20 * 60;
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        picker.key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            shown_at,
            &chrono::Utc,
        );
        let presets = &picker.snooze_prompt().unwrap().presets;
        assert_eq!(presets[2].0.label(), "today 18:00");
        let confirm = KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE);
        let confirmed_at = shown_at + 20 * 60;
        assert_eq!(
            picker.key(confirm, confirmed_at, &chrono::Utc),
            Effect::Apply {
                key: "k-p2".into(),
                action: Action::Snooze(NOW + 5 * 3_600)
            }
        );
    }

    #[test]
    fn snooze_digits_keep_their_preset_after_the_evening_one_is_gone() {
        let evening = NOW + 4 * 3_600 + 40 * 60;
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        picker.key(key('z'), evening, &chrono::Utc);
        assert_eq!(
            picker.key(key('4'), evening, &chrono::Utc),
            Effect::Apply {
                key: "k-p2".into(),
                action: Action::Snooze(NOW + 20 * 3_600)
            },
            "4 is still tomorrow 09:00"
        );
        picker.key(key('z'), evening, &chrono::Utc);
        assert_eq!(picker.key(key('3'), evening, &chrono::Utc), Effect::None);
        assert!(picker.snooze_prompt().is_none());
        assert_eq!(picker.message(), Some("today 18:00 is too soon"));
    }

    #[test]
    fn a_snooze_prompt_left_open_past_its_time_does_not_snooze_into_the_past() {
        let shown_at = NOW + 4 * 3_600;
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        picker.key(key('z'), shown_at, &chrono::Utc);
        let past_six = NOW + 5 * 3_600 + 60;
        assert_eq!(picker.key(key('3'), past_six, &chrono::Utc), Effect::None);
        assert_eq!(picker.message(), Some("today 18:00 is too soon"));
    }

    #[test]
    fn settling_an_agent_that_cannot_be_resumed_asks_first() {
        let sessionless = || {
            let mut items = items();
            for item in &mut items {
                item.row.thread.session = None;
            }
            items
        };
        let settle = |answer| {
            let mut picker = Picker::new(sessionless(), Some("p2"), &closed_sections());
            assert_eq!(press(&mut picker, KeyCode::Char('s')), Effect::None);
            assert_eq!(
                picker.close_prompt().map(|p| p.label.as_str()),
                Some("fix flaky test")
            );
            let effect = press(&mut picker, answer);
            assert!(picker.close_prompt().is_none());
            effect
        };
        let apply = |action| Effect::Apply {
            key: "k-p2".into(),
            action,
        };
        assert_eq!(settle(KeyCode::Char('y')), apply(Action::CloseForGood));
        assert_eq!(settle(KeyCode::Char('n')), apply(Action::ToggleSettle));
        assert_eq!(settle(KeyCode::Esc), Effect::None);
        assert_eq!(settle(KeyCode::Char('q')), Effect::None);

        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        assert_eq!(
            press(&mut picker, KeyCode::Char('s')),
            apply(Action::ToggleSettle),
            "a resumable agent settles at once"
        );
    }

    #[test]
    fn n_starts_an_agent_where_the_selected_thread_lives() {
        let mut items = items();
        items[1].row.thread.cwd = Some("/work/api".into());
        let mut picker = Picker::new(items, Some("p2"), &closed_sections());
        assert_eq!(press(&mut picker, KeyCode::Char('n')), Effect::None);
        assert_eq!(
            picker.new_agent_prompt().map(|p| p.label.as_str()),
            Some("acme-api")
        );
        assert_eq!(press(&mut picker, KeyCode::Char('9')), Effect::None);
        assert_eq!(
            press(&mut picker, KeyCode::Char('1')),
            Effect::NewAgent(NewAgent {
                workspace_id: Some("w1".into()),
                cwd: Some("/work/api".into()),
                branch: None,
                command: "claude",
            })
        );
        assert!(picker.new_agent_prompt().is_none());
    }

    #[test]
    fn w_in_the_new_agent_prompt_asks_for_a_worktree_branch_first() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        press(&mut picker, KeyCode::Char('n'));
        press(&mut picker, KeyCode::Char('w'));
        for c in "fix/a b".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(
            picker.new_agent_prompt().and_then(|p| p.typing.as_deref()),
            Some("fix/ab"),
            "a branch has no spaces"
        );
        press(&mut picker, KeyCode::Enter);
        assert_eq!(
            press(&mut picker, KeyCode::Char('2')),
            Effect::NewAgent(NewAgent {
                workspace_id: Some("w1".into()),
                cwd: None,
                branch: Some("fix/ab".into()),
                command: "codex",
            })
        );

        press(&mut picker, KeyCode::Char('n'));
        press(&mut picker, KeyCode::Char('w'));
        press(&mut picker, KeyCode::Char('x'));
        press(&mut picker, KeyCode::Esc);
        let prompt = picker.new_agent_prompt().unwrap();
        assert!(
            prompt.typing.is_none() && prompt.branch.is_none(),
            "esc goes back"
        );
        assert_eq!(press(&mut picker, KeyCode::Char('q')), Effect::None);
        assert!(picker.new_agent_prompt().is_none());
    }

    #[test]
    fn n_on_a_t3_thread_starts_the_agent_in_the_current_workspace() {
        let mut picker = Picker::new(with_t3(), None, &closed_sections());
        select(&mut picker, "t3-done");
        press(&mut picker, KeyCode::Char('n'));
        assert_eq!(
            picker.new_agent_prompt().map(|p| p.label.as_str()),
            Some("this workspace")
        );
        assert_eq!(
            press(&mut picker, KeyCode::Char('4')),
            Effect::NewAgent(NewAgent {
                workspace_id: None,
                cwd: None,
                branch: None,
                command: "pi",
            })
        );
    }

    #[test]
    fn escape_or_q_cancels_the_snooze_prompt() {
        for cancel in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
            press(&mut picker, KeyCode::Char('z'));
            assert_eq!(press(&mut picker, cancel), Effect::None);
            assert!(picker.snooze_prompt().is_none());
            assert_eq!(press(&mut picker, KeyCode::Esc), Effect::Close);
        }
    }

    #[test]
    fn selection_follows_the_agent_even_when_it_moves_to_another_pane() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        let mut moved = items();
        moved[1].target = Target::Pane("p9".into());
        moved.swap(0, 1);
        picker.refresh(moved, &closed_sections());
        assert_eq!(picker.selected().map(|i| i.row.key.as_str()), Some("k-p2"));
    }

    #[test]
    fn selection_follows_the_agent_across_refreshes() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        let mut reordered = items();
        reordered.rotate_left(1);
        picker.refresh(reordered, &closed_sections());
        assert_eq!(
            picker.selected().map(|i| i.pane_id().unwrap_or_default()),
            Some("p2")
        );
    }

    #[test]
    fn escape_leaves_search_keeping_the_filter_then_clears_it_then_closes() {
        let mut picker = Picker::new(items(), None, &closed_sections());
        press(&mut picker, KeyCode::Char('/'));
        for c in "flky".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(layout(&picker), ["p2"]);

        assert_eq!(press(&mut picker, KeyCode::Esc), Effect::None);
        assert_eq!(picker.mode(), Mode::Navigate);
        assert_eq!(picker.filter(), "flky");
        assert_eq!(layout(&picker), ["p2"]);
        assert_eq!(
            press(&mut picker, KeyCode::Char('s')),
            Effect::Apply {
                key: "k-p2".into(),
                action: Action::ToggleSettle
            }
        );
        press(&mut picker, KeyCode::Char('j'));
        assert_eq!(picker.filter(), "flky", "j moves instead of typing");

        assert_eq!(press(&mut picker, KeyCode::Esc), Effect::None);
        assert_eq!(picker.filter(), "");
        assert_eq!(layout(&picker).len(), 3);
        assert_eq!(press(&mut picker, KeyCode::Esc), Effect::Close);
    }

    #[test]
    fn slash_reopens_the_kept_filter_for_editing() {
        let mut picker = Picker::new(items(), None, &closed_sections());
        press(&mut picker, KeyCode::Char('/'));
        press(&mut picker, KeyCode::Char('f'));
        press(&mut picker, KeyCode::Esc);
        press(&mut picker, KeyCode::Char('/'));
        press(&mut picker, KeyCode::Char('l'));
        assert_eq!(picker.filter(), "fl");
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Effect::Focus("p2".into())
        );
    }

    #[test]
    fn parked_agents_collapse_into_counted_sections() {
        let picker = Picker::new(shelf(), None, &closed_sections());
        assert_eq!(
            layout(&picker),
            ["p1", "Snoozed:1:false", "Settled:2:false"]
        );

        let picker = Picker::new(shelf(), None, &BTreeSet::from([Section::Settled]));
        assert_eq!(
            layout(&picker),
            ["p1", "Snoozed:1:false", "Settled:2:true", "p3", "p4"]
        );
    }

    #[test]
    fn enter_on_a_section_header_toggles_it_and_keeps_it_selected() {
        let mut picker = Picker::new(shelf(), None, &closed_sections());
        press(&mut picker, KeyCode::Char('k'));
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Effect::ToggleSection(Section::Settled)
        );
        assert_eq!(press(&mut picker, KeyCode::Char('s')), Effect::None);

        picker.refresh(shelf(), &BTreeSet::from([Section::Settled]));
        assert_eq!(picker.selected_row(), Some(2));
        assert!(picker.selected().is_none());
    }

    #[test]
    fn working_agents_wait_in_their_own_section_unless_pinned() {
        let fixture = Fixture::new(
            NOW,
            &[
                ("p1", "api refactor", Status::Working),
                ("p2", "fix flaky test", Status::Done),
                ("p5", "deploy", Status::Working),
                ("p3", "release notes", Status::Idle),
            ],
        )
        .act("p5", Action::TogglePin)
        .act("p3", Action::ToggleSettle);
        let list = listed(&fixture);
        let picker = Picker::new(list.clone(), Some("p1"), &closed_sections());
        assert_eq!(
            layout(&picker),
            ["p5", "p2", "Working:1:false", "Settled:1:false"]
        );
        assert_eq!(
            picker.selected_row(),
            Some(2),
            "opens on the agent's header"
        );

        let picker = Picker::new(list, None, &BTreeSet::from([Section::Working]));
        assert_eq!(
            layout(&picker),
            ["p5", "p2", "Working:1:true", "p1", "Settled:1:false"]
        );
    }

    #[test]
    fn searching_reaches_into_collapsed_sections() {
        let mut picker = Picker::new(shelf(), None, &closed_sections());
        press(&mut picker, KeyCode::Char('/'));
        for c in "notes".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(layout(&picker), ["Settled:1:true", "p3"]);
    }

    #[test]
    fn a_search_selects_its_best_match_even_inside_a_section() {
        let mut picker = Picker::new(shelf(), None, &closed_sections());
        press(&mut picker, KeyCode::Char('/'));
        for c in "notes".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(
            picker.selected().map(|item| item.row.label()),
            Some("release notes")
        );

        let mut list = listed(&Fixture::new(
            NOW,
            &[
                ("p1", "migrate auth", Status::Idle),
                ("p2", "bump postgres", Status::Working),
            ],
        ));
        list[0].search = "claude migrate auth sandboxes/ui/acme-api".into();
        let mut picker = Picker::new(list, None, &closed_sections());
        press(&mut picker, KeyCode::Char('/'));
        for c in "bump".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(layout(&picker), ["p1", "Working:1:true", "p2"]);
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Effect::Focus("p2".into())
        );
    }

    #[test]
    fn an_agent_inside_a_collapsed_section_opens_on_its_header() {
        let picker = Picker::new(shelf(), Some("p4"), &closed_sections());
        assert_eq!(picker.selected_row(), Some(2));
    }

    #[test]
    fn digits_count_agents_and_skip_section_headers() {
        let mut picker = Picker::new(shelf(), None, &BTreeSet::from([Section::Settled]));
        assert_eq!(
            press(&mut picker, KeyCode::Char('2')),
            Effect::Focus("p3".into())
        );
        assert_eq!(press(&mut picker, KeyCode::Char('4')), Effect::None);
    }

    #[test]
    fn movement_wraps() {
        let mut picker = Picker::new(items(), None, &closed_sections());
        press(&mut picker, KeyCode::Char('k'));
        assert_eq!(
            picker.selected().map(|i| i.pane_id().unwrap_or_default()),
            Some("p3")
        );
        press(&mut picker, KeyCode::Down);
        assert_eq!(
            picker.selected().map(|i| i.pane_id().unwrap_or_default()),
            Some("p1")
        );
    }

    #[test]
    fn parking_moves_on_to_the_next_agent_that_needs_you() {
        let list = |first_parked: bool| {
            let fixture = Fixture::new(
                NOW,
                &[
                    ("p1", "api refactor", Status::Idle),
                    ("p2", "fix flaky test", Status::Done),
                    ("p3", "terraform plan", Status::Idle),
                    ("p4", "bench run", Status::Blocked),
                ],
            );
            if first_parked {
                listed(&fixture.act("p1", Action::ToggleSettle))
            } else {
                listed(&fixture)
            }
        };
        let mut picker = Picker::new(list(false), Some("p1"), &closed_sections());
        picker.refresh(list(true), &closed_sections());
        picker.move_on_from("k-p1", 0);
        assert_eq!(picker.selected().map(|i| i.row.key.as_str()), Some("k-p4"));

        let calm = listed(
            &Fixture::new(
                NOW,
                &[
                    ("p1", "api refactor", Status::Idle),
                    ("p3", "terraform plan", Status::Idle),
                ],
            )
            .act("p1", Action::ToggleSettle),
        );
        let mut picker = Picker::new(calm.clone(), Some("p3"), &closed_sections());
        picker.refresh(calm, &closed_sections());
        picker.move_on_from("k-p1", 0);
        assert_eq!(
            picker.selected().map(|i| i.row.key.as_str()),
            Some("k-p3"),
            "with nothing waiting, the cursor stays where the parked row was"
        );

        let mut picker = Picker::new(list(false), Some("p3"), &closed_sections());
        picker.move_on_from("k-p1", 2);
        assert_eq!(
            picker.selected().map(|i| i.row.key.as_str()),
            Some("k-p3"),
            "an unparked thread keeps the selection"
        );

        let closed_for_good = listed(&Fixture::new(
            NOW,
            &[
                ("p2", "fix flaky test", Status::Done),
                ("p3", "terraform plan", Status::Idle),
                ("p4", "bench run", Status::Blocked),
            ],
        ));
        let mut picker = Picker::new(list(false), Some("p1"), &closed_sections());
        picker.refresh(closed_for_good, &closed_sections());
        picker.move_on_from("k-p1", 0);
        assert_eq!(
            picker.selected().map(|i| i.row.key.as_str()),
            Some("k-p4"),
            "a settle that forgot its thread moves on too"
        );
    }

    #[test]
    fn opening_a_closed_thread_reopens_it() {
        let mut picker = Picker::new(with_closed(), None, &closed_sections());
        let reopen = Effect::Apply {
            key: "k-p2".into(),
            action: Action::Reopen,
        };
        assert_eq!(press(&mut picker, KeyCode::Char('2')), reopen);
        press(&mut picker, KeyCode::Char('j'));
        assert_eq!(press(&mut picker, KeyCode::Enter), reopen);
    }

    #[test]
    fn shift_d_forgets_a_closed_thread_after_confirmation() {
        let mut picker = Picker::new(with_closed(), None, &closed_sections());
        assert_eq!(press(&mut picker, KeyCode::Char('D')), Effect::None);
        assert_eq!(
            picker.message(),
            Some("only closed threads can be forgotten")
        );

        press(&mut picker, KeyCode::Char('j'));
        press(&mut picker, KeyCode::Char('D'));
        assert_eq!(
            picker.forget_prompt().map(|p| p.label.as_str()),
            Some("old spike")
        );
        assert_eq!(press(&mut picker, KeyCode::Char('n')), Effect::None);
        assert!(picker.forget_prompt().is_none());

        press(&mut picker, KeyCode::Char('D'));
        assert_eq!(
            press(&mut picker, KeyCode::Char('y')),
            Effect::Apply {
                key: "k-p2".into(),
                action: Action::Forget
            }
        );
    }

    #[test]
    fn q_cancels_the_forget_prompt_and_keeps_the_picker_open() {
        let mut picker = Picker::new(with_closed(), None, &closed_sections());
        press(&mut picker, KeyCode::Char('j'));
        press(&mut picker, KeyCode::Char('D'));
        assert!(picker.forget_prompt().is_some());
        assert_eq!(press(&mut picker, KeyCode::Char('q')), Effect::None);
        assert!(picker.forget_prompt().is_none());
        assert_eq!(press(&mut picker, KeyCode::Char('q')), Effect::Close);
    }

    #[test]
    fn enter_on_a_header_does_nothing_while_a_filter_holds_sections_open() {
        let mut picker = Picker::new(shelf(), None, &closed_sections());
        press(&mut picker, KeyCode::Char('/'));
        press(&mut picker, KeyCode::Char('o'));
        press(&mut picker, KeyCode::Esc);
        while picker.selected().is_some() {
            press(&mut picker, KeyCode::Char('j'));
        }
        assert_eq!(press(&mut picker, KeyCode::Enter), Effect::None);
    }

    #[test]
    fn ctrl_w_deletes_a_word_after_wide_whitespace() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        press(&mut picker, KeyCode::Char('r'));
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        picker.key(ctrl('u'), NOW, &chrono::Utc);
        for c in "foo\u{3000}bar".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        picker.key(ctrl('w'), NOW, &chrono::Utc);
        assert_eq!(
            picker.rename_prompt().map(|p| p.text.as_str()),
            Some("foo\u{3000}")
        );

        press(&mut picker, KeyCode::Esc);
        press(&mut picker, KeyCode::Char('/'));
        for c in "foo\u{3000}bar".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        picker.key(ctrl('w'), NOW, &chrono::Utc);
        assert_eq!(picker.filter(), "foo\u{3000}");
    }

    #[test]
    fn a_rename_keeps_its_thread_when_herdr_learns_the_session() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        press(&mut picker, KeyCode::Char('r'));
        let mut rekeyed = items();
        rekeyed[1].row.key = "session:claude:id:abc".into();
        picker.refresh(rekeyed, &closed_sections());
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Effect::Rename {
                key: "session:claude:id:abc".into(),
                title: Some("fix flaky test".into())
            }
        );
    }

    #[test]
    fn shift_u_undoes_the_last_change() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        assert_eq!(press(&mut picker, KeyCode::Char('U')), Effect::Undo);
    }

    #[test]
    fn r_renames_the_selected_thread_in_a_footer_prompt() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        press(&mut picker, KeyCode::Char('r'));
        assert_eq!(
            picker.rename_prompt().map(|p| p.text.as_str()),
            Some("fix flaky test")
        );
        press(&mut picker, KeyCode::Char('u'));
        assert_eq!(
            picker.rename_prompt().map(|p| p.text.as_str()),
            Some("fix flaky testu")
        );
        picker.key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            NOW,
            &chrono::Utc,
        );
        for c in "quick fix".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Effect::Rename {
                key: "k-p2".into(),
                title: Some("quick fix".into())
            }
        );
        assert!(picker.rename_prompt().is_none());

        press(&mut picker, KeyCode::Char('r'));
        assert_eq!(press(&mut picker, KeyCode::Esc), Effect::None);
        assert!(picker.rename_prompt().is_none());
    }

    fn with_t3() -> Vec<Item> {
        let mut snooze = t3_thread("t3-snoozed", "completed");
        snooze.snoozed_until = Some(NOW + 600);
        let snapshot = crate::t3::Snapshot {
            environment_id: "env".into(),
            projects: std::collections::BTreeMap::new(),
            threads: vec![
                t3_thread("t3-done", "completed"),
                t3_thread("t3-busy", "running"),
                snooze,
            ],
        };
        let mut items = items();
        items.extend(crate::t3::items(&snapshot, NOW));
        items
    }

    fn t3_thread(id: &str, status: &str) -> crate::t3::T3Thread {
        crate::t3::T3Thread {
            id: id.into(),
            project_id: "p".into(),
            title: id.into(),
            status: status.into(),
            provider: "codex".into(),
            settled: false,
            snoozed_until: None,
            pinned: false,
            unseen: true,
            updated_at: NOW - 60,
        }
    }

    fn select(picker: &mut Picker, label: &str) {
        for _ in 0..picker.rows().len() {
            if picker
                .selected()
                .is_some_and(|item| item.row.label() == label)
            {
                return;
            }
            press(picker, KeyCode::Char('j'));
        }
        panic!("no row {label}");
    }

    #[test]
    fn t3_threads_join_the_sections_herdr_agents_use() {
        let picker = Picker::new(with_t3(), None, &closed_sections());
        let rows: Vec<String> = picker
            .rows()
            .iter()
            .map(|row| match row {
                ListRow::Agent(item) => item.row.label().to_owned(),
                ListRow::Section { section, count, .. } => format!("{section:?}:{count}"),
            })
            .collect();
        assert_eq!(
            rows,
            [
                "api refactor",
                "fix flaky test",
                "terraform plan",
                "t3-done",
                "Working:1",
                "Snoozed:1"
            ]
        );
    }

    #[test]
    fn enter_or_its_digit_opens_a_t3_thread_in_t3() {
        let mut picker = Picker::new(with_t3(), None, &closed_sections());
        select(&mut picker, "t3-done");
        let opened = Effect::OpenT3("t3code://threads/env/t3-done".into());
        assert_eq!(press(&mut picker, KeyCode::Enter), opened);
        assert_eq!(press(&mut picker, KeyCode::Char('4')), opened);
    }

    fn t3_op(thread: &str, op: crate::t3::Op) -> Effect {
        Effect::T3 {
            thread: thread.into(),
            op,
        }
    }

    fn t3_shelf() -> Vec<Item> {
        let thread = |id: &str, status: &str, unseen: bool| crate::t3::T3Thread {
            unseen,
            ..t3_thread(id, status)
        };
        let snapshot = crate::t3::Snapshot {
            environment_id: "env".into(),
            projects: std::collections::BTreeMap::new(),
            threads: vec![
                thread("t3-done", "completed", true),
                thread("t3-seen", "completed", false),
                crate::t3::T3Thread {
                    pinned: true,
                    ..thread("t3-pinned", "completed", false)
                },
                crate::t3::T3Thread {
                    snoozed_until: Some(NOW + 600),
                    ..thread("t3-snoozed", "completed", false)
                },
                crate::t3::T3Thread {
                    settled: true,
                    ..thread("t3-settled", "completed", false)
                },
            ],
        };
        crate::t3::items(&snapshot, NOW)
    }

    #[test]
    fn triage_keys_on_a_t3_thread_send_t3_its_own_commands() {
        use crate::t3::Op;
        let mut sections = closed_sections();
        sections.extend([Section::Snoozed, Section::Settled]);
        let mut picker = Picker::new(t3_shelf(), None, &sections);
        select(&mut picker, "t3-done");
        assert_eq!(
            press(&mut picker, KeyCode::Char('s')),
            t3_op("t3-done", Op::Settle)
        );
        assert_eq!(
            press(&mut picker, KeyCode::Char('p')),
            t3_op("t3-done", Op::Pin)
        );
        assert_eq!(press(&mut picker, KeyCode::Char('z')), Effect::None);
        assert!(picker.snooze_prompt().is_some());
        assert_eq!(
            press(&mut picker, KeyCode::Char('1')),
            t3_op("t3-done", Op::Snooze { until: NOW + 3_600 })
        );
        assert_eq!(press(&mut picker, KeyCode::Char('u')), Effect::None);
        assert!(picker.message_is_error());
        assert_eq!(
            picker.message(),
            Some("T3 marks a thread read when you open it")
        );

        select(&mut picker, "t3-seen");
        assert_eq!(
            press(&mut picker, KeyCode::Char('u')),
            t3_op("t3-seen", Op::MarkUnread)
        );
        select(&mut picker, "t3-pinned");
        assert_eq!(
            press(&mut picker, KeyCode::Char('p')),
            t3_op("t3-pinned", Op::Unpin)
        );
        select(&mut picker, "t3-snoozed");
        assert_eq!(
            press(&mut picker, KeyCode::Char('z')),
            t3_op("t3-snoozed", Op::Unsnooze)
        );
        select(&mut picker, "t3-settled");
        assert_eq!(
            press(&mut picker, KeyCode::Char('s')),
            t3_op("t3-settled", Op::Unsettle)
        );
    }

    #[test]
    fn rename_and_forget_stay_with_t3_and_undo_stays_with_herdr() {
        let mut picker = Picker::new(t3_shelf(), None, &closed_sections());
        select(&mut picker, "t3-done");
        for key in ['r', 'D'] {
            assert_eq!(
                press(&mut picker, KeyCode::Char(key)),
                Effect::None,
                "{key}"
            );
            assert!(picker.message_is_error(), "{key}");
            assert_eq!(
                picker.message(),
                Some("rename or forget T3 threads in T3"),
                "{key}"
            );
            assert!(picker.rename_prompt().is_none() && picker.forget_prompt().is_none());
        }
        assert_eq!(press(&mut picker, KeyCode::Char('U')), Effect::Undo);
        press(&mut picker, KeyCode::Char('/'));
        press(&mut picker, KeyCode::Char('s'));
        assert_eq!(picker.filter(), "s");
    }

    #[test]
    fn t3_threads_join_herdr_agents_of_the_same_rank_by_recency_and_snoozes_wake_in_order() {
        let t3 = |id: &str, updated_ago: i64, snoozed_for: Option<i64>, settled: bool| {
            crate::t3::T3Thread {
                updated_at: NOW - updated_ago,
                snoozed_until: snoozed_for.map(|secs| NOW + secs),
                settled,
                ..t3_thread(id, "completed")
            }
        };
        let snapshot = crate::t3::Snapshot {
            environment_id: "env".into(),
            projects: std::collections::BTreeMap::new(),
            threads: vec![
                t3("t3 settled", 600, None, true),
                t3("t3 settled just now", 1, None, true),
                t3("t3 wakes late", 60, Some(600), false),
                t3("t3 older", 7_200, None, false),
                t3("t3 wakes soon", 60, Some(30), false),
                t3("t3 newer", 60, None, false),
            ],
        };
        let mut herdr = shelf();
        for item in &mut herdr {
            item.row.changed_at = Some(NOW - 120);
        }
        let combined = combine(herdr, crate::t3::items(&snapshot, NOW));
        let labels: Vec<&str> = combined.iter().map(|item| item.row.label()).collect();
        assert_eq!(
            labels,
            [
                "api refactor",
                "t3 newer",
                "t3 older",
                "t3 wakes soon",
                "bench run",
                "t3 wakes late",
                "t3 settled just now",
                "release notes",
                "old spike",
                "t3 settled",
            ]
        );
    }

    #[test]
    fn a_t3_thread_that_just_blocked_comes_before_herdr_agents_idle_for_longer() {
        let herdr = Fixture::new(
            NOW,
            &[
                ("p1", "idle for a day", Status::Idle),
                ("p2", "also idle", Status::Idle),
            ],
        );
        let mut items = listed(&herdr);
        for item in &mut items {
            item.row.changed_at = Some(NOW - 86_400);
        }
        let snapshot = crate::t3::Snapshot {
            environment_id: "env".into(),
            projects: std::collections::BTreeMap::new(),
            threads: vec![crate::t3::T3Thread {
                updated_at: NOW - 30,
                ..t3_thread("t3 blocked", "waiting")
            }],
        };
        let combined = combine(items, crate::t3::items(&snapshot, NOW));
        let labels: Vec<&str> = combined.iter().map(|item| item.row.label()).collect();
        assert_eq!(labels, ["t3 blocked", "idle for a day", "also idle"]);
    }

    #[test]
    fn parking_moves_on_to_a_t3_thread_that_needs_you_more() {
        let herdr = Fixture::new(
            NOW,
            &[
                ("p1", "api refactor", Status::Idle),
                ("p2", "fix flaky test", Status::Done),
            ],
        )
        .act("p1", Action::ToggleSettle);
        let mut items = listed(&herdr);
        let snapshot = crate::t3::Snapshot {
            environment_id: "env".into(),
            projects: std::collections::BTreeMap::new(),
            threads: vec![t3_thread("t3-waiting", "waiting")],
        };
        items.extend(crate::t3::items(&snapshot, NOW));
        let mut picker = Picker::new(items, None, &closed_sections());
        picker.move_on_from("k-p1", 0);
        assert_eq!(
            picker.selected().map(|item| item.row.label()),
            Some("t3-waiting")
        );
    }

    #[test]
    fn control_keys_on_a_t3_thread_act_as_on_herdr_rows_and_ctrl_p_still_moves() {
        let mut picker = Picker::new(with_t3(), None, &closed_sections());
        select(&mut picker, "t3-done");
        let ctrl = |picker: &mut Picker, c| {
            picker.key(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
                NOW,
                &chrono::Utc,
            )
        };
        assert_eq!(
            ctrl(&mut picker, 's'),
            t3_op("t3-done", crate::t3::Op::Settle)
        );
        assert_eq!(ctrl(&mut picker, 'r'), Effect::None);
        assert!(picker.rename_prompt().is_none());
        ctrl(&mut picker, 'p');
        assert_eq!(
            picker.selected().map(|item| item.row.label()),
            Some("terraform plan")
        );
    }

    #[test]
    fn a_t3_change_moves_the_cursor_on_only_if_you_have_not_moved_since() {
        let fixture = Fixture::new(NOW, &[("p1", "needs you", Status::Done)]);
        let snapshot = |settled| crate::t3::Snapshot {
            environment_id: "env".into(),
            projects: std::collections::BTreeMap::new(),
            threads: vec![crate::t3::T3Thread {
                settled,
                ..t3_thread("t3-a", "idle")
            }],
        };
        let shown = |settled| {
            let mut items = listed(&fixture);
            items.extend(crate::t3::items(&snapshot(settled), NOW));
            items
        };
        let mut picker = Picker::new(shown(false), None, &closed_sections());
        select(&mut picker, "t3-a");
        press(&mut picker, KeyCode::Char('s'));
        let sent = picker.sent("t3:t3-a");
        picker.refresh(shown(true), &closed_sections());
        picker.move_on_after(&sent);
        assert_eq!(
            picker.selected().map(|item| item.row.label()),
            Some("needs you")
        );

        let mut picker = Picker::new(shown(false), None, &closed_sections());
        select(&mut picker, "t3-a");
        press(&mut picker, KeyCode::Char('s'));
        let sent = picker.sent("t3:t3-a");
        press(&mut picker, KeyCode::Char('k'));
        press(&mut picker, KeyCode::Char('j'));
        picker.refresh(shown(true), &closed_sections());
        picker.move_on_after(&sent);
        assert_eq!(
            picker.selected().map(|item| item.row.label()),
            None,
            "stays where you put it: the settled section"
        );
    }
}
