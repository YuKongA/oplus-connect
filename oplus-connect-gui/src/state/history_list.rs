use super::{AppState, ClipboardFilter, ClipboardRecord, ClipboardSource, NotificationRecord};
use gpui_kit::{ListAlignment, ListOffset, ListState, Pixels, SharedString, px};
use std::cell::RefCell;

/// Source indices change on every prepend. Keys identify rows for GPUI's height
/// cache and scroll anchor; indices make rendering a visible row constant-time.
struct Row {
    key: SharedString,
    source_index: usize,
}

struct Projection {
    rows: Vec<Row>,
    scratch: Vec<Row>,
    dirty: bool,
    filter: ClipboardFilter,
    query: String,
    rem_size: Option<Pixels>,
}

pub(super) struct HistoryList {
    native: ListState,
    projection: RefCell<Projection>,
}

impl Default for HistoryList {
    fn default() -> Self {
        Self {
            native: ListState::new(0, ListAlignment::Top, px(150.)),
            projection: RefCell::new(Projection {
                rows: Vec::new(),
                scratch: Vec::new(),
                dirty: true,
                filter: ClipboardFilter::All,
                query: String::new(),
                rem_size: None,
            }),
        }
    }
}

impl HistoryList {
    pub(super) fn invalidate(&mut self) {
        self.projection.get_mut().dirty = true;
    }

    pub(super) fn invalidate_row(&mut self, key: &SharedString) {
        let p = self.projection.get_mut();
        p.dirty = true;
        // An update to a notification at the head need not change its position
        // or count, but its new title/body can wrap to a different height.
        if let Some(ix) = p.rows.iter().position(|row| &row.key == key) {
            self.native.remeasure_items(ix..ix + 1);
        }
    }

    fn prepare(
        &self,
        filter: ClipboardFilter,
        query: &str,
        rem_size: Pixels,
        fill: impl FnOnce(&str, &mut Vec<Row>),
    ) -> ListState {
        let p = &mut *self.projection.borrow_mut();
        let query = query.trim();
        if p.dirty || p.filter != filter || p.query != query {
            fill(&query.to_lowercase(), &mut p.scratch);
            reconcile(&self.native, &p.rows, &p.scratch);
            std::mem::swap(&mut p.rows, &mut p.scratch);
            p.scratch.clear();
            p.dirty = false;
            p.filter = filter;
            if p.query != query {
                query.clone_into(&mut p.query);
            }
        }
        // List itself invalidates wrap measurements when its content width changes.
        if p.rem_size
            .replace(rem_size)
            .is_some_and(|prev| prev != rem_size)
        {
            self.native.remeasure();
        }
        self.native.clone()
    }

    fn source_index(&self, ix: usize) -> usize {
        self.projection.borrow().rows[ix].source_index
    }
}

fn reconcile(list: &ListState, old: &[Row], new: &[Row]) {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(a, b)| a.key == b.key)
        .count();
    if prefix == old.len() && prefix == new.len() {
        return;
    }
    let top = list.logical_scroll_top();
    let old_anchor = old.get(top.item_ix);
    // At capacity, an insert drops the oldest row. Remove that tail first so
    // the normal prefix/suffix splice keeps every surviving row's measurement.
    let old_end = new
        .last()
        .and_then(|last| old.iter().rposition(|r| r.key == last.key))
        .map_or(old.len(), |ix| ix + 1);
    if old_end < old.len() {
        list.splice(old_end..old.len(), 0);
    }
    let suffix = old[prefix..old_end]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a.key == b.key)
        .count();
    if prefix + suffix != old_end || prefix + suffix != new.len() {
        list.splice(prefix..old_end - suffix, new.len() - prefix - suffix);
    }
    // Stay pinned to newest at the top. Else keep the same record AND pixel
    // offset across prepends, filters and moves even inside the spliced range.
    let target = if top == ListOffset::default() {
        Some(top)
    } else {
        old_anchor
            .and_then(|anchor| new.iter().position(|r| r.key == anchor.key))
            .map(|item_ix| ListOffset { item_ix, ..top })
    };
    if let Some(target) = target
        && list.logical_scroll_top() != target
    {
        list.scroll_to(target);
    }
}

impl AppState {
    pub(crate) fn clipboard_list_state(&self, rem: Pixels) -> ListState {
        self.clipboard_list
            .prepare(self.clipboard_filter, &self.search_query, rem, |q, rows| {
                for (source_index, record) in self.clipboard_history.iter().enumerate() {
                    let include = match self.clipboard_filter {
                        ClipboardFilter::All => true,
                        ClipboardFilter::Favorites => record.is_favorite,
                        ClipboardFilter::Local => matches!(record.source, ClipboardSource::Local),
                        ClipboardFilter::Remote => {
                            matches!(record.source, ClipboardSource::Remote(_))
                        }
                    };
                    if include && (q.is_empty() || record.content.to_lowercase().contains(q)) {
                        rows.push(Row {
                            key: record.id.clone(),
                            source_index,
                        });
                    }
                }
            })
    }

    pub(crate) fn clipboard_row(&self, index: usize) -> &ClipboardRecord {
        &self.clipboard_history[self.clipboard_list.source_index(index)]
    }

