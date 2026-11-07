use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, HighlightSpacing, Padding, Paragraph, Row, Table, TableState,
};

use crate::picker::{Item, ListRow, Mode, Picker};
use crate::triage::{Display, Section, Unread};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const HIGHLIGHT: &str = "▎ ";
/// Blank cells after Workspace and Agent, so the last three columns read apart.
const GAP: u16 = 1;
/// The fewest title cells worth showing beside ` (focused)` or quoting in a prompt.
const MIN_LABEL: usize = 8;
/// Thread cells kept before Status, Workspace and Agent give up theirs.
const MIN_THREAD: u16 = 24;
/// The most Thread cells Status gives up from the room it keeps for rows not on show.
const WIDE_THREAD: u16 = 40;
const FOCUSED: &str = " (focused)";

pub fn draw(frame: &mut Frame, picker: &mut Picker, tick: usize, pin_icon: &str) {
    let area = frame.area().inner(Margin::new(1, 0));
    let [search, _, heading, top_rule, list, bottom_rule, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_search(frame, picker, search);

    let rows = picker.rows();
    let shown: Vec<&Item> = rows
        .iter()
        .filter_map(|row| match row {
            ListRow::Agent(item) => Some(*item),
            ListRow::Section { .. } => None,
        })
        .collect();
    let columns = Columns::new(picker.items(), &shown, area.width, pin_icon);
    draw_heading(frame, picker, &columns, heading);

    let (above, below) = if rows.is_empty() {
        draw_empty(frame, picker, list);
        (0, 0)
    } else {
        let numbered = picker.mode() == Mode::Navigate;
        let offset = first_row_on_screen(picker.offset(), picker.selected_row(), list.height);
        let mut agents_on_screen = 0;
        let table_rows: Vec<Row> = rows
            .iter()
            .enumerate()
            .map(|(i, list_row)| match list_row {
                ListRow::Agent(item) => {
                    let number = (numbered && i >= offset).then(|| {
                        agents_on_screen += 1;
                        agents_on_screen
                    });
                    row(item, number, tick, pin_icon, &columns)
                }
                ListRow::Section {
                    section,
                    count,
                    open,
                } => section_row(*section, *count, *open),
            })
            .collect();
        let table = Table::new(table_rows, columns.constraints)
            .highlight_symbol(Line::from(HIGHLIGHT).blue())
            .highlight_spacing(HighlightSpacing::Always)
            .row_highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .remove_modifier(Modifier::DIM),
            )
            .column_spacing(1);
        let mut state = TableState::default()
            .with_offset(offset)
            .with_selected(picker.selected_row());
        frame.render_stateful_widget(table, list, &mut state);
        let (offset, height) = (state.offset(), usize::from(list.height));
        let hidden = (
            threads(&rows[..offset.min(rows.len())]),
            threads(rows.get(offset + height..).unwrap_or_default()),
        );
        picker.set_viewport(offset, height);
        hidden
    };
    frame.render_widget(Paragraph::new(rule(top_rule.width, '↑', above)), top_rule);
    frame.render_widget(
        Paragraph::new(rule(bottom_rule.width, '↓', below)),
        bottom_rule,
    );

    let (line, cursor) = footer_line(picker, usize::from(footer.width));
    frame.render_widget(Paragraph::new(line), footer);
    if let Some(column) = cursor {
        let x = footer
            .x
            .saturating_add(u16::try_from(column).unwrap_or(u16::MAX))
            .min(footer.right().saturating_sub(1));
        frame.set_cursor_position((x, footer.y));
    }
}

fn draw_heading(frame: &mut Frame, picker: &Picker, columns: &Columns, area: Rect) {
    let number_heading = if picker.mode() == Mode::Search {
        ""
    } else {
        "#"
    };
    let header = Table::new(Vec::<Row>::new(), columns.constraints)
        .header(
            Row::new([
                number_heading,
                "",
                "",
                "Thread",
                "Workspace",
                "Agent",
                "Status",
            ])
            .dim(),
        )
        .highlight_symbol(HIGHLIGHT)
        .highlight_spacing(HighlightSpacing::Always)
        .column_spacing(1);
    frame.render_widget(header, area);
}

/// Threads in `rows`, counting those inside collapsed sections.
fn threads(rows: &[ListRow]) -> usize {
    rows.iter()
        .map(|row| match row {
            ListRow::Agent(_) => 1,
            ListRow::Section {
                count, open: false, ..
            } => *count,
            ListRow::Section { .. } => 0,
        })
        .sum()
}

/// A divider that says how many threads sit beyond it, when any do.
fn rule(width: u16, arrow: char, hidden: usize) -> Line<'static> {
    let width = usize::from(width);
    if hidden == 0 {
        return Line::from("─".repeat(width)).dark_gray();
    }
    let label = format!(" {arrow} {hidden} more ");
    let tail = 2;
    let lead = width.saturating_sub(cells(&label) + tail);
    Line::from(vec![
        Span::from("─".repeat(lead)).dark_gray(),
        Span::from(label).dim(),
        Span::from("─".repeat(tail)).dark_gray(),
    ])
}

