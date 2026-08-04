// PROTOTYPE — throwaway code, not production. Delete freely.
//
// Question this answers (ticket 06 / docs/mission-control/issues/06-surface-content.md):
// does the resolved sidebar design — t3code sidebar-v2 copied onto a GTK4 layer-shell
// surface — actually feel right on niri? Exclusive zone on the left, static creation
// order (activity never reorders), brightness-not-position attention, settled/archived
// shelves, keyboard verbs.
//
// Run: `just demo-sidebar`. All state is in-memory mock data. Press `t` repeatedly to
// step a scripted simulation (thread finishes -> Done brightens in place, new thread
// lands on top, settled thread raises a hand and un-settles, ...).
//
// Keys: j/k move · 1-9 jump · Enter summon · s settle · p park · a archive (confirm)
//       m toggle read · g area/global scope · Tab settled shelf · z archived shelf
//       t simulate · q/Esc quit

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Box as GtkBox, Label, Orientation, ScrolledWindow};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

const SIDEBAR_WIDTH: i32 = 340;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Attention {
    Approval,
    Input,
    Working,
    Failed,
    Unread,
    Idle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Live,
    Settled,
    Archived,
}

#[derive(Clone)]
struct ProtoThread {
    id: u64,
    title: String,
    area: &'static str,
    repo: &'static str,
    branch: String,
    harness: &'static str,
    host: Option<&'static str>,
    created: u64,
    attention: Attention,
    lifecycle: Lifecycle,
    parked: bool,
    working_since: Option<Instant>,
    idle_mins: u32,
    settled_mins: Option<u32>,
}

#[derive(Clone, PartialEq, Eq)]
enum Scope {
    Area(&'static str),
    Global,
}

struct ProtoState {
    threads: Vec<ProtoThread>,
    scope: Scope,
    selected: Option<u64>,
    visible: Vec<u64>,
    settled_expanded: bool,
    archived_expanded: bool,
    confirm_archive: Option<u64>,
    sim_step: usize,
    next_id: u64,
    message: String,
}

fn harness_glyph(harness: &str) -> &'static str {
    match harness {
        "pi" => "π",
        "claude" => "✳",
        "codex" => "◆",
        _ => "·",
    }
}

fn rel_time(mins: u32) -> String {
    match mins {
        0 => "now".to_string(),
        m if m < 60 => format!("{m}m"),
        m if m < 1440 => format!("{}h", m / 60),
        m => format!("{}d", m / 1440),
    }
}