    pub(crate) fn notification_list_state(&self, rem: Pixels) -> ListState {
        self.notification_list
            .prepare(ClipboardFilter::All, &self.search_query, rem, |q, rows| {
                for (source_index, record) in self.notification_history.iter().enumerate() {
                    if q.is_empty()
                        || record.title.to_lowercase().contains(q)
                        || record.content.to_lowercase().contains(q)
                        || record.label.to_lowercase().contains(q)
                    {
                        rows.push(Row {
                            key: record.key.clone(),
                            source_index,
                        });
                    }
                }
            })
    }

    pub(crate) fn notification_row(&self, index: usize) -> &NotificationRecord {
        &self.notification_history[self.notification_list.source_index(index)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_and_immediate_duplicate_clipboards_do_not_change_history() {
        let mut app = AppState::new();
        assert!(!app.add_clipboard_record(" \n\t", true));
        assert!(app.add_clipboard_record(" 中🦀 ", false));
        let original_id = app.clipboard_history[0].id.clone();
        assert!(!app.add_clipboard_record(" 中🦀 ", true));
        let row = &app.clipboard_history[0];
        assert_eq!(row.id, original_id);
        assert_eq!(row.content, " 中🦀 ");
        assert_eq!(row.char_count, 4);
        assert_eq!(row.source, ClipboardSource::Remote("移动设备".into()));
        assert_eq!(app.clipboard_history.len(), 1);
    }

    #[test]
    fn clipboard_projection_handles_filter_changes_and_hidden_prepends() {
        let mut app = AppState::new();
        app.add_clipboard_record("Local", true);
        let local = app.clipboard_history[0].id.clone();
        app.add_clipboard_record("Remote", false);
        app.clipboard_filter = ClipboardFilter::Local;
        let list = app.clipboard_list_state(px(16.));
        assert_eq!(list.item_count(), 1);
        assert_eq!(app.clipboard_row(0).id, local);
        app.add_clipboard_record("Second remote", false);
        app.clipboard_list_state(px(16.));
        assert_eq!(app.clipboard_row(0).id, local);
        app.clipboard_filter = ClipboardFilter::Remote;
        app.search_query = " SECOND ".into();
        app.clipboard_list_state(px(16.));
        assert_eq!(list.item_count(), 1);
        assert_eq!(app.clipboard_row(0).content, "Second remote");
        app.toggle_clipboard_favorite(&local);
        app.clipboard_filter = ClipboardFilter::Favorites;
        app.search_query.clear();
        app.clear_clipboard_records();
        app.clipboard_list_state(px(16.));
        assert_eq!(list.item_count(), 1);
        assert_eq!(app.clipboard_row(0).id, local);
        app.delete_clipboard_record(&local);
        app.clipboard_list_state(px(16.));
        assert_eq!(list.item_count(), 0);
    }

    #[test]
    fn capped_prepend_preserves_the_scrolled_record_and_pins_the_top() {
        let mut app = AppState::new();
        for n in 0..200 {
            app.add_clipboard_record(&n.to_string(), true);
        }
        let list = app.clipboard_list_state(px(16.));
        let offset = ListOffset {
            item_ix: 19,
            offset_in_item: px(7.),
        };
        list.scroll_to(offset);
        let anchor = app.clipboard_row(19).id.clone();
        app.add_clipboard_record("200", true);
        app.clipboard_list_state(px(16.));
        assert_eq!(list.item_count(), 200);
        assert_eq!(
            list.logical_scroll_top(),
            ListOffset {
                item_ix: 20,
                ..offset
            }
        );
        assert_eq!(app.clipboard_row(20).id, anchor);
        list.scroll_to(ListOffset::default());
        app.add_clipboard_record("201", true);
        app.clipboard_list_state(px(16.));
        assert_eq!(list.logical_scroll_top(), ListOffset::default());
        assert_eq!(app.clipboard_row(0).content, "201");
    }

    #[test]
    fn notification_replacement_reorders_and_updates_filtered_rows_without_losing_anchor() {
        let record = |key: &str, content: &str| NotificationRecord {
            id: content.into(),
            key: key.into(),
            title: "标题".into(),
            label: "应用".into(),
            content: content.into(),
            icon: None,
            timestamp: chrono::Local::now(),
        };
        let mut app = AppState::new();
        for key in ["a", "b", "c"] {
            app.add_notification_record(record(key, key));
        }
        let list = app.notification_list_state(px(16.));
        let offset = ListOffset {
            item_ix: 1,
            offset_in_item: px(3.),
        };
        list.scroll_to(offset);
        app.add_notification_record(record("a", "changed\nmultiline"));
        app.notification_list_state(px(16.));
        assert_eq!(list.item_count(), 3);
        assert_eq!(
            list.logical_scroll_top(),
            ListOffset {
                item_ix: 2,
                ..offset
            }
        );
        assert_eq!(app.notification_row(2).key, "b");
        app.add_notification_record(record("a", "changed again"));
        app.search_query = "AGAIN".into();
        app.notification_list_state(px(18.));
        assert_eq!(list.item_count(), 1);
        assert_eq!(app.notification_row(0).content, "changed again");
        app.clear_notifications();
        app.notification_list_state(px(18.));
        assert_eq!(list.item_count(), 0);
    }
}