fn draw_search(frame: &mut Frame, picker: &Picker, area: Rect) {
    let searching = picker.mode() == Mode::Search;
    let border = if searching {
        Style::new().cyan()
    } else {
        Style::new().dark_gray()
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let count = if picker.filter().is_empty() {
        picker.total().to_string()
    } else {
        format!("{}/{}", picker.matched(), picker.total())
    };
    let count_width = u16::try_from(cells(&count)).unwrap_or(u16::MAX);
    let [query_area, _, count_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(2),
        Constraint::Length(count_width),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(count).dim(), count_area);

    let prompt = "❯ ";
    let room = usize::from(query_area.width).saturating_sub(cells(prompt) + 1);
    let query = tail(picker.filter(), room);
    let line = match picker.mode() {
        Mode::Navigate if picker.filter().is_empty() => Line::from(vec![
            Span::from(prompt).dim(),
            Span::from("/ to search").dim().italic(),
        ]),
        Mode::Navigate => Line::from(vec![
            Span::from(prompt).cyan(),
            Span::from(query.to_string()),
        ]),
        Mode::Search => Line::from(vec![
            Span::from(prompt).cyan().bold(),
            Span::from(query.to_string()),
        ]),
    };
    frame.render_widget(Paragraph::new(line), query_area);
    if searching {
        let typed = u16::try_from(cells(prompt) + cells(&query)).unwrap_or(u16::MAX);
        let end = query_area.right().saturating_sub(1);
        frame.set_cursor_position((query_area.x.saturating_add(typed).min(end), query_area.y));
    }
}

fn first_row_on_screen(previous: usize, selected: Option<usize>, height: u16) -> usize {
    let height = usize::from(height.max(1));
    match selected {
        Some(selected) if selected < previous => selected,
        Some(selected) if selected >= previous + height => selected + 1 - height,
        _ => previous,
    }
}

/// Column constraints, and the cells each text column can show (as the table lays them out).
struct Columns {
    constraints: [Constraint; 7],
    thread: usize,
    workspace: usize,
    agent: usize,
    status: usize,
}

impl Columns {
    /// Widths come from every thread, not just the rows on show, so opening a section or
    /// typing a search leaves the columns where they are. Only a popup too narrow for the
    /// titles on show sizes Status to the statuses on show, so there those can move it.
    fn new(items: &[Item], shown: &[&Item], width: u16, pin_icon: &str) -> Self {
        let longest = |field: fn(&Item) -> &str, floor: usize, cap: u16| {
            let widest = items
                .iter()
                .map(|item| cells(field(item)))
                .max()
                .unwrap_or(0);
            u16::try_from(widest.max(floor))
                .unwrap_or(u16::MAX)
                .min(cap)
        };
        let pin = u16::try_from(cells(pin_icon)).unwrap_or(1);
        let selection = u16::try_from(cells(HIGHLIGHT)).unwrap_or(2);
        // [workspace, agent, status]
        let mut lengths = [
            longest(|i| &i.location, 9, width / 4),
            longest(|i| &i.kind, 5, width / 5),
            longest(|i| &i.status, 6, width * 2 / 5),
        ];
        let mut gap = GAP;
        let thread_room = |lengths: &[u16; 3], gap: u16| {
            let used = selection + 1 + pin + 1 + 6 + 2 * gap + lengths.iter().sum::<u16>();
            width.saturating_sub(used)
        };
        // First, Status gives back the room it keeps for statuses not on show, as far as the
        // titles on show need it.
        if !shown.is_empty() {
            let to_u16 = |cells: usize| u16::try_from(cells).unwrap_or(u16::MAX);
            let shown_status = to_u16(shown.iter().map(|i| cells(&i.status)).max().unwrap_or(0));
            let wanted = to_u16(
                shown
                    .iter()
                    .map(|i| cells(&i.label) + if i.focused { cells(FOCUSED) } else { 0 })
                    .max()
                    .unwrap_or(0),
            )
            .clamp(MIN_THREAD, WIDE_THREAD);
            let short = wanted.saturating_sub(thread_room(&lengths, gap));
            lengths[2] = lengths[2]
                .saturating_sub(short)
                .max(shown_status.max(6).min(lengths[2]));
        }
        // Then narrow popups take room from Status (whose middle details drop first), the
        // extra column gaps, Agent and Workspace, before the title goes below MIN_THREAD.
        for step in [
            Some((2, 20)),
            None,
            Some((1, 5)),
            Some((0, 9)),
            Some((2, 10)),
        ] {
            let short = MIN_THREAD.saturating_sub(thread_room(&lengths, gap));
            if short == 0 {
                break;
            }
            match step {
                Some((column, floor)) => {
                    let length = &mut lengths[column];
                    *length = length.saturating_sub(short).max(floor.min(*length));
                }
                None => gap = 0,
            }
        }
        let [workspace, agent, status] = lengths;
        let constraints = [
            Constraint::Length(1),
            Constraint::Length(pin),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(workspace + gap),
            Constraint::Length(agent + gap),
            Constraint::Length(status),
        ];
        let split = Layout::horizontal(constraints)
            .flex(Flex::Start)
            .spacing(1)
            .split(Rect::new(selection, 0, width.saturating_sub(selection), 1));
        let room = |i: usize, gap: u16| usize::from(split[i].width.saturating_sub(gap));
        Self {
            constraints,
            thread: room(3, 0),
            workspace: room(4, gap),
            agent: room(5, gap),
            status: room(6, 0),
        }
    }
}

/// Cuts `text` to `room` cells, ending in `…` when anything was cut.
fn ellipsize(text: &str, room: usize) -> Cow<'_, str> {
    if cells(text) <= room {
        return Cow::Borrowed(text);
    }
    if room == 0 {
        return Cow::Borrowed("");
    }
    let mut kept = String::new();
    let mut used = 0;
    for grapheme in Span::raw(text).styled_graphemes(Style::new()) {
        let width = cells(grapheme.symbol);
        if used + width + 1 > room {
            break;
        }
        kept.push_str(grapheme.symbol);
        used += width;
    }
    let kept = kept.trim_end().trim_end_matches('·').trim_end();
    Cow::Owned(format!("{kept}…"))
}

/// The end of `text` that fits `room` cells, starting with `…` when the start was cut.
fn tail(text: &str, room: usize) -> Cow<'_, str> {
    if cells(text) <= room {
        return Cow::Borrowed(text);
    }
    let span = Span::raw(text);
    let graphemes: Vec<&str> = span
        .styled_graphemes(Style::new())
        .map(|grapheme| grapheme.symbol)
        .collect();
    let mut used = cells("…");
    let mut start = graphemes.len();
    while start > 0 && used + cells(graphemes[start - 1]) <= room {
        start -= 1;
        used += cells(graphemes[start]);
    }
    Cow::Owned(format!("…{}", graphemes[start..].concat()))
}