fn working_duration(since: Instant) -> String {
    let secs = since.elapsed().as_secs();
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

fn mock_threads() -> Vec<ProtoThread> {
    let mut id = 0u64;
    let mut t = |title: &str,
                 area: &'static str,
                 repo: &'static str,
                 branch: &str,
                 harness: &'static str,
                 host: Option<&'static str>,
                 attention: Attention,
                 lifecycle: Lifecycle,
                 idle_mins: u32,
                 settled_mins: Option<u32>,
                 working_offset_secs: Option<u64>| {
        id += 1;
        ProtoThread {
            id,
            title: title.to_string(),
            area,
            repo,
            branch: branch.to_string(),
            harness,
            host,
            created: id,
            attention,
            lifecycle,
            parked: false,
            working_since: working_offset_secs.map(|s| Instant::now() - Duration::from_secs(s)),
            idle_mins,
            settled_mins,
        }
    };

    vec![
        t(
            "menu importer cleanup",
            "kanel",
            "kanel-api",
            "chore/importer",
            "codex",
            None,
            Attention::Idle,
            Lifecycle::Archived,
            4320,
            Some(2880),
            None,
        ),
        t(
            "tenant onboarding emails",
            "kanel",
            "kanel-web",
            "feat/onboarding-mails",
            "pi",
            None,
            Attention::Idle,
            Lifecycle::Settled,
            2200,
            Some(1440),
            None,
        ),
        t(
            "fix flaky order tests",
            "kanel",
            "kanel-api",
            "fix/flaky-orders",
            "claude",
            Some("devbox"),
            Attention::Idle,
            Lifecycle::Settled,
            950,
            Some(600),
            None,
        ),
        t(
            "upgrade nixpkgs pin",
            "dotfiles",
            "dotfiles",
            "chore/nixpkgs-bump",
            "pi",
            None,
            Attention::Idle,
            Lifecycle::Live,
            310,
            None,
            None,
        ),
        t(
            "webhook retry backoff",
            "kanel",
            "kanel-api",
            "fix/webhook-retries",
            "pi",
            Some("devbox"),
            Attention::Failed,
            Lifecycle::Live,
            42,
            None,
            None,
        ),
        t(
            "sidebar layer-shell spike",
            "agent-switch",
            "agent-switch",
            "proto/sidebar",
            "claude",
            None,
            Attention::Unread,
            Lifecycle::Live,
            18,
            None,
            None,
        ),
        t(
            "tenant db migration",
            "kanel",
            "kanel-api",
            "feat/tenant-migration",
            "codex",
            None,
            Attention::Input,
            Lifecycle::Live,
            9,
            None,
            None,
        ),
        t(
            "invoice PDF layout",
            "kanel",
            "kanel-web",
            "feat/invoice-pdf",
            "claude",
            None,
            Attention::Working,
            Lifecycle::Live,
            0,
            None,
            Some(430),
        ),
        t(
            "registry schema draft",
            "agent-switch",
            "agent-switch",
            "feat/registry",
            "pi",
            None,
            Attention::Approval,
            Lifecycle::Live,
            2,
            None,
            None,
        ),
    ]
}

impl ProtoState {
    fn in_scope(&self, thread: &ProtoThread) -> bool {
        match self.scope {
            Scope::Global => true,
            Scope::Area(area) => thread.area == area,
        }
    }

    fn section(&self, lifecycle: Lifecycle) -> Vec<&ProtoThread> {
        let mut rows: Vec<&ProtoThread> = self
            .threads
            .iter()
            .filter(|t| t.lifecycle == lifecycle && self.in_scope(t))
            .collect();
        match lifecycle {
            // Static creation order, newest first. Activity never reorders.
            Lifecycle::Live => rows.sort_by(|a, b| b.created.cmp(&a.created)),
            // Shelves: most recently settled/archived first.
            _ => rows.sort_by_key(|t| t.settled_mins.unwrap_or(0)),
        }
        rows
    }

    fn selected_thread(&self) -> Option<&ProtoThread> {
        let id = self.selected?;
        self.threads.iter().find(|t| t.id == id)
    }

    fn selected_thread_mut(&mut self) -> Option<&mut ProtoThread> {
        let id = self.selected?;
        self.threads.iter_mut().find(|t| t.id == id)
    }

    fn move_selection(&mut self, delta: i64) {
        if self.visible.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|id| self.visible.iter().position(|&v| v == id))
            .unwrap_or(0) as i64;
        let next = (current + delta).rem_euclid(self.visible.len() as i64) as usize;
        self.selected = Some(self.visible[next]);
    }

    fn ensure_selection(&mut self) {
        let valid = self
            .selected
            .map(|id| self.visible.contains(&id))
            .unwrap_or(false);
        if !valid {
            self.selected = self.visible.first().copied();
        }
    }

    fn aggregate(&self) -> (usize, usize) {
        let live = self
            .threads
            .iter()
            .filter(|t| t.lifecycle != Lifecycle::Archived);
        let mut waiting = 0;
        let mut running = 0;
        for t in live {
            match t.attention {
                Attention::Approval | Attention::Input | Attention::Failed | Attention::Unread => {
                    waiting += 1
                }
                Attention::Working => running += 1,
                Attention::Idle => {}
            }
        }
        (waiting, running)
    }

    fn simulate(&mut self) {
        let step = self.sim_step % 6;
        self.sim_step += 1;
        let msg = match step {
            0 => self
                .threads
                .iter_mut()
                .find(|t| t.attention == Attention::Working && t.lifecycle == Lifecycle::Live)
                .map(|t| {
                    t.attention = Attention::Unread;
                    t.working_since = None;
                    t.idle_mins = 0;
                    format!(
                        "sim: '{}' finished → Done (brightens, does not move)",
                        t.title
                    )
                }),
            1 => self
                .threads
                .iter_mut()
                .find(|t| t.attention == Attention::Idle && t.lifecycle == Lifecycle::Live)
                .map(|t| {
                    t.attention = Attention::Working;
                    t.working_since = Some(Instant::now());
                    format!("sim: '{}' started working (dims in place)", t.title)
                }),
            2 => self
                .threads
                .iter_mut()
                .find(|t| t.attention == Attention::Working && t.lifecycle == Lifecycle::Live)
                .map(|t| {
                    t.attention = Attention::Approval;
                    t.working_since = None;
                    format!("sim: '{}' raised a hand → Approval", t.title)
                }),
            3 => {
                self.next_id += 1;
                let id = self.next_id;
                self.threads.push(ProtoThread {
                    id,
                    title: format!("new thread #{id}"),
                    area: "kanel",
                    repo: "kanel-api",
                    branch: format!("feat/new-{id}"),
                    harness: "pi",
                    host: None,
                    created: id,
                    attention: Attention::Working,
                    lifecycle: Lifecycle::Live,
                    parked: false,
                    working_since: Some(Instant::now()),
                    idle_mins: 0,
                    settled_mins: None,
                });
                Some("sim: new thread created (lands on top — the only kind of move)".into())
            }
            4 => self
                .threads
                .iter_mut()
                .find(|t| t.attention == Attention::Approval && t.lifecycle == Lifecycle::Live)
                .map(|t| {
                    t.attention = Attention::Working;
                    t.working_since = Some(Instant::now());
                    format!("sim: '{}' approved → Working again", t.title)
                }),
            _ => self
                .threads
                .iter_mut()
                .find(|t| t.lifecycle == Lifecycle::Settled)
                .map(|t| {
                    t.lifecycle = Lifecycle::Live;
                    t.attention = Attention::Approval;
                    t.settled_mins = None;
                    format!("sim: settled '{}' raised a hand → un-settled", t.title)
                }),
        };
        self.message = msg.unwrap_or_else(|| "sim: no candidate for this step".into());
    }
}

