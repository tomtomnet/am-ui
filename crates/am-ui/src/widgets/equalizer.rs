//! The Equalizer dialog: an on/off switch, the frequency response of the active preset, and the
//! presets (make one active, rename, move, delete, paste a new one, import the other
//! application's).
//!
//! What the dialog shows is a `View` made on a worker thread from what is saved; the widgets are
//! only ever set from one. The user's changes run one after the other, in the order they were
//! made, and each is followed by a fresh `View`: the dialog shows what is saved, not what was
//! asked for.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use amui::eq;
use gtk::{gdk, gio, glib, pango};

use crate::app::ctx;
use crate::equalizer::{EqError, EqualizerService, Preset, Snapshot, signed};
use crate::signal::background;
use crate::widgets::eq_graph::{EqGraph, Kind};

/// Points of the curve in the graph, and of the one in a preset's row.
const CURVE_POINTS: usize = eq::CURVE_POINTS;
const SPARK_POINTS: usize = 64;
/// How often the note about the sink is brought up to date while the dialog is open.
const NOTE_INTERVAL: Duration = Duration::from_secs(5);
/// How long the text of a new preset rests before it is read.
const TYPING_PAUSE: Duration = Duration::from_millis(150);

// --- what the dialog shows -------------------------------------------------------------------

/// A preset as its row shows it.
#[derive(Debug, Clone, PartialEq)]
struct RowView {
    id: String,
    name: String,
    /// "15 filters · Preamp -10.4 dB".
    detail: String,
    /// Where it is from: "Pasted", "From the old clone", "Sample".
    source: String,
    /// Its response, for the sparkline.
    spark: Vec<f32>,
}

/// Everything the dialog shows.
#[derive(Debug, Clone, PartialEq)]
struct View {
    enabled: bool,
    /// The id of the active preset.
    active: Option<String>,
    rows: Vec<RowView>,
    /// The response of the active preset; empty without one.
    curve: Vec<f32>,
    can_import: bool,
    /// The service's line about the sink (`EqualizerService::live_note`), when it has one.
    note: Option<String>,
}

/// "15 filters · Preamp -10.4 dB".
fn detail(filters: usize, preamp: f64) -> String {
    let filters = match filters {
        0 => "No filters".to_string(),
        1 => "1 filter".to_string(),
        count => format!("{count} filters"),
    };
    format!("{filters} · Preamp {} dB", signed(preamp))
}