/// Shortens a status, dropping middle details (like a countdown) before cutting the state
/// word, and keeping its last part (`closed`, `stays open`) whole.
fn fit_status(text: &str, room: usize) -> Cow<'_, str> {
    if cells(text) <= room {
        return Cow::Borrowed(text);
    }
    let parts: Vec<&str> = text.split(" · ").collect();
    let (Some(head), Some(last)) = (parts.first(), parts.last()) else {
        return ellipsize(text, room);
    };
    if parts.len() > 2 {
        let short = format!("{head} · {last}");
        if cells(&short) <= room {
            return Cow::Owned(short);
        }
    }
    if parts.len() == 1 {
        return ellipsize(text, room);
    }
    let marker = format!(" · {last}");
    if room < cells(&marker) + 2 {
        return ellipsize(text, room);
    }
    Cow::Owned(format!(
        "{}{marker}",
        ellipsize(head, room - cells(&marker))
    ))
}

fn row<'a>(
    item: &'a Item,
    number: Option<usize>,
    tick: usize,
    pin_icon: &'a str,
    columns: &Columns,
) -> Row<'a> {
    let number = match number {
        Some(n) if n <= 10 => (n % 10).to_string(),
        _ => String::new(),
    };
    let pin = if item.pinned {
        Span::from(pin_icon).blue()
    } else {
        Span::raw("")
    };
    let parked = item.park.is_some();
    let faint = Style::new().gray().dim();
    let glyph = match item.display {
        _ if item.closed() => Span::styled("◌", faint),
        _ if parked => Span::styled("○", faint),
        Display::Working => Span::from(SPINNER[tick % SPINNER.len()]).yellow(),
        Display::Blocked => Span::from("◉").red(),
        Display::Done => Span::from("●").cyan(),
        Display::Unread(_) => Span::from("●").cyan().bold(),
        Display::Idle => Span::from("✓").green(),
        Display::Unknown => Span::styled("○", faint),
    };
    let needs_you = !parked && matches!(item.display, Display::Done | Display::Unread(_));
    let suffix = if item.focused { FOCUSED } else { "" };
    let (label, suffix) = if cells(&item.label) + cells(suffix) <= columns.thread {
        (Cow::Borrowed(item.label.as_str()), suffix)
    } else if columns.thread >= cells(suffix) + MIN_LABEL {
        (
            ellipsize(&item.label, columns.thread - cells(suffix)),
            suffix,
        )
    } else {
        (ellipsize(&item.label, columns.thread), "")
    };
    let mut thread = vec![if needs_you {
        Span::from(label).bold()
    } else {
        Span::from(label)
    }];
    if !suffix.is_empty() {
        thread.push(Span::from(suffix).dim());
    }
    let status_style = match item.display {
        _ if parked => Style::new().dim(),
        Display::Working => Style::new().yellow(),
        Display::Blocked => Style::new().red(),
        Display::Done => Style::new().cyan(),
        Display::Unread(Unread::Manual | Unread::Woke) => Style::new().cyan().bold(),
        Display::Idle | Display::Unknown => Style::new().dim(),
    };
    let row_cells = [
        Cell::from(Span::from(number).blue()),
        Cell::from(pin),
        Cell::from(glyph),
        Cell::from(Line::from(thread)),
        Cell::from(Span::from(ellipsize(&item.location, columns.workspace)).dim()),
        Cell::from(Span::from(ellipsize(&item.kind, columns.agent)).dim()),
        Cell::from(Span::styled(
            fit_status(&item.status, columns.status),
            status_style,
        )),
    ];
    let row = Row::new(row_cells);
    if parked {
        row.add_modifier(Modifier::DIM)
    } else {
        row
    }
}

fn section_row(section: Section, count: usize, open: bool) -> Row<'static> {
    let name = match section {
        Section::Working => "working",
        Section::Snoozed => "snoozed",
        Section::Settled => "settled",
    };
    let fold = if open { "▾" } else { "▸" };
    Row::new([
        Cell::from(""),
        Cell::from(""),
        Cell::from(Span::from(fold).dim()),
        Cell::from(
            Line::from(vec![
                Span::from(name).bold(),
                Span::from(format!(" · {count}")),
            ])
            .dim(),
        ),
    ])
}

fn draw_empty(frame: &mut Frame, picker: &Picker, area: Rect) {
    let (headline, hint) = if picker.total() == 0 {
        ("No agents running".to_owned(), "esc/q close")
    } else {
        (
            format!("No matches for \"{}\"", picker.filter()),
            match picker.mode() {
                Mode::Search => "^u clears the search",
                Mode::Navigate => "esc clears the search",
            },
        )
    };
    let top = area.y + area.height / 3;
    let text = vec![Line::from(headline), Line::from(hint).dim()];
    let rect = Rect {
        y: top,
        height: area.height.saturating_sub(top - area.y),
        ..area
    };
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), rect);
}

fn hints(pairs: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, action)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::raw(*key));
        spans.push(Span::from(format!(" {action}")).dim());
    }
    Line::from(spans)
}

/// Hints that fit `width`, dropping whole pairs in `droppable` order rather than cutting one.
fn fitted_hints(
    mut pairs: Vec<(&'static str, &'static str)>,
    droppable: &[&str],
    width: usize,
) -> Line<'static> {
    for key in droppable {
        if hints(&pairs).width() <= width {
            break;
        }
        pairs.retain(|(k, _)| k != key);
    }
    hints(&pairs)
}

fn enter_hint(picker: &Picker) -> Option<(&'static str, &'static str)> {
    let row = picker.selected_row()?;
    match picker.selected() {
        Some(item) if item.closed() => Some(("enter", "reopen")),
        Some(_) => Some(("enter", "focus")),
        None if picker.filter().is_empty() => match picker.rows().get(row) {
            Some(ListRow::Section { open: true, .. }) => Some(("enter", "collapse")),
            _ => Some(("enter", "expand")),
        },
        None => None,
    }
}

/// Cells as the buffer lays them out, grapheme by grapheme; a whole-string width can differ.
fn cells(text: &str) -> usize {
    Span::raw(text)
        .styled_graphemes(Style::new())
        .map(|grapheme| Span::raw(grapheme.symbol).width())
        .sum()
}