fn status_label(thread: &ProtoThread) -> (String, &'static str) {
    match thread.attention {
        Attention::Approval => ("Approval".into(), "proto-approval"),
        Attention::Input => ("Input".into(), "proto-input"),
        Attention::Working => (
            format!(
                "Working {}",
                thread
                    .working_since
                    .map(working_duration)
                    .unwrap_or_default()
            ),
            "proto-working",
        ),
        Attention::Failed => ("⚠ Failed".into(), "proto-failed"),
        Attention::Unread => ("✓ Done".into(), "proto-done"),
        Attention::Idle => (rel_time(thread.idle_mins), "proto-time"),
    }
}

fn build_card(thread: &ProtoThread, index: Option<usize>, selected: bool, global: bool) -> GtkBox {
    let card = GtkBox::new(Orientation::Vertical, 2);
    card.add_css_class("proto-card");
    if selected {
        card.add_css_class("proto-selected");
    }
    let recede = matches!(thread.attention, Attention::Working | Attention::Idle);
    if recede && !selected {
        card.add_css_class("proto-recede");
    }

    // Line 1: [jump index] repo (area·repo in global view)  ···  status/time
    let line1 = GtkBox::new(Orientation::Horizontal, 6);
    if let Some(n) = index {
        let hint = Label::new(Some(&n.to_string()));
        hint.add_css_class("proto-jump");
        line1.append(&hint);
    }
    let repo_text = if global {
        format!("{} · {}", thread.area, thread.repo)
    } else {
        thread.repo.to_string()
    };
    let repo = Label::new(Some(&repo_text));
    repo.add_css_class("proto-repo");
    repo.set_halign(gtk4::Align::Start);
    repo.set_hexpand(true);
    repo.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    line1.append(&repo);
    if thread.parked {
        let parked = Label::new(Some("◌"));
        parked.add_css_class("proto-time");
        line1.append(&parked);
    }
    let (status_text, status_class) = status_label(thread);
    let status = Label::new(Some(&status_text));
    status.add_css_class(status_class);
    line1.append(&status);
    card.append(&line1);

    // Line 2: title
    let title = Label::new(Some(&thread.title));
    title.add_css_class("proto-title");
    if thread.attention == Attention::Unread {
        title.add_css_class("proto-title-unread");
    }
    title.set_halign(gtk4::Align::Start);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    card.append(&title);

    // Line 3: branch + host + harness (PR badge / diff stats: reserved seams)
    let line3 = GtkBox::new(Orientation::Horizontal, 6);
    let branch = Label::new(Some(&thread.branch));
    branch.add_css_class("proto-branch");
    branch.set_halign(gtk4::Align::Start);
    branch.set_hexpand(true);
    branch.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    line3.append(&branch);
    if let Some(host) = thread.host {
        let host_label = Label::new(Some(&format!("☁ {host}")));
        host_label.add_css_class("proto-host");
        line3.append(&host_label);
    }
    let harness = Label::new(Some(harness_glyph(thread.harness)));
    harness.add_css_class("proto-harness");
    line3.append(&harness);
    card.append(&line3);

    card
}