/// The first letter as a capital: the engine's messages start small.
fn sentence(text: &str) -> String {
    let mut letters = text.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

fn source_label(source: &str) -> String {
    match source {
        "pasted" => "Pasted".to_string(),
        "bridge" => "From the old clone".to_string(),
        "sample" => "Sample".to_string(),
        other => sentence(other),
    }
}

/// The response of a preset in dB at `points` frequencies from 20 Hz to 20 kHz.
fn response(preset: &Preset, points: usize) -> Vec<f32> {
    eq::response(Some(preset), &eq::default_freqs(points)).into_iter().map(|db| db as f32).collect()
}

impl View {
    /// What is saved now. For a worker thread: the state file is read.
    fn of(service: &dyn EqualizerService) -> View {
        View::new(&service.snapshot(), service.can_import(), service.live_note())
    }

    fn new(snapshot: &Snapshot, can_import: bool, note: Option<String>) -> View {
        let active = snapshot.presets.iter().find(|preset| Some(&preset.id) == snapshot.active.as_ref());
        let row = |preset: &Preset| RowView {
            id: preset.id.clone(),
            name: preset.name.clone(),
            detail: detail(preset.filters.len(), preset.preamp),
            source: source_label(&preset.source),
            spark: response(preset, SPARK_POINTS),
        };
        View {
            enabled: snapshot.enabled,
            active: active.map(|preset| preset.id.clone()),
            rows: snapshot.presets.iter().map(row).collect(),
            curve: active.map(|preset| response(preset, CURVE_POINTS)).unwrap_or_default(),
            can_import,
            note,
        }
    }

    fn row(&self, id: &str) -> Option<&RowView> {
        self.rows.iter().find(|row| row.id == id)
    }

    fn active_row(&self) -> Option<&RowView> {
        self.active.as_deref().and_then(|id| self.row(id))
    }

    /// The graph in words.
    fn description(&self) -> String {
        let Some(row) = self.active_row() else { return "Frequency response: flat, no preset is active".to_string() };
        let lowest = self.curve.iter().copied().fold(f32::INFINITY, f32::min);
        let highest = self.curve.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        format!(
            "Frequency response of {}: between {} and {} dB{}",
            row.name,
            signed(lowest.into()),
            signed(highest.into()),
            if self.enabled { "" } else { ", off" }
        )
    }
}

/// What the text of a new preset is.
#[derive(Debug, Clone, PartialEq)]
enum Pasted {
    Nothing,
    Preset {
        curve: Vec<f32>,
        detail: String,
    },
    /// Not a preset: why, and on which line (counted from 1) when it is one line's fault.
    Problem {
        message: String,
        line: Option<usize>,
    },
}

fn pasted(text: &str) -> Pasted {
    if text.trim().is_empty() {
        return Pasted::Nothing;
    }
    match eq::parse(text) {
        Ok((preamp, filters)) => {
            let detail = detail(filters.len(), preamp);
            Pasted::Preset { curve: response(&Preset::of(preamp, filters), CURVE_POINTS), detail }
        }
        Err(error) => Pasted::Problem { message: sentence(&error.to_string()), line: error.line() },
    }
}

// --- the dialog --------------------------------------------------------------------------------

/// What a job brings back from its worker thread.
struct Report {
    view: View,
    /// Something to tell the user: what failed, or what an import found.
    said: Option<String>,
    failed: bool,
}

type Work = Box<dyn FnOnce(&dyn EqualizerService) -> Report + Send>;
type Then = Box<dyn FnOnce(&Rc<Panel>, bool)>;

/// The open dialog.
struct Panel {
    service: Arc<dyn EqualizerService>,
    toasts: adw::ToastOverlay,
    pages: adw::NavigationView,
    title: adw::WindowTitle,
    switch: gtk::Switch,
    spinner: adw::Spinner,
    graph: EqGraph,
    /// The line about the sink under the graph; hidden when the service has nothing to say.
    note: gtk::Label,
    import: gtk::Button,
    scroller: gtk::ScrolledWindow,
    list: gtk::ListBox,
    empty: adw::StatusPage,
    rows: RefCell<Vec<Row>>,
    /// What the widgets show.
    view: RefCell<Option<View>>,
    /// The widgets are being set from a view: what changes then is not the user's doing.
    setting: Cell<bool>,
    /// Jobs that wait for the running one.
    jobs: RefCell<VecDeque<(Work, Then)>>,
    running: Cell<bool>,
    /// Until the dialog is closed.
    open: Cell<bool>,
    /// Brings the note up to date.
    timer: RefCell<Option<glib::SourceId>>,
}

impl Panel {
    // --- jobs: one at a time, each followed by a fresh view

    fn submit(self: &Rc<Self>, work: Work, then: Then) {
        self.jobs.borrow_mut().push_back((work, then));
        if !self.running.replace(true) {
            // applying can take a moment: say that something is going on when it does
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(Duration::from_millis(400), move || {
                if let Some(panel) = weak.upgrade()
                    && panel.running.get()
                {
                    panel.spinner.set_visible(true);
                }
            });
            self.run_next();
        }
    }

    fn run_next(self: &Rc<Self>) {
        let next = self.jobs.borrow_mut().pop_front();
        let Some((work, then)) = next else {
            self.running.set(false);
            self.spinner.set_visible(false);
            return;
        };
        let (service, panel) = (self.service.clone(), self.clone());
        background(
            move || work(service.as_ref()),
            move |report| {
                panel.show(report.view);
                if let Some(message) = &report.said {
                    panel.say(message);
                }
                then(&panel, !report.failed);
                panel.run_next();
            },
        );
    }

    /// Look at what is saved again; nothing is changed.
    fn refresh(self: &Rc<Self>, then: impl FnOnce(&Rc<Panel>) + 'static) {
        self.submit(
            Box::new(|service| Report { view: View::of(service), said: None, failed: false }),
            Box::new(move |panel, _| then(panel)),
        );
    }

    /// Make a change, then show what is saved. `work` may bring something to tell the user; when
    /// it fails, the reason is told with `what` before it.
    fn change(
        self: &Rc<Self>,
        what: &'static str,
        work: impl FnOnce(&dyn EqualizerService) -> Result<Option<String>, EqError> + Send + 'static,
    ) {
        self.change_then(what, work, |_, _| {});
    }

    /// `change`, and `then` hears whether it worked.
    fn change_then(
        self: &Rc<Self>,
        what: &'static str,
        work: impl FnOnce(&dyn EqualizerService) -> Result<Option<String>, EqError> + Send + 'static,
        then: impl FnOnce(&Rc<Panel>, bool) + 'static,
    ) {
        let work = move |service: &dyn EqualizerService| {
            let outcome = work(service);
            let view = View::of(service);
            match outcome {
                Ok(said) => Report { view, said, failed: false },
                Err(error) => Report { view, said: Some(format!("{what}: {error}")), failed: true },
            }
        };
        self.submit(Box::new(work), Box::new(then));
    }

    /// A message for the user: in the dialog while it is open, in the window afterwards.
    fn say(&self, message: &str) {
        if self.open.get() {
            // names and the engine's reasons are text, not markup
            self.toasts.add_toast(adw::Toast::builder().title(message).use_markup(false).timeout(4).build());
        } else {
            ctx().toast.emit(&message.to_string());
        }
    }

    // --- the view

    fn show(self: &Rc<Self>, view: View) {
        // while changes wait, the switch is where the user left it: the last of them settles it
        let settled = self.jobs.borrow().is_empty();
        let switched = settled && self.switch.is_active() != view.enabled;
        if !switched && self.view.borrow().as_ref() == Some(&view) {
            return;
        }
        self.setting.set(true);
        if settled {
            self.switch.set_active(view.enabled);
        }
        self.title.set_subtitle(view.active_row().map_or("", |row| row.name.as_str()));
        self.graph.set_curve(&view.curve);
        self.graph.set_dimmed(!view.enabled || view.active.is_none());
        self.graph.set_description(&view.description());
        self.note.set_visible(view.note.is_some());
        self.note.set_text(view.note.as_deref().unwrap_or_default());
        self.import.set_visible(view.can_import);

        let same_rows = {
            let rows = self.rows.borrow();
            rows.len() == view.rows.len() && rows.iter().zip(&view.rows).all(|(row, shown)| row.id == shown.id)
        };
        if !same_rows {
            self.list.remove_all();
            let rows: Vec<Row> = view.rows.iter().map(|shown| Row::new(self, &shown.id)).collect();
            for row in &rows {
                self.list.append(&row.row);
            }
            self.rows.replace(rows);
        }
        let last = view.rows.len().saturating_sub(1);
        for (index, (row, shown)) in self.rows.borrow().iter().zip(&view.rows).enumerate() {
            let active = view.active.as_deref() == Some(shown.id.as_str());
            row.show(shown, active, view.enabled, index == 0, index == last);
        }
        self.scroller.set_visible(!view.rows.is_empty());
        self.empty.set_visible(view.rows.is_empty());
        self.empty.set_description(Some(if view.can_import {
            "Paste one from AutoEQ, EqualizerAPO or PEQdB, or import those of the old clone."
        } else {
            "Paste one from AutoEQ, EqualizerAPO or PEQdB."
        }));
        self.setting.set(false);
        self.view.replace(Some(view));
    }

    fn name_of(&self, id: &str) -> Option<String> {
        self.view.borrow().as_ref().and_then(|view| view.row(id)).map(|row| row.name.clone())
    }

    // --- what the user can do

    fn activate(self: &Rc<Self>, id: &str) {
        let id = id.to_string();
        self.change("Equalizer", move |service| service.activate(&id).map(|()| None));
    }

    /// One place up or down the list.
    fn move_preset(self: &Rc<Self>, id: &str, up: bool) {
        let id = id.to_string();
        self.change("Move", move |service| {
            // where it is by now: earlier moves may still have been on their way
            let order = service.snapshot().order;
            let index =
                order.iter().position(|known| *known == id).ok_or(EqError::Other("no such preset".to_string()))?;
            service.move_to(&id, if up { index.saturating_sub(1) } else { index + 1 }).map(|()| None)
        });
    }

    fn import(self: &Rc<Self>) {
        self.change("Import", |service| {
            service.import().map(|count| {
                Some(match count {
                    0 => "Nothing new to import from the old clone".to_string(),
                    1 => "Imported 1 preset from the old clone".to_string(),
                    count => format!("Imported {count} presets from the old clone"),
                })
            })
        });
    }

    fn rename(self: &Rc<Self>, id: &str) {
        let Some(name) = self.name_of(id) else { return };
        let entry = adw::EntryRow::new();
        entry.set_title("Name");
        entry.set_text(&name);
        let group = adw::PreferencesGroup::new();
        group.add(&entry);
        group.set_size_request(340, -1);

        let dialog = adw::AlertDialog::new(Some("Rename Preset"), None);
        dialog.set_extra_child(Some(&group));
        dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        {
            let weak = dialog.downgrade();
            entry.connect_changed(move |entry| {
                if let Some(dialog) = weak.upgrade() {
                    dialog.set_response_enabled("rename", !entry.text().trim().is_empty());
                }
            });
            // Enter in the field confirms, as the default button would
            let weak = dialog.downgrade();
            entry.connect_entry_activated(move |entry| {
                if let Some(dialog) = weak.upgrade()
                    && !entry.text().trim().is_empty()
                {
                    dialog.set_close_response("rename");
                    dialog.close();
                }
            });
        }
        let (weak, id, field) = (Rc::downgrade(self), id.to_string(), entry.downgrade());
        dialog.connect_response(None, move |_, response| {
            let name = field.upgrade().map(|field| field.text().trim().to_string()).unwrap_or_default();
            if let Some(panel) = weak.upgrade()
                && response == "rename"
                && !name.is_empty()
            {
                let id = id.clone();
                panel.change("Rename", move |service| service.rename(&id, &name).map(|()| None));
            }
        });
        dialog.present(Some(&self.pages));
        entry.grab_focus();
    }

    fn confirm_delete(self: &Rc<Self>, id: &str) {
        let Some(name) = self.name_of(id) else { return };
        let sounding =
            self.view.borrow().as_ref().is_some_and(|view| view.enabled && view.active.as_deref() == Some(id));
        let dialog = adw::AlertDialog::new(
            Some(&format!("Delete “{name}”?")),
            Some(if sounding {
                "It is the active preset: the equalizer goes flat. The preset cannot be brought back."
            } else {
                "The preset cannot be brought back."
            }),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let (weak, id) = (Rc::downgrade(self), id.to_string());
        dialog.connect_response(None, move |_, response| {
            if let Some(panel) = weak.upgrade()
                && response == "delete"
            {
                let id = id.clone();
                panel.change("Delete", move |service| service.delete(&id).map(|()| None));
            }
        });
        dialog.present(Some(&self.pages));
    }
}

// --- a preset in the list ----------------------------------------------------------------------

struct Row {
    id: String,
    row: adw::ActionRow,
    spark: EqGraph,
    source: gtk::Label,
    check: gtk::Image,
    menu: gtk::MenuButton,
    move_up: gio::SimpleAction,
    move_down: gio::SimpleAction,
}

impl Row {
    fn new(panel: &Rc<Panel>, id: &str) -> Row {
        let row = adw::ActionRow::new();
        // the name is the user's: it is text, not markup
        row.set_use_markup(false);
        row.set_title_lines(1);
        row.set_subtitle_lines(1);
        row.set_activatable(true);
        let spark = EqGraph::new(Kind::Spark);
        spark.set_valign(gtk::Align::Center);
        row.add_prefix(&spark);
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.add_css_class("accent");
        check.update_property(&[gtk::accessible::Property::Label("Active")]);
        row.add_suffix(&check);
        let source = gtk::Label::new(None);
        source.add_css_class("dim-label");
        source.add_css_class("caption");
        row.add_suffix(&source);

        let model = gio::Menu::new();
        let actions = gio::SimpleActionGroup::new();
        let entry = |section: &gio::Menu, label: &str, name: &str, run: fn(&Rc<Panel>, &str)| {
            let action = gio::SimpleAction::new(name, None);
            let (weak, id) = (Rc::downgrade(panel), id.to_string());
            action.connect_activate(move |_, _| {
                if let Some(panel) = weak.upgrade() {
                    run(&panel, &id);
                }
            });
            actions.add_action(&action);
            section.append(Some(label), Some(&format!("preset.{name}")));
            action
        };
        let section = gio::Menu::new();
        entry(&section, "Rename…", "rename", |panel, id| panel.rename(id));
        model.append_section(None, &section);
        let section = gio::Menu::new();
        let move_up = entry(&section, "Move Up", "move-up", |panel, id| panel.move_preset(id, true));
        let move_down = entry(&section, "Move Down", "move-down", |panel, id| panel.move_preset(id, false));
        model.append_section(None, &section);
        let section = gio::Menu::new();
        entry(&section, "Delete…", "delete", |panel, id| panel.confirm_delete(id));
        model.append_section(None, &section);
        row.insert_action_group("preset", Some(&actions));
        let menu = gtk::MenuButton::new();
        menu.set_icon_name("view-more-symbolic");
        menu.set_tooltip_text(Some("More"));
        menu.set_menu_model(Some(&model));
        menu.add_css_class("flat");
        menu.set_valign(gtk::Align::Center);
        row.add_suffix(&menu);

        let (weak, activated) = (Rc::downgrade(panel), id.to_string());
        row.connect_activated(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.activate(&activated);
            }
        });
        // the menu on a right click as well
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_SECONDARY);
        let weak = menu.downgrade();
        click.connect_pressed(move |gesture, _, _, _| {
            if let Some(menu) = weak.upgrade() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                menu.popup();
            }
        });
        row.add_controller(click);
        Row { id: id.to_string(), row, spark, source, check, menu, move_up, move_down }
    }

    /// `active`: it is the preset of the equalizer; `on`: the equalizer is on.
    fn show(&self, shown: &RowView, active: bool, on: bool, first: bool, last: bool) {
        self.row.set_title(&shown.name);
        self.row.set_subtitle(&shown.detail);
        self.source.set_text(&shown.source);
        self.spark.set_curve(&shown.spark);
        self.spark.set_dimmed(!(active && on));
        self.check.set_visible(active);
        if active {
            self.row.add_css_class("eq-active");
        } else {
            self.row.remove_css_class("eq-active");
        }
        self.move_up.set_enabled(!first);
        self.move_down.set_enabled(!last);
    }
}

