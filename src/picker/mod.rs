use crate::apps::App;
use crate::history::History;
use nucleo_matcher::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};

mod render;
mod text;

pub use render::{draw, CardRect};
pub use text::TextRenderer;

pub const VISIBLE_ITEMS: usize = 8;

#[derive(Clone, Copy)]
struct ScoredMatch {
    app_idx: usize,
    score: u32,
}

pub struct PickerState {
    apps: Vec<App>,
    query: String,
    matches: Vec<ScoredMatch>,
    selected: usize,
    scroll_offset: usize,
    matcher: Matcher,
    history: History,
}

impl PickerState {
    pub fn new(history: History) -> Self {
        Self {
            apps: Vec::new(),
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            matcher: Matcher::new(Config::DEFAULT),
            history,
        }
    }

    pub fn set_apps(&mut self, apps: Vec<App>) {
        self.apps = apps;
        self.refilter();
    }

    pub fn loading(&self) -> bool {
        self.apps.is_empty()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn push_char(&mut self, c: char) {
        if !c.is_control() {
            self.query.push(c);
            self.refilter();
        }
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        let new = (self.selected as isize + delta).clamp(0, len - 1) as usize;
        self.selected = new;

        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + VISIBLE_ITEMS {
            self.scroll_offset = self.selected + 1 - VISIBLE_ITEMS;
        }
    }

    pub fn selected_app(&self) -> Option<&App> {
        self.matches
            .get(self.selected)
            .map(|m| &self.apps[m.app_idx])
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub fn visible_count(&self) -> usize {
        self.match_count().min(VISIBLE_ITEMS)
    }

    /// Count of **visible** rows that are previously-picked apps. Drives the
    /// "Recent" label section at the top of the list when the query is empty.
    pub fn visible_recent_count(&self) -> usize {
        if !self.query.is_empty() {
            return 0;
        }
        self.matches
            .iter()
            .take(VISIBLE_ITEMS)
            .filter(|m| self.history.has_picked(&self.apps[m.app_idx].exec))
            .count()
    }

    /// Set selection to an absolute index (clamped). Used by mouse clicks.
    pub fn select(&mut self, idx: usize) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = idx.min(self.matches.len() - 1);
    }

    /// True if the app at absolute index `abs` has been picked before — used
    /// to decide whether to render the × "forget" button on that row.
    pub fn is_history_row(&self, abs: usize) -> bool {
        self.matches
            .get(abs)
            .map(|m| self.history.has_picked(&self.apps[m.app_idx].exec))
            .unwrap_or(false)
    }

    /// Forget the app at absolute index `abs` from history. Rewrites the
    /// history file, refilters (so the row drops out of the Recent section),
    /// and returns the forgotten exec for logging/confirmation.
    pub fn forget_at(&mut self, abs: usize) -> Option<String> {
        let exec = self.matches.get(abs).map(|m| self.apps[m.app_idx].exec.clone())?;
        self.history.forget(&exec);
        self.refilter();
        Some(exec)
    }

    /// Scroll the viewport by `delta` rows without moving the selection.
    /// Selection may drift off-screen; up/down keys will snap it back.
    pub fn scroll_by(&mut self, delta: isize) {
        if self.matches.len() <= VISIBLE_ITEMS {
            return;
        }
        let max = self.matches.len().saturating_sub(VISIBLE_ITEMS) as isize;
        let new = (self.scroll_offset as isize + delta).clamp(0, max) as usize;
        self.scroll_offset = new;
    }

    pub fn visible(&self) -> impl Iterator<Item = (usize, &App, bool)> {
        let start = self.scroll_offset;
        let end = (start + VISIBLE_ITEMS).min(self.matches.len());
        self.matches[start..end].iter().enumerate().map(move |(i, m)| {
            let absolute = start + i;
            (absolute, &self.apps[m.app_idx], absolute == self.selected)
        })
    }

    /// Rebuild the filtered+scored list from the current query. Empty query
    /// orders by history only (most-used + most-recent on top); non-empty
    /// query uses fuzzy score + history bonus.
    fn refilter(&mut self) {
        if self.query.is_empty() {
            let mut scored: Vec<ScoredMatch> = self
                .apps
                .iter()
                .enumerate()
                .map(|(i, app)| ScoredMatch {
                    app_idx: i,
                    score: self.history.score_bonus(&app.exec),
                })
                .collect();
            scored.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| self.apps[a.app_idx].name.cmp(&self.apps[b.app_idx].name))
            });
            self.matches = scored;
        } else {
            let pattern =
                Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
            let mut buf = Vec::new();
            let mut scored: Vec<ScoredMatch> = self
                .apps
                .iter()
                .enumerate()
                .filter_map(|(i, app)| {
                    buf.clear();
                    let haystack = Utf32Str::new(&app.name, &mut buf);
                    pattern.score(haystack, &mut self.matcher).map(|s| ScoredMatch {
                        app_idx: i,
                        score: s.saturating_add(self.history.score_bonus(&app.exec)),
                    })
                })
                .collect();
            scored.sort_by(|a, b| b.score.cmp(&a.score));
            self.matches = scored;
        }
        self.selected = 0;
        self.scroll_offset = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_apps(names: &[&str]) -> Vec<App> {
        names
            .iter()
            .map(|n| App {
                name: (*n).to_string(),
                exec: (*n).to_string(),
                icon: None,
            })
            .collect()
    }

    #[test]
    fn empty_query_lists_all_apps() {
        let mut p = PickerState::new(History::default());
        p.set_apps(mock_apps(&["firefox", "kitty", "discord"]));
        assert_eq!(p.matches.len(), 3);
    }

    #[test]
    fn query_filters_and_orders_by_score() {
        let mut p = PickerState::new(History::default());
        p.set_apps(mock_apps(&["firefox", "kitty", "discord", "kicad"]));
        for c in "ki".chars() {
            p.push_char(c);
        }
        // kitty and kicad should match; both start with "ki" so both score high
        let names: Vec<&str> = p.matches.iter().map(|m| p.apps[m.app_idx].name.as_str()).collect();
        assert!(names.contains(&"kitty"));
        assert!(names.contains(&"kicad"));
        assert!(!names.contains(&"firefox"));
    }

    #[test]
    fn move_selection_clamps_to_bounds() {
        let mut p = PickerState::new(History::default());
        p.set_apps(mock_apps(&["a", "b", "c"]));
        p.move_selection(-5);
        assert_eq!(p.selected, 0);
        p.move_selection(100);
        assert_eq!(p.selected, 2);
    }

    #[test]
    fn move_selection_scrolls_viewport() {
        let mut p = PickerState::new(History::default());
        let names: Vec<String> = (0..20).map(|i| format!("app{i}")).collect();
        p.set_apps(
            names
                .iter()
                .map(|n| App {
                    name: n.clone(),
                    exec: n.clone(),
                    icon: None,
                })
                .collect(),
        );
        p.move_selection(10);
        assert_eq!(p.selected, 10);
        assert!(p.scroll_offset > 0);
        assert!(p.selected >= p.scroll_offset);
        assert!(p.selected < p.scroll_offset + VISIBLE_ITEMS);
    }
}