fn build_slim_row(thread: &ProtoThread, index: Option<usize>, selected: bool) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 6);
    row.add_css_class("proto-slim");
    if selected {
        row.add_css_class("proto-selected");
    }
    if thread.lifecycle == Lifecycle::Archived {
        row.add_css_class("proto-ghost");
    }
    if let Some(n) = index {
        let hint = Label::new(Some(&n.to_string()));
        hint.add_css_class("proto-jump");
        row.append(&hint);
    }
    let title = Label::new(Some(&thread.title));
    title.add_css_class("proto-slim-title");
    title.set_halign(gtk4::Align::Start);
    title.set_hexpand(true);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    row.append(&title);
    let time = Label::new(Some(&rel_time(thread.settled_mins.unwrap_or(0))));
    time.add_css_class("proto-time");
    row.append(&time);
    row
}

fn build_shelf_header(label: &str, count: usize, expanded: bool) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 6);
    row.add_css_class("proto-shelf");
    let text = if expanded {
        format!("▾ {label}")
    } else {
        format!("▸ {label} ({count})")
    };
    let title = Label::new(Some(&text));
    title.add_css_class("proto-shelf-title");
    title.set_halign(gtk4::Align::Start);
    row.append(&title);
    let rule = gtk4::Separator::new(Orientation::Horizontal);
    rule.set_hexpand(true);
    rule.set_valign(gtk4::Align::Center);
    rule.add_css_class("proto-shelf-rule");
    row.append(&rule);
    row
}

fn rebuild(list_box: &GtkBox, header_box: &GtkBox, footer: &Label, state: &mut ProtoState) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    while let Some(child) = header_box.first_child() {
        header_box.remove(&child);
    }

    // Header: scope name + waybar-style aggregate preview
    let scope_label = Label::new(Some(match state.scope {
        Scope::Area(area) => area,
        Scope::Global => "All areas",
    }));
    scope_label.add_css_class("proto-scope");
    scope_label.set_halign(gtk4::Align::Start);
    scope_label.set_hexpand(true);
    header_box.append(&scope_label);
    let (waiting, running) = state.aggregate();
    let agg = Label::new(Some(&format!("{waiting} waiting · {running} running")));
    agg.add_css_class(if waiting > 0 {
        "proto-agg-waiting"
    } else {
        "proto-agg-quiet"
    });
    header_box.append(&agg);

    // Assemble visible rows: active cards, then shelves.
    let active: Vec<ProtoThread> = state
        .section(Lifecycle::Live)
        .into_iter()
        .cloned()
        .collect();
    let settled: Vec<ProtoThread> = state
        .section(Lifecycle::Settled)
        .into_iter()
        .cloned()
        .collect();
    let archived: Vec<ProtoThread> = state
        .section(Lifecycle::Archived)
        .into_iter()
        .cloned()
        .collect();

    let mut visible: Vec<u64> = active.iter().map(|t| t.id).collect();
    if state.settled_expanded {
        visible.extend(settled.iter().map(|t| t.id));
    }
    if state.archived_expanded {
        visible.extend(archived.iter().map(|t| t.id));
    }
    state.visible = visible;
    state.ensure_selection();

    let jump_index = |id: u64, state: &ProtoState| -> Option<usize> {
        state
            .visible
            .iter()
            .position(|&v| v == id)
            .filter(|&pos| pos < 9)
            .map(|pos| pos + 1)
    };

    let global = state.scope == Scope::Global;
    for thread in &active {
        list_box.append(&build_card(
            thread,
            jump_index(thread.id, state),
            state.selected == Some(thread.id),
            global,
        ));
    }

    if !settled.is_empty() {
        list_box.append(&build_shelf_header(
            "Settled",
            settled.len(),
            state.settled_expanded,
        ));
        if state.settled_expanded {
            for thread in &settled {
                list_box.append(&build_slim_row(
                    thread,
                    jump_index(thread.id, state),
                    state.selected == Some(thread.id),
                ));
            }
        }
    }

    if !archived.is_empty() {
        list_box.append(&build_shelf_header(
            "Archived",
            archived.len(),
            state.archived_expanded,
        ));
        if state.archived_expanded {
            for thread in &archived {
                list_box.append(&build_slim_row(
                    thread,
                    jump_index(thread.id, state),
                    state.selected == Some(thread.id),
                ));
            }
        }
    }

    footer.set_text(&state.message);
}