// --- the page for a new preset -----------------------------------------------------------------

/// A name, the text, and what the text gives: its curve, or the line that is not understood.
struct AddPage {
    name: adw::EntryRow,
    text: gtk::TextView,
    /// Underlines the line that is not understood.
    problem: Option<gtk::TextTag>,
    /// What such text looks like, shown while there is none.
    example: gtk::Label,
    preview: EqGraph,
    result: gtk::Label,
    add: gtk::Button,
    /// The text is a preset.
    readable: Cell<bool>,
    /// Counts the changes of the text: what was read of an earlier text is dropped.
    serial: Cell<u64>,
    /// Waits for the typing to pause.
    pause: RefCell<Option<glib::SourceId>>,
}

impl AddPage {
    /// Put the page on top of the dialog's. `start` fills the two fields (screenshots).
    fn open(panel: &Rc<Panel>, start: Option<(&str, &str)>) {
        let name = adw::EntryRow::new();
        name.set_title("Name");
        let fields = adw::PreferencesGroup::new();
        fields.add(&name);

        let caption = |text: &str| {
            let label = gtk::Label::new(Some(text));
            label.add_css_class("caption-heading");
            label.add_css_class("dim-label");
            label.set_xalign(0.0);
            label
        };
        let text = gtk::TextView::new();
        let problem = text.buffer().create_tag(Some("problem"), &[("underline", &pango::Underline::Error)]);
        text.set_monospace(true);
        text.set_wrap_mode(gtk::WrapMode::WordChar);
        text.set_accepts_tab(false);
        text.add_css_class("inline");
        text.set_top_margin(10);
        text.set_bottom_margin(10);
        text.set_left_margin(12);
        text.set_right_margin(12);
        text.update_property(&[gtk::accessible::Property::Label("Filter text")]);
        let example = gtk::Label::new(Some(
            "Preamp: -6.2 dB\nFilter 1: ON PK Fc 105 Hz Gain -2.6 dB Q 0.70\nFilter 2: ON HSC Fc 10000 Hz Gain 3.0 dB Q 0.70",
        ));
        example.add_css_class("monospace");
        example.add_css_class("dim-label");
        example.set_xalign(0.0);
        example.set_halign(gtk::Align::Start);
        example.set_valign(gtk::Align::Start);
        example.set_margin_top(10);
        example.set_margin_start(12);
        example.set_can_target(false);
        let paper = gtk::Overlay::new();
        paper.set_child(Some(&crate::pages::scrolled(&text)));
        paper.add_overlay(&example);
        paper.add_css_class("card");
        paper.set_overflow(gtk::Overflow::Hidden);
        paper.set_hexpand(true);
        let left = gtk::Box::new(gtk::Orientation::Vertical, 6);
        left.append(&caption("EqualizerAPO, AutoEQ or PEQdB Text"));
        left.append(&paper);

        let preview = EqGraph::new(Kind::Preview);
        preview.set_margin_top(10);
        preview.set_margin_bottom(6);
        preview.set_margin_start(8);
        preview.set_margin_end(10);
        let frame = adw::Bin::new();
        frame.add_css_class("card");
        frame.set_child(Some(&preview));
        let result = gtk::Label::new(None);
        result.set_wrap_mode(pango::WrapMode::WordChar);
        let limits = gtk::Label::new(Some(
            "The preamp sets the gain stage. Up to 24 peaking, 4 low-shelf and 4 high-shelf filters are used.",
        ));
        limits.add_css_class("caption");
        limits.add_css_class("dim-label");
        for label in [&result, &limits] {
            label.set_xalign(0.0);
            label.set_wrap(true);
            // the column is as wide as the preview, whatever these say
            label.set_max_width_chars(28);
        }
        let right = gtk::Box::new(gtk::Orientation::Vertical, 6);
        right.append(&caption("Preview"));
        right.append(&frame);
        right.append(&result);
        right.append(&limits);

        let columns = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        columns.append(&left);
        columns.append(&right);
        columns.set_vexpand(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
        content.append(&fields);
        content.append(&columns);
        content.set_margin_top(8);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);

        let add = gtk::Button::with_label("Add");
        add.add_css_class("suggested-action");
        let header = adw::HeaderBar::new();
        header.pack_end(&add);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&content));
        let page = adw::NavigationPage::builder().title("Add Preset").child(&view).build();

        let this = Rc::new(AddPage {
            name,
            text,
            problem,
            example,
            preview,
            result,
            add,
            readable: Cell::new(false),
            serial: Cell::new(0),
            pause: RefCell::default(),
        });
        this.show(Pasted::Nothing);

        let weak = Rc::downgrade(&this);
        this.text.buffer().connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.text_changed();
            }
        });
        let weak = Rc::downgrade(&this);
        this.name.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.offer();
            }
        });
        let submit = {
            let (weak, panel) = (Rc::downgrade(&this), Rc::downgrade(panel));
            move || {
                if let (Some(this), Some(panel)) = (weak.upgrade(), panel.upgrade()) {
                    this.submit(&panel);
                }
            }
        };
        let on_click = submit.clone();
        this.add.connect_clicked(move |_| on_click());
        // Enter in the name adds, or goes on to the text while that is not a preset
        let (weak, on_enter) = (Rc::downgrade(&this), submit.clone());
        this.name.connect_entry_activated(move |_| match weak.upgrade() {
            Some(this) if !this.add.is_sensitive() => {
                this.text.grab_focus();
            }
            _ => on_enter(),
        });
        // Control and Enter in the text add (seen before the text view makes a new line of it)
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
            if enter && modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
                submit();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        this.text.add_controller(keys);
        // the page keeps its state alive; one that is left does not read its text any more
        let kept = this.clone();
        page.connect_hidden(move |_| {
            if let Some(pause) = kept.pause.take() {
                pause.remove();
            }
        });

        panel.pages.push(&page);
        match start {
            Some((title, filters)) => {
                this.name.set_text(title);
                this.text.buffer().set_text(filters);
            }
            None => {
                this.text.grab_focus();
            }
        }
    }

    fn written(&self) -> String {
        let buffer = self.text.buffer();
        let (start, end) = buffer.bounds();
        buffer.text(&start, &end, false).to_string()
    }

    /// "Add" is offered for text that is a preset, under a name.
    fn offer(&self) {
        self.add.set_sensitive(self.readable.get() && !self.name.text().trim().is_empty());
    }

    /// Show what the text is.
    fn show(&self, pasted: Pasted) {
        self.readable.set(matches!(pasted, Pasted::Preset { .. }));
        self.example.set_visible(pasted == Pasted::Nothing);
        self.preview.set_dimmed(!self.readable.get());
        self.result.remove_css_class("error");
        self.result.remove_css_class("dim-label");
        match pasted {
            Pasted::Nothing => {
                self.preview.set_curve(&[]);
                self.result.set_text("Paste a preset to see its curve. OFF filters and # comments are skipped.");
                self.result.add_css_class("dim-label");
            }
            Pasted::Preset { curve, detail } => {
                self.preview.set_curve(&curve);
                self.result.set_text(&detail);
            }
            Pasted::Problem { message, line } => {
                self.preview.set_curve(&[]);
                self.result.set_text(&message);
                self.result.add_css_class("error");
                // the line it is about gets underlined
                let buffer = self.text.buffer();
                let line = line.and_then(|line| i32::try_from(line).ok()).map(|line| line - 1);
                if let (Some(problem), Some(line)) = (&self.problem, line)
                    && let Some(start) = buffer.iter_at_line(line)
                {
                    let mut end = start;
                    end.forward_to_line_end();
                    buffer.apply_tag(problem, &start, &end);
                }
            }
        }
        self.offer();
    }

    /// The text is read on a worker thread once the typing pauses; until then it is not known
    /// to be a preset.
    fn text_changed(self: &Rc<Self>) {
        let buffer = self.text.buffer();
        if let Some(problem) = &self.problem {
            let (start, end) = buffer.bounds();
            buffer.remove_tag(problem, &start, &end);
        }
        self.example.set_visible(buffer.char_count() == 0);
        self.readable.set(false);
        self.offer();
        self.serial.set(self.serial.get() + 1);
        if let Some(pause) = self.pause.take() {
            pause.remove();
        }
        let weak = Rc::downgrade(self);
        let pause = glib::timeout_add_local_once(TYPING_PAUSE, move || {
            let Some(this) = weak.upgrade() else { return };
            this.pause.replace(None);
            let (text, asked) = (this.written(), this.serial.get());
            background(
                move || pasted(&text),
                move |pasted| {
                    // unless the text has changed again
                    if this.serial.get() == asked {
                        this.show(pasted);
                    }
                },
            );
        });
        self.pause.replace(Some(pause));
    }

    fn submit(self: &Rc<Self>, panel: &Rc<Panel>) {
        if !self.add.is_sensitive() {
            return;
        }
        // not twice while the first is on its way
        self.add.set_sensitive(false);
        let (name, text, this) = (self.name.text().trim().to_string(), self.written(), self.clone());
        panel.change_then(
            "Add Preset",
            move |service| service.add(&name, &text).map(|preset| Some(format!("“{}” added", preset.name))),
            move |panel, added| {
                if added {
                    panel.pages.pop();
                } else {
                    this.offer();
                }
            },
        );
    }
}