/// The rename prompt; `room` is what the title (or its placeholder) may take.
fn rename_line(text: &str, room: usize) -> Line<'static> {
    let shown = if text.is_empty() {
        Span::from(ellipsize("automatic title", room).into_owned())
            .dim()
            .italic()
    } else {
        Span::raw(text.to_owned())
    };
    Line::from(vec![
        Span::raw(" "),
        Span::from("rename").cyan().bold(),
        Span::raw(": "),
        shown,
        Span::raw("  enter"),
        Span::from(" save").dim(),
        Span::raw("  esc"),
        Span::from(" cancel").dim(),
    ])
}

/// `verb “label”punct options  esc cancel`, shortening the label, then dropping it, then
/// `esc cancel`, so the options always show.
fn labelled_prompt(
    verb: Span<'static>,
    label: &str,
    punct: &'static str,
    options: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let cancel = [Span::raw("  esc"), Span::from(" cancel").dim()];
    let fixed: usize = 1
        + verb.width()
        + cells(punct)
        + options.iter().map(Span::width).sum::<usize>()
        + cancel.iter().map(Span::width).sum::<usize>();
    let room = width.saturating_sub(fixed + cells(" “”"));
    let quoted = if cells(label) <= room || room >= MIN_LABEL {
        Some(ellipsize(label, room))
    } else {
        None
    };
    let mut spans = vec![Span::raw(" "), verb];
    if let Some(label) = quoted {
        spans.push(Span::raw(format!(" “{label}”")));
    }
    spans.push(Span::raw(punct));
    spans.extend(options);
    if Line::from(spans.clone()).width() + cancel.iter().map(Span::width).sum::<usize>() <= width {
        spans.extend(cancel);
    }
    Line::from(spans)
}

/// The footer prompt, if one is open, and where the typing cursor sits in it.
fn prompt_line(picker: &Picker, width: usize) -> Option<(Line<'static>, Option<usize>)> {
    if let Some(prompt) = picker.rename_prompt() {
        let room = width.saturating_sub(cells(" rename: ") + cells("  enter save  esc cancel"));
        let text = tail(&prompt.text, room);
        let cursor = cells(" rename: ") + cells(&text);
        return Some((rename_line(&text, room), Some(cursor)));
    }
    if let Some(prompt) = picker.forget_prompt() {
        let options = vec![Span::raw("  y"), Span::from(" yes").dim()];
        let verb = Span::from("forget").red().bold();
        return Some((
            labelled_prompt(verb, &prompt.label, "?", options, width),
            None,
        ));
    }
    if let Some(prompt) = picker.snooze_prompt() {
        let options = prompt
            .presets
            .iter()
            .enumerate()
            .flat_map(|(i, preset)| {
                [
                    Span::raw(format!("  {}", i + 1)),
                    Span::from(format!(" {}", preset.label())).dim(),
                ]
            })
            .collect();
        let verb = Span::from("snooze").yellow().bold();
        return Some((
            labelled_prompt(verb, &prompt.label, "", options, width),
            None,
        ));
    }
    None
}