const PROTO_CSS: &str = "
window { background-color: transparent; }
.proto-outer {
    background-color: rgba(24, 25, 30, 0.97);
    border-right: 1px solid rgba(255, 255, 255, 0.12);
}
.proto-header { padding: 12px 14px 8px 14px; }
label.proto-scope { color: #d4d4d4; font-size: 13px; font-weight: bold; font-family: monospace; }
label.proto-agg-waiting { color: #ff9e3b; font-size: 11px; font-family: monospace; }
label.proto-agg-quiet { color: #6a6f7a; font-size: 11px; font-family: monospace; }

.proto-card { padding: 8px 14px; border-radius: 6px; margin: 1px 6px; }
.proto-card.proto-selected, .proto-slim.proto-selected { background-color: rgba(255, 255, 255, 0.09); }
.proto-card.proto-recede { opacity: 0.62; }

label { color: #c8ccd4; }
label.proto-repo { color: #8a8f9a; font-size: 11px; font-family: monospace; }
label.proto-title { color: #d8dce4; font-size: 14px; }
label.proto-title-unread { color: #ffffff; font-weight: bold; }
label.proto-branch { color: #6a6f7a; font-size: 11px; font-family: monospace; }
label.proto-host { color: #7aa2f7; font-size: 11px; }
label.proto-harness { color: #8a8f9a; font-size: 12px; }
label.proto-jump { color: #565b66; font-size: 10px; font-family: monospace; }

/* Colorblind-safe status hues: orange=act-now, blue family=info/motion,
   magenta=failed, brightness+check=done. No red/green pairs. */
label.proto-approval { color: #ff9e3b; font-size: 11px; font-weight: bold; }
label.proto-input { color: #7aa2f7; font-size: 11px; font-weight: bold; }
label.proto-working { color: #7dcfff; font-size: 11px; font-family: monospace; }
label.proto-failed { color: #d27ce0; font-size: 11px; font-weight: bold; }
label.proto-done { color: #ffffff; font-size: 11px; font-weight: bold; }
label.proto-time { color: #6a6f7a; font-size: 11px; font-family: monospace; }

.proto-shelf { padding: 10px 14px 4px 14px; }
label.proto-shelf-title { color: #6a6f7a; font-size: 11px; font-family: monospace; }
separator.proto-shelf-rule { background-color: rgba(255, 255, 255, 0.08); min-height: 1px; }
.proto-slim { padding: 6px 14px; margin: 0px 6px; border-radius: 6px; }
label.proto-slim-title { color: #8a8f9a; font-size: 12px; }
.proto-ghost label.proto-slim-title { color: #565b66; font-style: italic; }

label.proto-footer { color: #8a8f9a; font-size: 10px; font-family: monospace; padding: 4px 14px; }
label.proto-help { color: #565b66; font-size: 9px; font-family: monospace; padding: 0px 14px 10px 14px; }
";

fn build_proto_ui(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(SIDEBAR_WIDTH)
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Bottom, true);
    // Explicit zone, not auto_exclusive_zone_enable(): auto follows the measured
    // surface width, which races with content sizing — the drawn surface and the
    // reserved zone could disagree (sidebar overlapping windows, or stale gap).
    window.set_exclusive_zone(SIDEBAR_WIDTH);
    window.set_size_request(SIDEBAR_WIDTH, -1);

    let provider = gtk4::CssProvider::new();
    provider.load_from_data(PROTO_CSS);
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().unwrap(),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let threads = mock_threads();
    let next_id = threads.iter().map(|t| t.id).max().unwrap_or(0);
    let state = Rc::new(RefCell::new(ProtoState {
        threads,
        scope: Scope::Area("kanel"),
        selected: None,
        visible: Vec::new(),
        settled_expanded: true,
        archived_expanded: false,
        confirm_archive: None,
        sim_step: 0,
        next_id,
        message: "prototype — press t to simulate events".into(),
    }));

    let outer = GtkBox::new(Orientation::Vertical, 0);
    outer.add_css_class("proto-outer");

    let header_box = GtkBox::new(Orientation::Horizontal, 8);
    header_box.add_css_class("proto-header");
    outer.append(&header_box);

    let scroller = ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroller.set_vexpand(true);
    let list_box = GtkBox::new(Orientation::Vertical, 0);
    scroller.set_child(Some(&list_box));
    outer.append(&scroller);

    // Footer/help must never widen the surface past SIDEBAR_WIDTH: wrap + a small
    // natural width keeps the window at its requested size.
    let footer = Label::new(None);
    footer.add_css_class("proto-footer");
    footer.set_halign(gtk4::Align::Start);
    footer.set_wrap(true);
    footer.set_max_width_chars(1);
    footer.set_hexpand(true);
    footer.set_xalign(0.0);
    outer.append(&footer);
    let help = Label::new(Some(
        "j/k move · 1-9 jump · ⏎ summon · s settle · p park · a archive · m read · g scope · Tab/z shelves · t simulate · q quit",
    ));
    help.add_css_class("proto-help");
    help.set_halign(gtk4::Align::Start);
    help.set_wrap(true);
    help.set_max_width_chars(1);
    help.set_hexpand(true);
    help.set_xalign(0.0);
    outer.append(&help);

    window.set_child(Some(&outer));

    {
        let mut s = state.borrow_mut();
        rebuild(&list_box, &header_box, &footer, &mut s);
    }

    let key_controller = gtk4::EventControllerKey::new();
    let state_for_keys = state.clone();
    let list_for_keys = list_box.clone();
    let header_for_keys = header_box.clone();
    let footer_for_keys = footer.clone();
    let window_for_keys = window.clone();
    key_controller.connect_key_pressed(move |_, keyval, _, _| {
        let ch = keyval.to_unicode();
        let name = keyval.name().map(|s| s.to_lowercase());
        let mut s = state_for_keys.borrow_mut();
        let mut dirty = true;

        // Anything except a second `a` disarms the archive confirmation.
        let confirm = s.confirm_archive.take();

        match (ch, name.as_deref()) {
            (Some('q'), _) | (_, Some("escape")) => {
                window_for_keys.close();
                return gtk4::glib::Propagation::Stop;
            }
            (Some('j'), _) | (_, Some("down")) => s.move_selection(1),
            (Some('k'), _) | (_, Some("up")) => s.move_selection(-1),
            (Some(c), _) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as usize) - ('1' as usize);
                if let Some(&id) = s.visible.get(idx) {
                    s.selected = Some(id);
                }
            }
            (_, Some("return")) => {
                if let Some(t) = s.selected_thread_mut() {
                    if t.attention == Attention::Unread {
                        t.attention = Attention::Idle;
                    }
                    let was_settled = t.lifecycle == Lifecycle::Settled;
                    if was_settled {
                        t.lifecycle = Lifecycle::Live;
                        t.settled_mins = None;
                    }
                    t.parked = false;
                    s.message = format!(
                        "summon: would focus/resurrect '{}'{} — real sidebar dismisses here",
                        s.selected_thread().unwrap().title,
                        if was_settled { " (un-settled)" } else { "" },
                    );
                }
            }
            (Some('s'), _) => {
                if let Some(t) = s.selected_thread_mut() {
                    match t.lifecycle {
                        Lifecycle::Live => {
                            t.lifecycle = Lifecycle::Settled;
                            t.settled_mins = Some(0);
                            t.attention = Attention::Idle;
                            s.message = "settled — row moved to the shelf".into();
                        }
                        Lifecycle::Settled => {
                            t.lifecycle = Lifecycle::Live;
                            t.settled_mins = None;
                            s.message = "un-settled — back in the active list".into();
                        }
                        Lifecycle::Archived => {
                            s.message = "archived threads: a to unarchive".into()
                        }
                    }
                }
            }
            (Some('p'), _) => {
                if let Some(t) = s.selected_thread_mut() {
                    t.parked = !t.parked;
                    s.message = format!(
                        "park: windows {} (spatial only — row unchanged)",
                        if t.parked { "hidden" } else { "shown" }
                    );
                }
            }
            (Some('a'), _) => {
                let selected = s.selected;
                if let Some(t) = s.selected_thread_mut() {
                    match t.lifecycle {
                        Lifecycle::Archived => {
                            t.lifecycle = Lifecycle::Live;
                            t.settled_mins = None;
                            s.message = "unarchived — restored to live".into();
                        }
                        _ if confirm == selected => {
                            t.lifecycle = Lifecycle::Archived;
                            t.attention = Attention::Idle;
                            t.settled_mins = Some(0);
                            s.message = "archived — tombstone in the Archived shelf (z)".into();
                        }
                        _ => {
                            s.confirm_archive = selected;
                            s.message =
                                "archive reclaims worktree + runtime — press a again to confirm"
                                    .into();
                        }
                    }
                }
            }
            (Some('m'), _) => {
                if let Some(t) = s.selected_thread_mut() {
                    match t.attention {
                        Attention::Idle => {
                            t.attention = Attention::Unread;
                            s.message = "marked unread → Done".into();
                        }
                        Attention::Unread => {
                            t.attention = Attention::Idle;
                            s.message = "marked read".into();
                        }
                        _ => s.message = "mark read/unread only applies to quiet threads".into(),
                    }
                }
            }
            (Some('g'), _) => {
                s.scope = match s.scope {
                    Scope::Area(_) => Scope::Global,
                    Scope::Global => Scope::Area("kanel"),
                };
                s.message = match s.scope {
                    Scope::Global => "scope: all areas (non-area workspace fallback)".into(),
                    Scope::Area(a) => format!("scope: area '{a}' (follows focused workspace)"),
                };
            }
            (_, Some("tab")) => {
                s.settled_expanded = !s.settled_expanded;
                s.message = format!(
                    "settled shelf {}",
                    if s.settled_expanded {
                        "expanded"
                    } else {
                        "collapsed"
                    }
                );
            }
            (Some('z'), _) => {
                s.archived_expanded = !s.archived_expanded;
                s.message = format!(
                    "archived shelf {}",
                    if s.archived_expanded {
                        "expanded"
                    } else {
                        "collapsed"
                    }
                );
            }
            (Some('t'), _) => s.simulate(),
            _ => dirty = false,
        }

        if dirty {
            rebuild(&list_for_keys, &header_for_keys, &footer_for_keys, &mut s);
        }
        gtk4::glib::Propagation::Stop
    });
    window.add_controller(key_controller);

    // Tick the Working durations once a second.
    let state_for_timer = state.clone();
    let list_for_timer = list_box.clone();
    let header_for_timer = header_box.clone();
    let footer_for_timer = footer.clone();
    gtk4::glib::timeout_add_local(Duration::from_secs(1), move || {
        let mut s = state_for_timer.borrow_mut();
        let any_working = s
            .threads
            .iter()
            .any(|t| t.attention == Attention::Working && t.lifecycle == Lifecycle::Live);
        if any_working {
            rebuild(
                &list_for_timer,
                &header_for_timer,
                &footer_for_timer,
                &mut s,
            );
        }
        gtk4::glib::ControlFlow::Continue
    });

    window.present();
}

pub fn run() -> gtk4::glib::ExitCode {
    // Single-instance: a second `demo-sidebar` forwards activation here and exits,
    // and the guard below refuses a second window (two stacked sidebars = two
    // exclusive zones = phantom gap).
    let app = Application::builder()
        .application_id("com.thrawny.agent-switch.sidebar-proto")
        .build();
    app.connect_activate(|app| {
        if app.windows().is_empty() {
            build_proto_ui(app);
        }
    });
    app.run_with_args::<&str>(&[])
}