// --- opening it --------------------------------------------------------------------------------

fn open(parent: &impl IsA<gtk::Widget>) -> Rc<Panel> {
    // --- the header: the active preset under the title, and the switch
    let title = adw::WindowTitle::new("Equalizer", "");
    let switch = gtk::Switch::new();
    switch.set_valign(gtk::Align::Center);
    switch.set_tooltip_text(Some("Equalizer On or Off"));
    switch.update_property(&[gtk::accessible::Property::Label("Equalizer")]);
    let spinner = adw::Spinner::new();
    spinner.set_visible(false);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_end(&switch);
    header.pack_end(&spinner);

    // --- the graph, and the note under it
    let graph = EqGraph::new(Kind::Full);
    // grey until the first view says otherwise
    graph.set_dimmed(true);
    graph.set_margin_top(12);
    graph.set_margin_bottom(8);
    graph.set_margin_start(10);
    graph.set_margin_end(12);
    let card = adw::Bin::new();
    card.add_css_class("card");
    card.set_child(Some(&graph));
    card.set_margin_start(18);
    card.set_margin_end(18);
    let note = gtk::Label::new(None);
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.add_css_class("caption");
    note.add_css_class("dim-label");
    note.set_margin_start(22);
    note.set_margin_end(22);
    note.set_visible(false);

    // --- the presets
    let heading = gtk::Label::new(Some("Presets"));
    heading.add_css_class("heading");
    heading.set_xalign(0.0);
    heading.set_hexpand(true);
    let import = gtk::Button::with_label("Import from the Old Clone");
    import.add_css_class("flat");
    import.set_visible(false);
    let add = gtk::Button::with_label("Add Preset…");
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.append(&heading);
    bar.append(&import);
    bar.append(&add);
    bar.set_margin_top(8);
    bar.set_margin_start(22);
    bar.set_margin_end(18);
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.add_css_class("eq-presets");
    list.set_selection_mode(gtk::SelectionMode::None);
    list.set_valign(gtk::Align::Start);
    // room for the list's shadow inside the scrolling area
    list.set_margin_top(2);
    list.set_margin_bottom(18);
    list.set_margin_start(18);
    list.set_margin_end(18);
    let scroller = crate::pages::scrolled(&list);
    // in a small window the graph gives way: two presets are always in view, and four before
    // the graph has its full height
    scroller.set_min_content_height(104);
    scroller.set_propagate_natural_height(true);
    scroller.set_max_content_height(212);
    scroller.set_visible(false);
    let empty = adw::StatusPage::new();
    empty.set_icon_name(Some("am-sound-symbolic"));
    empty.set_title("No Presets");
    empty.add_css_class("compact");
    empty.set_vexpand(true);
    empty.set_visible(false);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(6);
    content.append(&card);
    content.append(&note);
    content.append(&bar);
    content.append(&scroller);
    content.append(&empty);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&content));
    let pages = adw::NavigationView::new();
    pages.add(&adw::NavigationPage::builder().title("Equalizer").child(&view).build());
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&pages));
    let dialog = adw::Dialog::new();
    dialog.set_title("Equalizer");
    dialog.set_content_width(760);
    dialog.set_content_height(680);
    dialog.set_child(Some(&toasts));

    let panel = Rc::new(Panel {
        service: ctx().equalizer.clone(),
        toasts,
        pages,
        title,
        switch,
        spinner,
        graph,
        note,
        import,
        scroller,
        list,
        empty,
        rows: RefCell::default(),
        view: RefCell::default(),
        setting: Cell::new(false),
        jobs: RefCell::default(),
        running: Cell::new(false),
        open: Cell::new(true),
        timer: RefCell::default(),
    });

    let weak = Rc::downgrade(&panel);
    panel.switch.connect_active_notify(move |switch| {
        if let Some(panel) = weak.upgrade()
            && !panel.setting.get()
        {
            let on = switch.is_active();
            panel.change("Equalizer", move |service| service.set_enabled(on).map(|()| None));
        }
    });
    let weak = Rc::downgrade(&panel);
    panel.import.connect_clicked(move |_| {
        if let Some(panel) = weak.upgrade() {
            panel.import();
        }
    });
    let weak = Rc::downgrade(&panel);
    add.connect_clicked(move |_| {
        if let Some(panel) = weak.upgrade() {
            AddPage::open(&panel, None);
        }
    });

    // the note is about a sink that others set too: it is brought up to date every few seconds,
    // for as long as the dialog is open
    let weak = Rc::downgrade(&panel);
    let timer = glib::timeout_add_local(NOTE_INTERVAL, move || {
        let Some(panel) = weak.upgrade() else { return glib::ControlFlow::Break };
        if !panel.running.get() {
            panel.refresh(|_| {});
        }
        glib::ControlFlow::Continue
    });
    panel.timer.replace(Some(timer));
    // the dialog keeps its state alive, and takes the timer with it when it is closed
    let kept = panel.clone();
    dialog.connect_closed(move |_| {
        kept.open.set(false);
        if let Some(timer) = kept.timer.take() {
            timer.remove();
        }
    });
    dialog.present(Some(parent));
    panel.refresh(|_| {});
    panel
}