fn footer_line(picker: &Picker, width: usize) -> (Line<'static>, Option<usize>) {
    if let Some(message) = picker.message() {
        let mark = if picker.message_is_error() {
            vec![
                Span::from(" ✗ ").red().bold(),
                Span::from(message.to_owned()).red(),
            ]
        } else {
            vec![Span::from(" • ").blue(), Span::raw(message.to_owned())]
        };
        return (Line::from(mark), None);
    }
    if let Some(prompt) = prompt_line(picker, width) {
        return prompt;
    }
    let mut pairs: Vec<(&str, &str)> = enter_hint(picker).into_iter().collect();
    let line = match picker.mode() {
        Mode::Navigate => {
            pairs.extend([("j/k", "move"), ("/", "search")]);
            if let Some(item) = picker.selected() {
                let parked = |section| item.park.is_some_and(|park| park.section() == section);
                pairs.extend([
                    (
                        "s",
                        if parked(Section::Settled) {
                            "unsettle"
                        } else {
                            "settle"
                        },
                    ),
                    (
                        "z",
                        if parked(Section::Snoozed) {
                            "wake"
                        } else {
                            "snooze"
                        },
                    ),
                    (
                        "u",
                        if matches!(item.display, Display::Done | Display::Unread(_)) {
                            "read"
                        } else {
                            "unread"
                        },
                    ),
                    ("p", if item.pinned { "unpin" } else { "pin" }),
                    ("r", "rename"),
                ]);
                if item.closed() {
                    pairs.push(("D", "forget"));
                }
            }
            if picker.undoable() {
                pairs.push(("U", "undo"));
            }
            if picker.filter().is_empty() {
                pairs.push(("esc/q", "close"));
            } else {
                pairs.extend([("esc", "clear search"), ("q", "close")]);
            }
            pairs.push(("1-9/0", "open"));
            fitted_hints(pairs, &["1-9/0", "j/k", "/", "r", "p", "u", "U"], width)
        }
        Mode::Search => {
            pairs.push(("esc", "done"));
            if picker.selected_row().is_some() {
                pairs.push(("^j/^k", "move"));
            }
            pairs.extend([("^u", "clear"), ("^w", "delete word")]);
            fitted_hints(pairs, &["^w", "^u", "^j/^k"], width)
        }
    };
    (line, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::{Item, Picker};
    use std::collections::BTreeSet;

    use crate::triage::{Display, Park, Section};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Modifier;

    const NOW: i64 = 1_793_710_800;

    fn item(pane: &str, label: &str, display: Display, status: &str) -> Item {
        Item {
            pane_id: Some(pane.into()),
            key: pane.into(),
            label: label.into(),
            location: "acme-api".into(),
            kind: "claude".into(),
            search: label.into(),
            display,
            park: None,
            pinned: false,
            status: status.into(),
            focused: false,
        }
    }

    fn picker() -> Picker {
        let mut pinned = item("p1", "api refactor", Display::Working, "working 4m");
        pinned.pinned = true;
        let mut settled = item("p3", "release notes", Display::Idle, "settled");
        settled.park = Some(Park::Settled);
        let mut focused = item("p2", "fix flaky test", Display::Done, "done 2m");
        focused.focused = true;
        Picker::new(
            vec![pinned, focused, settled],
            Some("p2"),
            &BTreeSet::from([Section::Settled]),
        )
    }

    fn render(
        picker: &mut Picker,
        width: u16,
        height: u16,
    ) -> (Vec<String>, ratatui::buffer::Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, picker, 0, "P")).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let lines = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect();
        (lines, buffer)
    }

    fn press(picker: &mut Picker, code: KeyCode) {
        picker.key(KeyEvent::new(code, KeyModifiers::NONE), NOW, &chrono::Utc);
    }

    #[test]
    fn navigate_layout_has_search_box_numbered_rows_and_triage_hints() {
        let (lines, _) = render(&mut picker(), 100, 14);
        assert!(lines[0].trim_start().starts_with('╭'));
        assert!(lines[1].contains("❯ / to search") && lines[1].trim_end().ends_with("3 │"));
        assert!(lines[2].trim_start().starts_with('╰'));
        assert!(
            ["#", "Thread", "Workspace", "Agent", "Status"]
                .iter()
                .all(|h| lines[4].contains(h))
        );
        assert!(lines[5].contains("────"));
        assert!(
            lines[6].contains("1 P")
                && lines[6].contains("api refactor")
                && lines[6].contains("working 4m")
        );
        assert!(lines[7].contains("▎ 2") && lines[7].contains("fix flaky test (focused)"));
        assert!(lines[8].contains("▾ settled · 1"), "{}", lines[8]);
        assert!(
            lines[9].contains('3')
                && lines[9].contains("release notes")
                && lines[9].contains("settled")
        );
        assert!(lines[12].contains("────"));
        assert!(lines[13].contains("s settle  z snooze  u read  p pin  r rename  esc/q close"));
        assert_eq!(lines.iter().filter(|l| l.contains('│')).count(), 1);
    }

    #[test]
    fn parked_rows_are_dimmed() {
        let (lines, buffer) = render(&mut picker(), 100, 14);
        let x = lines[9]
            .find("release")
            .map(|byte| lines[9][..byte].chars().count())
            .unwrap();
        assert!(
            buffer[(u16::try_from(x).unwrap(), 9)]
                .modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn snooze_prompt_replaces_the_hints() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('z'));
        let (lines, _) = render(&mut picker, 120, 14);
        assert!(
            lines[13].contains(
                "snooze “fix flaky test”  1 1h  2 3h  3 today 18:00  4 tomorrow 09:00  5 next Mon 09:00  esc cancel"
            )
        );
    }

    #[test]
    fn refusals_show_in_the_footer() {
        let mut picker = picker();
        picker.notify("can't settle a working agent".into());
        let (lines, _) = render(&mut picker, 100, 14);
        assert!(lines[13].contains("✗ can't settle a working agent"));
    }

    #[test]
    fn search_mode_shows_the_query_and_hides_row_numbers() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('/'));
        press(&mut picker, KeyCode::Char('f'));
        let (lines, _) = render(&mut picker, 100, 14);
        assert!(lines[1].contains("❯ f"));
        assert!(!lines[6].contains("1 "));
        assert!(lines[13].contains("enter focus  esc done"));
    }

    #[test]
    fn empty_states_explain_themselves() {
        let (lines, _) = render(
            &mut Picker::new(Vec::new(), None, &BTreeSet::new()),
            100,
            14,
        );
        assert!(lines.iter().any(|l| l.contains("No agents running")));
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('/'));
        press(&mut picker, KeyCode::Char('x'));
        press(&mut picker, KeyCode::Char('q'));
        let (lines, _) = render(&mut picker, 100, 14);
        assert!(lines.iter().any(|l| l.contains("No matches for \"xq\"")));
    }

    #[test]
    fn digits_follow_the_rows_on_screen_after_scrolling() {
        let many: Vec<Item> = (0..12)
            .map(|i| {
                item(
                    &format!("p{i}"),
                    &format!("agent {i}"),
                    Display::Idle,
                    "idle",
                )
            })
            .collect();
        let mut picker = Picker::new(many, None, &BTreeSet::new());
        press(&mut picker, KeyCode::Char('k'));
        let (lines, _) = render(&mut picker, 100, 12);
        let first = lines[6].clone();
        assert!(first.contains("1 ") && first.contains("agent 8"), "{first}");
        let effect = picker.key(
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
            NOW,
            &chrono::Utc,
        );
        assert_eq!(effect, crate::picker::Effect::Focus("p8".into()));
    }

    #[test]
    fn long_queries_keep_the_cursor_inside_the_search_box() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('/'));
        for _ in 0..150 {
            press(&mut picker, KeyCode::Char('x'));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut picker, 0, "P"))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        assert!(cursor.x < 78, "cursor at {cursor:?}");
    }

    #[test]
    fn a_wide_pin_icon_gets_its_full_width() {
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        let mut picker = picker();
        terminal
            .draw(|frame| draw(frame, &mut picker, 0, "PIN"))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..100)
            .map(|x| buffer[(x, 6)].symbol().to_owned())
            .collect();
        assert!(row.contains("1 PIN"), "{row}");
    }

    #[test]
    fn digits_past_the_rows_on_screen_do_nothing() {
        let many: Vec<Item> = (0..12)
            .map(|i| {
                item(
                    &format!("p{i}"),
                    &format!("agent {i}"),
                    Display::Idle,
                    "idle",
                )
            })
            .collect();
        let mut picker = Picker::new(many, None, &BTreeSet::new());
        render(&mut picker, 100, 12);
        let five = KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE);
        assert_eq!(
            picker.key(five, NOW, &chrono::Utc),
            crate::picker::Effect::None
        );
    }

    #[test]
    fn collapsed_sections_show_a_count_and_no_number() {
        let mut snoozed = item("p4", "bench run", Display::Idle, "snoozed → 14:30");
        snoozed.park = Some(Park::Snoozed { until: NOW + 60 });
        let mut settled = item("p3", "release notes", Display::Idle, "settled");
        settled.park = Some(Park::Settled);
        let open = item("p1", "api refactor", Display::Idle, "idle");
        let mut picker = Picker::new(vec![open, snoozed, settled], None, &BTreeSet::new());
        let (lines, _) = render(&mut picker, 100, 14);
        assert!(
            lines[7].contains("▸ snoozed · 1") && !lines[7].contains('2'),
            "{}",
            lines[7]
        );
        assert!(lines[8].contains("▸ settled · 1"), "{}", lines[8]);
        assert!(!lines.iter().any(|l| l.contains("release notes")));
    }

    #[test]
    fn leaving_search_keeps_the_filter_visible_and_offers_to_clear_it() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('/'));
        let (lines, _) = render(&mut picker, 100, 14);
        assert!(lines[13].contains("esc done  ^j/^k move"), "{}", lines[13]);
        press(&mut picker, KeyCode::Char('f'));
        press(&mut picker, KeyCode::Char('l'));
        press(&mut picker, KeyCode::Esc);
        let (lines, _) = render(&mut picker, 120, 14);
        assert!(
            lines[1].contains("❯ fl") && lines[1].contains("1/3"),
            "{}",
            lines[1]
        );
        assert!(
            lines[13].contains("esc clear search  q close"),
            "{}",
            lines[13]
        );
    }

    #[test]
    fn closed_threads_are_marked_and_offer_reopen_and_forget() {
        let mut gone = item("p2", "old spike", Display::Idle, "idle · closed");
        gone.pane_id = None;
        let live = item("p1", "api refactor", Display::Idle, "idle");
        let mut picker = Picker::new(vec![live, gone], None, &BTreeSet::new());
        press(&mut picker, KeyCode::Char('j'));
        let (lines, _) = render(&mut picker, 110, 14);
        assert!(
            lines[7].contains("◌ old spike") && lines[7].contains("idle · closed"),
            "{}",
            lines[7]
        );
        assert!(
            lines[13].contains("enter reopen") && lines[13].contains("D forget"),
            "{}",
            lines[13]
        );
        press(&mut picker, KeyCode::Char('D'));
        let (lines, _) = render(&mut picker, 110, 14);
        assert!(
            lines[13].contains("forget “old spike”?  y yes"),
            "{}",
            lines[13]
        );
    }

    #[test]
    fn search_hints_name_what_enter_does_for_the_selection() {
        let mut gone = item("p2", "old spike", Display::Idle, "idle · closed");
        gone.pane_id = None;
        let mut picker = Picker::new(vec![gone], None, &BTreeSet::new());
        press(&mut picker, KeyCode::Char('/'));
        let (lines, _) = render(&mut picker, 110, 14);
        assert!(lines[13].contains("enter reopen"), "{}", lines[13]);
    }

    #[test]
    fn long_status_text_keeps_its_closed_or_stays_open_marker() {
        let mut parked = item(
            "p1",
            "bench run",
            Display::Idle,
            "snoozed → tmrw 09:00 · stays open",
        );
        parked.park = Some(Park::Snoozed { until: NOW + 60 });
        let mut picker = Picker::new(vec![parked], None, &BTreeSet::from([Section::Snoozed]));
        let (lines, _) = render(&mut picker, 100, 14);
        assert!(
            lines.iter().any(|l| l.contains("· stays open")),
            "{lines:#?}"
        );
    }

    #[test]
    fn narrow_popups_shorten_the_status_but_keep_its_marker() {
        let mut parked = item(
            "p1",
            "bench run",
            Display::Idle,
            "snoozed → tmrw 09:00 · stays open",
        );
        parked.park = Some(Park::Snoozed { until: NOW + 60 });
        let mut picker = Picker::new(vec![parked], None, &BTreeSet::from([Section::Snoozed]));
        let (lines, _) = render(&mut picker, 80, 14);
        assert!(lines[7].contains("… · stays open"), "{}", lines[7]);
    }

    #[test]
    fn notices_that_are_not_errors_have_no_error_mark() {
        let mut picker = picker();
        picker.inform("kept open: it is the last pane in its workspace".into());
        let (lines, _) = render(&mut picker, 110, 14);
        assert!(
            lines[13].contains("• kept open") && !lines[13].contains('✗'),
            "{}",
            lines[13]
        );
    }

    #[test]
    fn toggle_hints_name_what_the_key_will_do() {
        let mut settled = item("p1", "release notes", Display::Idle, "settled");
        settled.park = Some(Park::Settled);
        let mut snoozed = item("p2", "bench run", Display::Idle, "snoozed → 14:30");
        snoozed.park = Some(Park::Snoozed { until: NOW + 60 });
        let mut pinned_done = item("p3", "fix flaky test", Display::Done, "done 2m");
        pinned_done.pinned = true;
        let open = BTreeSet::from([Section::Snoozed, Section::Settled]);
        for (focus, expected) in [
            ("p1", "s unsettle"),
            ("p2", "z wake"),
            ("p3", "u read  p unpin"),
        ] {
            let list = vec![settled.clone(), snoozed.clone(), pinned_done.clone()];
            let mut picker = Picker::new(list, Some(focus), &open);
            let (lines, _) = render(&mut picker, 140, 14);
            assert!(lines[13].contains(expected), "{focus}: {}", lines[13]);
        }
    }

    #[test]
    fn a_selected_section_header_only_offers_what_works_on_it() {
        let mut settled = item("p1", "release notes", Display::Idle, "settled");
        settled.park = Some(Park::Settled);
        let mut picker = Picker::new(vec![settled], None, &BTreeSet::new());
        let (lines, _) = render(&mut picker, 110, 14);
        assert!(lines[13].contains("enter expand"), "{}", lines[13]);
        assert!(
            !lines[13].contains("settle") && !lines[13].contains("pin"),
            "{}",
            lines[13]
        );
        press(&mut picker, KeyCode::Char('/'));
        press(&mut picker, KeyCode::Char('n'));
        press(&mut picker, KeyCode::Esc);
        press(&mut picker, KeyCode::Char('k'));
        let (lines, _) = render(&mut picker, 110, 14);
        assert!(
            !lines[13].contains("enter"),
            "sections stay open while filtered: {}",
            lines[13]
        );
    }

    #[test]
    fn undo_is_offered_only_when_there_is_something_to_undo() {
        let mut picker = picker();
        let (lines, _) = render(&mut picker, 160, 14);
        assert!(!lines[13].contains("U undo"), "{}", lines[13]);
        picker.set_undoable(true);
        let (lines, _) = render(&mut picker, 160, 14);
        assert!(lines[13].contains("r rename  U undo"), "{}", lines[13]);
    }

    #[test]
    fn a_long_rename_keeps_its_end_and_hints_in_view_at_any_width() {
        for (width, text) in [
            (80, " and also the payment retries that keep timing out END"),
            (60, " 決済の再試行がタイムアウトし続ける問題を修正する終"),
            (48, " with a long tail END"),
        ] {
            let mut picker = picker();
            press(&mut picker, KeyCode::Char('r'));
            for c in text.chars() {
                press(&mut picker, KeyCode::Char(c));
            }
            let (lines, _) = render(&mut picker, width, 14);
            let end = text
                .chars()
                .rev()
                .take(3)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>();
            let footer = lines[13].replace(' ', "");
            assert!(
                footer.contains(&end.replace(' ', "")) && footer.contains("esccancel"),
                "{width}: {}",
                lines[13]
            );
        }
    }

    #[test]
    fn an_emoji_rename_keeps_its_hints_in_view() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('r'));
        for c in " ❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️❤️ end".chars()
        {
            press(&mut picker, KeyCode::Char(c));
        }
        let (lines, _) = render(&mut picker, 48, 14);
        assert!(
            lines[13].contains("end") && lines[13].contains("esc cancel"),
            "{}",
            lines[13]
        );
    }

    #[test]
    fn an_arabic_rename_keeps_its_hints_in_view() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('r'));
        for c in format!(" {} end", "لا".repeat(25)).chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        let (lines, _) = render(&mut picker, 48, 14);
        assert!(lines[13].contains("esc cancel"), "{}", lines[13]);
    }

    #[test]
    fn a_cleared_rename_shows_an_empty_title() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('r'));
        picker.key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            NOW,
            &chrono::Utc,
        );
        let (lines, _) = render(&mut picker, 80, 14);
        assert!(
            lines[13].contains("rename: automatic title  enter save"),
            "{}",
            lines[13]
        );
        let (lines, _) = render(&mut picker, 40, 14);
        assert!(
            lines[13].contains("rename: auto…  enter save  esc cancel"),
            "{}",
            lines[13]
        );
    }

    #[test]
    fn a_long_rename_keeps_its_end_in_view() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('r'));
        for c in " and also the payment retries that keep timing out".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        let (lines, _) = render(&mut picker, 80, 14);
        assert!(
            lines[13].contains("keep timing out") && lines[13].contains("esc cancel"),
            "{}",
            lines[13]
        );
    }

    #[test]
    fn narrow_footers_drop_whole_hints_but_keep_the_triage_keys_and_close() {
        let mut picker = picker();
        picker.set_undoable(true);
        let (lines, _) = render(&mut picker, 80, 14);
        let footer = lines[13].trim_end();
        assert!(
            footer.ends_with("esc/q close")
                && footer.contains("enter focus  s settle  z snooze  u read")
                && !footer.contains("1-9/0")
                && !footer.contains("j/k"),
            "{footer}"
        );
    }

    #[test]
    fn a_narrow_snooze_prompt_shortens_the_title_before_any_preset() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('z'));
        let (lines, _) = render(&mut picker, 100, 14);
        assert!(
            lines[13].contains("…”  1 1h") && lines[13].contains("5 next Mon 09:00  esc cancel"),
            "{}",
            lines[13]
        );
        let (lines, _) = render(&mut picker, 80, 14);
        assert!(
            lines[13].contains(" snooze  1 1h") && lines[13].contains("5 next Mon 09:00"),
            "{}",
            lines[13]
        );
    }

    #[test]
    fn a_long_forget_title_is_shortened_so_the_answer_shows() {
        let mut gone = item(
            "p2",
            &"a very long thread title ".repeat(6),
            Display::Idle,
            "idle · closed",
        );
        gone.pane_id = None;
        let mut picker = Picker::new(vec![gone], None, &BTreeSet::new());
        press(&mut picker, KeyCode::Char('D'));
        let (lines, _) = render(&mut picker, 80, 14);
        assert!(
            lines[13].contains("…”?  y yes  esc cancel"),
            "{}",
            lines[13]
        );
    }

    #[test]
    fn the_rename_prompt_puts_the_cursor_after_the_text() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('r'));
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut picker, 0, "P"))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!((cursor.x, cursor.y), (1 + 9 + 14, 13));
    }

    #[test]
    fn the_selected_row_is_never_dimmed() {
        let mut settled = item("p3", "release notes", Display::Idle, "settled");
        settled.park = Some(Park::Settled);
        let mut picker = Picker::new(
            vec![settled],
            Some("p3"),
            &BTreeSet::from([Section::Settled]),
        );
        let (lines, buffer) = render(&mut picker, 100, 14);
        let y = lines.iter().position(|l| l.contains("release")).unwrap();
        let x = lines[y]
            .find("release")
            .map(|byte| lines[y][..byte].chars().count())
            .unwrap();
        let cell = &buffer[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())];
        assert!(!cell.modifier.contains(Modifier::DIM));
    }

    #[test]
    fn opening_a_section_leaves_the_columns_where_they_were() {
        let mut snoozed = item(
            "p4",
            "bench run",
            Display::Idle,
            "snoozed · 2h · stays open",
        );
        snoozed.park = Some(Park::Snoozed { until: NOW + 60 });
        let open = item("p1", "api refactor", Display::Idle, "idle");
        let list = vec![open, snoozed];
        let (collapsed, _) = render(
            &mut Picker::new(list.clone(), None, &BTreeSet::new()),
            100,
            14,
        );
        let (expanded, _) = render(
            &mut Picker::new(list, None, &BTreeSet::from([Section::Snoozed])),
            100,
            14,
        );
        assert_eq!(collapsed[4], expanded[4]);
        assert_eq!(collapsed[6], expanded[6]);
    }

    #[test]
    fn the_rules_count_what_is_beyond_the_screen_both_ways() {
        let many: Vec<Item> = (0..12)
            .map(|i| {
                item(
                    &format!("p{i}"),
                    &format!("agent {i}"),
                    Display::Idle,
                    "idle",
                )
            })
            .collect();
        let mut picker = Picker::new(many, None, &BTreeSet::new());
        let (lines, _) = render(&mut picker, 100, 12);
        assert!(lines[10].contains("↓ 8 more"), "{}", lines[10]);
        assert!(!lines[5].contains("more"), "{}", lines[5]);
        press(&mut picker, KeyCode::Char('k'));
        let (lines, _) = render(&mut picker, 100, 12);
        assert!(lines[5].contains("↑ 8 more"), "{}", lines[5]);
        assert!(!lines[10].contains("more"), "{}", lines[10]);
    }

    #[test]
    fn long_titles_end_in_an_ellipsis_and_keep_the_focus_mark() {
        let mut long = item(
            "p1",
            &"a very long thread title that keeps going ".repeat(3),
            Display::Idle,
            "idle",
        );
        long.focused = true;
        let mut picker = Picker::new(vec![long], None, &BTreeSet::new());
        let (lines, _) = render(&mut picker, 80, 14);
        assert!(lines[6].contains("… (focused)"), "{}", lines[6]);
    }

    #[test]
    fn narrow_popups_give_the_title_room_before_hidden_statuses() {
        let mut snoozed = item(
            "p4",
            "bench run",
            Display::Idle,
            "snoozed · 3h · stays open",
        );
        snoozed.park = Some(Park::Snoozed { until: NOW + 60 });
        snoozed.location = "mobile · api".into();
        let mut focused = item("p1", "migrate auth to OAuth", Display::Working, "working");
        focused.focused = true;
        focused.pinned = true;
        let mut picker = Picker::new(vec![focused, snoozed], None, &BTreeSet::new());
        let (lines, _) = render(&mut picker, 79, 14);
        assert!(
            lines[6].contains("migrate auth to OAuth (focused)"),
            "{}",
            lines[6]
        );
        let mut picker = Picker::new(
            picker.items().to_vec(),
            None,
            &BTreeSet::from([Section::Snoozed]),
        );
        let (lines, _) = render(&mut picker, 79, 14);
        assert!(lines[8].contains("snoozed · stays open"), "{}", lines[8]);
    }

    #[test]
    fn the_rules_count_threads_not_rows() {
        let mut open = item("p1", "api refactor", Display::Idle, "idle");
        open.focused = true;
        let parked: Vec<Item> = (2..6)
            .map(|i| {
                let mut settled = item(
                    &format!("p{i}"),
                    &format!("old {i}"),
                    Display::Idle,
                    "settled",
                );
                settled.park = Some(Park::Settled);
                settled
            })
            .collect();
        let mut snoozed = item("p9", "bench run", Display::Idle, "snoozed · 2h");
        snoozed.park = Some(Park::Snoozed { until: NOW + 60 });
        let list = [vec![open, snoozed], parked].concat();
        let mut picker = Picker::new(list, Some("p1"), &BTreeSet::new());
        let (lines, _) = render(&mut picker, 100, 10);
        assert!(lines[8].contains("↓ 4 more"), "{lines:#?}");
        press(&mut picker, KeyCode::Char('j'));
        press(&mut picker, KeyCode::Char('j'));
        let (lines, _) = render(&mut picker, 100, 9);
        assert!(
            lines[6].contains("settled · 4") && lines[5].contains("↑ 2 more"),
            "{lines:#?}"
        );
    }

    #[test]
    fn a_cut_search_keeps_whole_graphemes() {
        assert_eq!(tail("xye\u{301}z", 3), "…e\u{301}z");
        assert_eq!(tail("xye\u{301}z", 2), "…z");
        assert_eq!(ellipsize("snoozed · 2h", 10), "snoozed…");
    }

    #[test]
    fn statuses_drop_their_countdown_before_their_state() {
        assert_eq!(
            fit_status("snoozed · 2h · stays open", 20),
            "snoozed · stays open"
        );
        let cut = fit_status("snoozed → tmrw 09:00 · stays open", 24);
        assert!(
            cut.starts_with("snoozed") && cut.ends_with("… · stays open"),
            "{cut}"
        );
        assert_eq!(fit_status("blocked 3m", 20), "blocked 3m");
    }

    #[test]
    fn the_no_match_hint_follows_the_mode() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('/'));
        press(&mut picker, KeyCode::Char('x'));
        press(&mut picker, KeyCode::Char('q'));
        let (lines, _) = render(&mut picker, 100, 14);
        assert!(lines.iter().any(|l| l.contains("^u clears the search")));
        assert!(
            !lines[13].contains("enter") && !lines[13].contains("^j/^k"),
            "{}",
            lines[13]
        );
        press(&mut picker, KeyCode::Esc);
        let (lines, _) = render(&mut picker, 100, 14);
        assert!(lines.iter().any(|l| l.contains("esc clears the search")));
    }

    #[test]
    fn threads_that_need_you_have_bold_titles() {
        let (lines, buffer) = render(&mut picker(), 100, 14);
        let at = |y: usize, word: &str| {
            let x = lines[y]
                .find(word)
                .map(|byte| lines[y][..byte].chars().count())
                .unwrap();
            buffer[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())].modifier
        };
        assert!(at(7, "fix flaky").contains(Modifier::BOLD));
        assert!(!at(6, "api refactor").contains(Modifier::BOLD));
    }

    #[test]
    fn the_rename_prompt_shows_the_title_being_typed() {
        let mut picker = picker();
        press(&mut picker, KeyCode::Char('r'));
        let (lines, _) = render(&mut picker, 120, 14);
        assert!(
            lines[13].contains("rename: fix flaky test")
                && lines[13].contains("enter save  esc cancel"),
            "{}",
            lines[13]
        );
    }
}
