use std::collections::BTreeSet;

use chrono::TimeZone;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::triage::{Action, Park, Refusal, Row, Section, SnoozePreset, snooze_presets};

#[derive(Debug, Clone)]
pub struct Item {
    pub row: Row,
    /// The pane a live thread runs in; `None` exactly when the row is not live.
    pub pane_id: Option<String>,
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
    Apply { key: String, action: Action },
    Rename { key: String, title: Option<String> },
    Undo,
    ToggleSection(Section),
}

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

#[derive(Debug, Clone)]
pub struct SnoozePrompt {
    pub key: String,
    pub label: String,
    pub presets: Vec<SnoozePreset>,
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
    rename: Option<RenamePrompt>,
    undoable: bool,
    message: Option<String>,
    message_is_error: bool,
    offset: usize,
    rows_on_screen: usize,
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
            rename: None,
            undoable: false,
            message: None,
            message_is_error: false,
            offset: 0,
            rows_on_screen: 10,
        };
        let start = focus_pane
            .and_then(|pane| {
                items
                    .iter()
                    .position(|item| item.pane_id.as_deref() == Some(pane))
            })
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
                let section = self.items[index].row.section()?;
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
            && let Some(item) = self
                .items
                .iter()
                .find(|item| item.pane_id.is_some() && item.pane_id == prompt.pane_id)
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
        self.message = None;
        if self.rename.is_some() {
            return self.rename_key(key, ctrl);
        }
        if let Some(prompt) = self.forget.take() {
            return match key.code {
                KeyCode::Char('y') => Effect::Apply {
                    key: prompt.key,
                    action: Action::Forget,
                },
                KeyCode::Char('q') => Effect::Close,
                _ => Effect::None,
            };
        }
        if let Some(prompt) = &self.snooze {
            let choice = match key.code {
                KeyCode::Char(c @ '1'..='9') => c as usize - '1' as usize,
                KeyCode::Char('q') => return Effect::Close,
                KeyCode::Esc => {
                    self.snooze = None;
                    return Effect::None;
                }
                _ => return Effect::None,
            };
            let Some(until) = prompt
                .presets
                .get(choice)
                .map(|preset| preset.until(now, tz))
            else {
                return Effect::None;
            };
            let key = prompt.key.clone();
            self.snooze = None;
            return Effect::Apply {
                key,
                action: Action::Snooze(until),
            };
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
            KeyCode::Char('s') => return apply(self, Action::ToggleSettle),
            KeyCode::Char('u') => return apply(self, Action::ToggleUnread),
            KeyCode::Char('p') => return apply(self, Action::TogglePin),
            KeyCode::Char('U') => return Effect::Undo,
            KeyCode::Char('r') => {
                if let Some(item) = self.selected() {
                    self.rename = Some(RenamePrompt {
                        key: item.row.key.clone(),
                        pane_id: item.pane_id.clone(),
                        text: item.row.label().to_owned(),
                    });
                }
            }
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
                    label: item.row.label().to_owned(),
                    presets: snooze_presets(now, tz),
                });
            }
            _ => {}
        }
        Effect::None
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
        match &item.pane_id {
            Some(pane) => Effect::Focus(pane.clone()),
            None => Effect::Apply {
                key: item.row.key.clone(),
                action: Action::Reopen,
            },
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
                .filter(|&i| items[i].row.section() == section)
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
    pub fn move_on_from(&mut self, key: &str, from: usize) {
        if !self
            .items
            .iter()
            .any(|item| item.row.key == key && item.row.park.is_some())
        {
            return;
        }
        let needs_you = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(at, &entry)| match entry {
                Entry::Agent(i) if self.items[i].row.live() => {
                    Some((self.items[i].row.attention()?, at))
                }
                _ => None,
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

    pub fn set_undoable(&mut self, undoable: bool) {
        self.undoable = undoable;
    }

    pub fn undoable(&self) -> bool {
        self.undoable
    }

    pub fn rename_prompt(&self) -> Option<&RenamePrompt> {
        self.rename.as_ref()
    }

    pub fn forget_prompt(&self) -> Option<&ForgetPrompt> {
        self.forget.as_ref()
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

#[cfg(test)]
impl Item {
    pub(crate) fn projected(fixture: &crate::triage::Fixture, now: i64) -> Vec<Item> {
        fixture
            .rows()
            .into_iter()
            .map(|(row, pane_id)| Item {
                pane_id,
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
                ("p2", "old spike", Status::Done),
            ],
        )
        .act("p1", Action::TogglePin)
        .close("p2")
        .act("p2", Action::ToggleUnread);
        listed(&fixture)
    }

    fn layout(picker: &Picker) -> Vec<String> {
        picker
            .rows()
            .iter()
            .map(|row| match row {
                ListRow::Agent(item) => item.pane_id.clone().unwrap_or_default(),
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
            picker
                .selected()
                .map(|i| i.pane_id.as_deref().unwrap_or_default()),
            Some("p3")
        );
        let picker = Picker::new(items(), None, &closed_sections());
        assert_eq!(
            picker
                .selected()
                .map(|i| i.pane_id.as_deref().unwrap_or_default()),
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
            picker
                .selected()
                .map(|i| i.pane_id.as_deref().unwrap_or_default()),
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
    fn snooze_on_a_snoozed_agent_wakes_it_and_on_a_busy_one_explains_why_not() {
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
        assert!(picker.snooze_prompt().is_none());
        assert_eq!(picker.message(), Some("can't snooze a blocked agent"));

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
        assert_eq!(presets[2].label(), "today 18:00");
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
    fn escape_cancels_the_snooze_prompt() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        press(&mut picker, KeyCode::Char('z'));
        assert_eq!(press(&mut picker, KeyCode::Esc), Effect::None);
        assert!(picker.snooze_prompt().is_none());
        assert_eq!(press(&mut picker, KeyCode::Esc), Effect::Close);
    }

    #[test]
    fn selection_follows_the_agent_even_when_it_moves_to_another_pane() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        let mut moved = items();
        moved[1].pane_id = Some("p9".into());
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
            picker
                .selected()
                .map(|i| i.pane_id.as_deref().unwrap_or_default()),
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
            picker
                .selected()
                .map(|i| i.pane_id.as_deref().unwrap_or_default()),
            Some("p3")
        );
        press(&mut picker, KeyCode::Down);
        assert_eq!(
            picker
                .selected()
                .map(|i| i.pane_id.as_deref().unwrap_or_default()),
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
    fn q_closes_the_picker_even_while_asking_to_forget() {
        let fixture = Fixture::new(NOW, &[("p2", "old spike", Status::Done)]).close("p2");
        let mut picker = Picker::new(listed(&fixture), None, &closed_sections());
        press(&mut picker, KeyCode::Char('D'));
        assert_eq!(press(&mut picker, KeyCode::Char('q')), Effect::Close);
    }

    #[test]
    fn q_closes_the_picker_from_the_snooze_prompt() {
        let mut picker = Picker::new(items(), Some("p2"), &closed_sections());
        press(&mut picker, KeyCode::Char('z'));
        assert!(picker.snooze_prompt().is_some());
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
}