/// Open the Equalizer dialog over the window `parent` is in.
pub fn show(parent: &impl IsA<gtk::Widget>) {
    open(parent);
}

/// Text for the Add Preset page of a screenshot: made up, with a comment and a filter that is
/// switched off.
const PASTED: &str = "# a made-up preset
Preamp: -5.5 dB
Filter 1: ON LSC Fc 80 Hz Gain 5.0 dB Q 0.70
Filter 2: ON PK Fc 300 Hz Gain -2.0 dB Q 1.10
Filter 3: ON PK Fc 2000 Hz Gain 2.5 dB Q 1.40
Filter 4: OFF PK Fc 4000 Hz Gain -6.0 dB Q 3.00
Filter 5: ON PK Fc 6500 Hz Gain -3.5 dB Q 2.50
Filter 6: ON HSC Fc 11000 Hz Gain 4.5 dB Q 0.70";
/// The same with a line that is not a filter: its gain has no unit.
const PASTED_WRONG: &str = "# a made-up preset
Preamp: -5.5 dB
Filter 1: ON LSC Fc 80 Hz Gain 5.0 dB Q 0.70
Filter 2: ON PK Fc 300 Hz Gain -2.0 dB Q 1.10
Filter 3: ON PK Fc 2000 Hz Gain 2.5 Q 1.40
Filter 4: OFF PK Fc 4000 Hz Gain -6.0 dB Q 3.00
Filter 5: ON PK Fc 6500 Hz Gain -3.5 dB Q 2.50
Filter 6: ON HSC Fc 11000 Hz Gain 4.5 dB Q 0.70";

/// The dialog in one of its states, for screenshots (`--show NAME`): `equalizer` as it opens;
/// `equalizer-add` and `equalizer-add-error`, the page for a new preset with made-up text;
/// `equalizer-menu`, `equalizer-rename` and `equalizer-delete`, of the active preset;
/// `equalizer-failed`, what a change that fails looks like (it asks to rename a preset that is
/// not there). `equalizer-off` and `equalizer-empty` are states the demonstration's equalizer
/// starts in (`equalizer::DemoStart`).
pub fn show_scene(parent: &impl IsA<gtk::Widget>, name: &str) {
    let panel = open(parent);
    // once the presets are shown: something with the active one
    let with_active = |run: fn(&Rc<Panel>, &str)| {
        panel.refresh(move |panel| {
            let active = panel.view.borrow().as_ref().and_then(|view| view.active.clone());
            if let Some(active) = active {
                run(panel, &active);
            }
        });
    };
    match name {
        "equalizer-add" => AddPage::open(&panel, Some(("Sample: Vivid", PASTED))),
        "equalizer-add-error" => AddPage::open(&panel, Some(("Sample: Vivid", PASTED_WRONG))),
        "equalizer-menu" => with_active(|panel, active| {
            if let Some(row) = panel.rows.borrow().iter().find(|row| row.id == active) {
                row.menu.popup();
            }
        }),
        "equalizer-rename" => with_active(|panel, active| panel.rename(active)),
        "equalizer-delete" => with_active(|panel, active| panel.confirm_delete(active)),
        "equalizer-failed" => {
            panel.change("Rename", |service| service.rename("a-preset-that-is-not-there", "Sample").map(|()| None))
        }
        _ => {}
    }
}

/// "Equalizer…" for the sound settings of the player bar: closes that menu and opens the dialog.
pub fn menu_row() -> adw::ButtonRow {
    let row = adw::ButtonRow::new();
    row.set_title("Equalizer…");
    row.connect_activated(|row| {
        if let Some(popover) = row.ancestor(gtk::Popover::static_type()).and_downcast::<gtk::Popover>() {
            popover.popdown();
        }
        let _ = row.activate_action("app.equalizer", None);
    });
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::equalizer::{DemoEqualizer, DemoStart};

    #[test]
    fn numbers_and_sources_read_as_in_the_earlier_application() {
        assert_eq!(detail(15, -10.4), "15 filters · Preamp -10.4 dB");
        assert_eq!(detail(1, 3.0), "1 filter · Preamp +3.0 dB");
        assert_eq!(detail(0, -0.0), "No filters · Preamp +0.0 dB");
        assert_eq!(source_label("pasted"), "Pasted");
        assert_eq!(source_label("bridge"), "From the old clone");
        assert_eq!(source_label("sample"), "Sample");
        assert_eq!(source_label("elsewhere"), "Elsewhere");
        assert_eq!(sentence("line 3: not a Preamp or Filter line: x"), "Line 3: not a Preamp or Filter line: x");
        assert_eq!(sentence(""), "");
    }

    #[test]
    fn a_view_is_what_the_service_has() {
        let view = View::of(&DemoEqualizer::new(DemoStart::On));
        assert!(view.enabled && view.can_import);
        let names: Vec<&str> = view.rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["Sample: Reference", "Sample: Warm", "Sample: Bright", "Sample: Detailed"]);
        assert_eq!(view.active_row().map(|row| row.name.as_str()), Some("Sample: Warm"));
        assert_eq!(view.rows[1].detail, "5 filters · Preamp -4.5 dB");
        assert_eq!(view.rows[3].detail, "15 filters · Preamp -10.4 dB");
        assert!(view.rows.iter().all(|row| row.source == "Sample" && row.spark.len() == SPARK_POINTS));
        assert_eq!(view.curve.len(), CURVE_POINTS);
        assert_eq!(view.description(), "Frequency response of Sample: Warm: between -7.7 and +0.1 dB");

        let view = View::of(&DemoEqualizer::new(DemoStart::Off));
        assert!(!view.enabled);
        assert_eq!(view.curve.len(), CURVE_POINTS, "the chosen preset is drawn while the equalizer is off");
        assert_eq!(view.description(), "Frequency response of Sample: Warm: between -7.7 and +0.1 dB, off");

        let view = View::of(&DemoEqualizer::new(DemoStart::Empty));
        assert!(view.rows.is_empty() && view.curve.is_empty() && view.active.is_none());
        assert_eq!(view.description(), "Frequency response: flat, no preset is active");
    }

    #[test]
    fn the_note_is_the_service_s_or_none() {
        let demo = DemoEqualizer::new(DemoStart::On);
        assert_eq!(View::of(&demo).note.as_deref(), Some("Sink: EQ on, 5 bands, -4.5 dB"));
        let view = View::new(&demo.snapshot(), false, None);
        assert_eq!((view.note, view.can_import), (None, false));
    }

    #[test]
    fn pasted_text_is_a_preset_or_a_problem_on_a_line() {
        assert_eq!(pasted(""), Pasted::Nothing);
        assert_eq!(pasted("  \n\n"), Pasted::Nothing);
        match pasted(PASTED) {
            Pasted::Preset { curve, detail } => {
                assert_eq!(detail, "5 filters · Preamp -5.5 dB");
                assert_eq!(curve.len(), CURVE_POINTS);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            pasted(PASTED_WRONG),
            Pasted::Problem {
                message: "Line 5: not a Preamp or Filter line: Filter 3: ON PK Fc 2000 Hz Gain 2.5 Q 1.40".to_string(),
                line: Some(5)
            }
        );
        // a problem of the whole text has no line
        let many: String = (1..=25).map(|n| format!("Filter {n}: ON PK Fc 1000 Hz Gain 1 dB Q 1\n")).collect();
        assert_eq!(pasted(&many), Pasted::Problem { message: "Too many PK filters (max 24)".to_string(), line: None });
    }
}
