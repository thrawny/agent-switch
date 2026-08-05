// PROTOTYPE — throwaway code, not production. Delete freely.
//
// Question this answers (ticket 06 / docs/mission-control/issues/06-surface-content.md):
// does the resolved sidebar design — t3code sidebar-v2 copied onto a GTK4 layer-shell
// surface — actually feel right on niri? Persistent dock on the left, static creation
// order (activity never reorders), brightness-not-position attention, settled/archived
// shelves, keyboard verbs.
//
// Run: `just demo-sidebar`. All state is in-memory mock data.
//
// Live mode (`just demo-sidebar-live`, ticket 08's window-hosted leg): rows are real
// agent sessions joined with niri windows via sidebar_proto_live::LiveWorld; Enter/p
// act on the real desktop (focus, nirius scratchpad, cold resurrection via harness
// resume), n spawns a new pi thread. By default the live sidebar stays visible and
// reserves its width; re-running the command toggles keyboard command mode. Pass
// --popup to restore the original transient overlay for quick A/B testing.
//
// Keys: j/k move · Shift+J/K reorder · 1-9 jump · Enter summon · s settle · p park
//       a archive (confirm) · r rename · m toggle read · g area/global scope
//       Tab settled shelf · z archived shelf · n new thread (live) · q/Esc release

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Box as GtkBox, Label, Orientation, ScrolledWindow};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::sidebar_proto_live::{LiveThread, LiveWorld, focused_area};
use crate::state::{SessionState, WaitingReason};

// Widened for the diagnostics pass (2026-08-05): room for the debug line and
// bigger type while more instrumentation gets added.
const SIDEBAR_WIDTH: i32 = 480;

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
    area: String,
    repo: String,
    branch: String,
    harness: String,
    host: Option<&'static str>,
    created: u64,
    attention: Attention,
    lifecycle: Lifecycle,
    parked: bool,
    cold: bool,
    working_since: Option<Instant>,
    idle_mins: u32,
    settled_mins: Option<u32>,
    /// Diagnostics line (live mode): seq, window id, harness session id.
    debug: String,
}

#[derive(Clone, PartialEq, Eq)]
enum Scope {
    Area(String),
    Global,
}

struct ProtoState {
    threads: Vec<ProtoThread>,
    live: Option<LiveWorld>,
    /// Restore the original transient overlay behavior for A/B feel-testing.
    popup: bool,
    scope: Scope,
    selected: Option<u64>,
    visible: Vec<u64>,
    settled_expanded: bool,
    archived_expanded: bool,
    confirm_archive: Option<u64>,
    /// Some(buffer) while typing a new title for the selected row (`r`).
    rename_buffer: Option<String>,
    message: String,
}

fn harness_glyph(harness: &str) -> &'static str {
    match harness {
        "pi" => "π",
        "claude" => "✳",
        // Text fallback when the embedded OpenAI texture cannot be decoded.
        "codex" => "✺",
        _ => "·",
    }
}

fn build_harness_icon(harness: &str) -> gtk4::Widget {
    if harness == "codex" {
        // OpenAI mark sourced from quotabar's LobeHub Icons asset (MIT),
        // recolored to the sidebar's Monokai purple.
        let bytes = gtk4::glib::Bytes::from_static(include_bytes!("../assets/openai.png"));
        if let Ok(texture) = gtk4::gdk::Texture::from_bytes(&bytes) {
            let image = gtk4::Image::from_paintable(Some(&texture));
            image.set_pixel_size(14);
            image.set_tooltip_text(Some("Codex"));
            image.add_css_class("proto-harness-icon");
            return image.upcast();
        }
    }

    let label = Label::new(Some(harness_glyph(harness)));
    label.add_css_class("proto-harness");
    label.set_tooltip_text(Some(harness));
    label.upcast()
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
                 area: &str,
                 repo: &str,
                 branch: &str,
                 harness: &str,
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
            area: area.to_string(),
            repo: repo.to_string(),
            branch: branch.to_string(),
            harness: harness.to_string(),
            host,
            created: id,
            attention,
            lifecycle,
            parked: false,
            cold: false,
            working_since: working_offset_secs.map(|s| Instant::now() - Duration::from_secs(s)),
            idle_mins,
            settled_mins,
            debug: String::new(),
        }
    };

    vec![
        t(
            "menu importer cleanup",
            "work",
            "backend",
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
            "work",
            "web",
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
            "work",
            "backend",
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
            "work",
            "backend",
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
            "work",
            "backend",
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
            "work",
            "web",
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

/// Map a live registry thread to a display row. Attention is derived Codex-style
/// from hook-authored state + the read marker (03): a working thread can never
/// be unread.
fn proto_from_live(t: &LiveThread, now: f64) -> ProtoThread {
    let shelved = t.archived_at.is_some() || t.settled_at.is_some();
    let attention = if shelved {
        Attention::Idle
    } else {
        match t.state {
            SessionState::Responding => Attention::Working,
            SessionState::Waiting => match t.waiting_reason {
                // Approval stays sticky until actually answered (the daemon
                // clears it from the transcript); a question you've visited
                // stops shouting until new activity — visiting = read.
                Some(WaitingReason::PermissionPrompt) => Attention::Approval,
                None if t.state_updated > t.last_read_at => Attention::Input,
                None => Attention::Idle,
            },
            _ => {
                if t.state_updated > t.last_read_at {
                    Attention::Unread
                } else {
                    Attention::Idle
                }
            }
        }
    };
    let lifecycle = if t.archived_at.is_some() {
        Lifecycle::Archived
    } else if t.settled_at.is_some() {
        Lifecycle::Settled
    } else {
        Lifecycle::Live
    };
    let working_since = (attention == Attention::Working)
        .then(|| Instant::now() - Duration::from_secs((now - t.state_updated).max(0.0) as u64));
    ProtoThread {
        id: t.seq,
        title: t.title.clone(),
        area: t.area.clone(),
        repo: t.repo.clone(),
        branch: t.branch.clone(),
        harness: t.harness.clone(),
        host: None,
        created: t.order,
        attention,
        lifecycle,
        parked: t.parked,
        cold: t.cold(),
        working_since,
        idle_mins: ((now - t.state_updated).max(0.0) / 60.0) as u32,
        settled_mins: t.settled_at.map(|at| ((now - at).max(0.0) / 60.0) as u32),
        debug: format!(
            "#{} · w{} · {}",
            t.seq,
            t.window_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "—".into()),
            t.harness_session_id,
        ),
    }
}

fn regen_live(state: &mut ProtoState) {
    let now = crate::state::now();
    let threads: Vec<ProtoThread> = match &state.live {
        Some(world) => world
            .threads()
            .iter()
            .map(|t| proto_from_live(t, now))
            .collect(),
        None => return,
    };
    state.threads = threads;
}

impl ProtoState {
    fn in_scope(&self, thread: &ProtoThread) -> bool {
        match &self.scope {
            Scope::Global => true,
            Scope::Area(area) => &thread.area == area,
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
            Lifecycle::Live => rows.sort_by_key(|t| std::cmp::Reverse(t.created)),
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
    let repo_text = if global && thread.area != thread.repo {
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
    if thread.cold {
        // Runtime gone (window closed / compositor restart): summon resurrects.
        let cold = Label::new(Some("❆"));
        cold.add_css_class("proto-time");
        line1.append(&cold);
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
    line3.append(&build_harness_icon(&thread.harness));
    card.append(&line3);

    // Line 4 (live only): diagnostics — seq · window id · harness session id.
    if !thread.debug.is_empty() {
        let dbg = Label::new(Some(&thread.debug));
        dbg.add_css_class("proto-debug");
        dbg.set_halign(gtk4::Align::Start);
        dbg.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        card.append(&dbg);
    }

    card
}

fn build_slim_row(thread: &ProtoThread, index: Option<usize>, selected: bool) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 6);
    row.add_css_class("proto-slim");
    if !thread.debug.is_empty() {
        row.set_tooltip_text(Some(&thread.debug));
        let seq = Label::new(Some(&format!("#{}", thread.id)));
        seq.add_css_class("proto-debug");
        row.append(&seq);
    }
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
    let scope_label = Label::new(Some(match &state.scope {
        Scope::Area(area) => area.as_str(),
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
/* Monokai Pro Spectrum, kept deliberately low-chroma outside status. */
window { background-color: transparent; }
.proto-outer {
    background-color: rgba(31, 31, 31, 0.97);
    border-right: 1px solid rgba(105, 103, 108, 0.38);
}
window.proto-docked .proto-outer {
    border-right: 2px solid #3a3a3a;
}
window.proto-docked.proto-interactive .proto-outer {
    border-right-color: #f92672;
}
.proto-header { padding: 14px 16px 10px 16px; }
label.proto-scope { color: #fce566; font-size: 15px; font-weight: bold; font-family: monospace; }
label.proto-agg-waiting { color: #fc9867; font-size: 13px; font-family: monospace; }
label.proto-agg-quiet { color: #69676c; font-size: 13px; font-family: monospace; }

.proto-card { padding: 10px 16px; border-radius: 6px; margin: 2px 8px; }
.proto-card.proto-selected, .proto-slim.proto-selected { background-color: #343745; }
.proto-card.proto-recede { opacity: 0.62; }

label { color: #d9d5df; }
label.proto-repo { color: #8b888f; font-size: 13px; font-family: monospace; }
label.proto-title { color: #e5e0e9; font-size: 16px; }
label.proto-title-unread { color: #f7f1ff; font-weight: bold; }
label.proto-branch { color: #69676c; font-size: 13px; font-family: monospace; }
label.proto-host { color: #5ad4e6; font-size: 13px; }
label.proto-harness { color: #948ae3; font-size: 14px; }
label.proto-jump { color: #69676c; font-size: 12px; font-family: monospace; }
label.proto-debug { color: #69676c; font-size: 11px; font-family: monospace; }

/* Spectrum status hues remain structurally distinct: orange=act-now,
   purple=input, cyan=motion, pink=failed, brightness+check=done. */
label.proto-approval { color: #fc9867; font-size: 13px; font-weight: bold; }
label.proto-input { color: #948ae3; font-size: 13px; font-weight: bold; }
label.proto-working { color: #5ad4e6; font-size: 13px; font-family: monospace; }
label.proto-failed { color: #fc618d; font-size: 13px; font-weight: bold; }
label.proto-done { color: #f7f1ff; font-size: 13px; font-weight: bold; }
label.proto-time { color: #69676c; font-size: 13px; font-family: monospace; }

.proto-shelf { padding: 12px 16px 5px 16px; }
label.proto-shelf-title { color: #69676c; font-size: 13px; font-family: monospace; }
separator.proto-shelf-rule { background-color: rgba(105, 103, 108, 0.28); min-height: 1px; }
.proto-slim { padding: 7px 16px; margin: 0px 8px; border-radius: 6px; }
label.proto-slim-title { color: #8b888f; font-size: 14px; }
.proto-ghost label.proto-slim-title { color: #69676c; font-style: italic; }

label.proto-footer { color: #8b888f; font-size: 12px; font-family: monospace; padding: 5px 16px; }
label.proto-help { color: #69676c; font-size: 11px; font-family: monospace; padding: 0px 16px 12px 16px; }
";

fn set_interactive<W>(window: &W, interactive: bool)
where
    W: IsA<gtk4::Window> + IsA<gtk4::Widget>,
{
    if interactive {
        window.add_css_class("proto-interactive");
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        window.present();
    } else {
        window.remove_css_class("proto-interactive");
        window.set_keyboard_mode(KeyboardMode::None);
    }
}

/// Live summon: the sidebar's exclusive keyboard grab makes niri treat "no
/// window" as focused, so focus changes only stick once the grab is gone.
/// Release command mode, let that layer-shell commit, then run the verb. The
/// dock stays mapped and continues reserving its width.
fn schedule_summon(
    state: Rc<RefCell<ProtoState>>,
    window: ApplicationWindow,
    seq: u64,
    popup: bool,
) {
    // `popup` is passed by value because the key handler already holds the
    // state's RefCell borrow while scheduling this callback.
    if popup {
        window.set_visible(false);
    } else {
        set_interactive(&window, false);
    }
    gtk4::glib::timeout_add_local_once(Duration::from_millis(80), move || {
        if let Some(world) = state.borrow_mut().live.as_mut() {
            world.summon(seq);
        }
    });
}

/// Fallback park (used when the matcher-based in-place park misses): keeps
/// the sidebar open but releases the keyboard grab, focuses the target window
/// (nirius resolves "current window" through niri focus), toggles it into the
/// scratchpad, then returns to the workspace the user came from and re-grabs
/// — settling a thread in another area must not strand you there.
#[allow(clippy::too_many_arguments)]
fn schedule_park(
    state: Rc<RefCell<ProtoState>>,
    window: ApplicationWindow,
    list_box: GtkBox,
    header_box: GtkBox,
    footer: Label,
    seq: u64,
    target: u64,
    verb: &'static str,
) {
    let home_workspace = crate::niri::niri_workspaces()
        .into_iter()
        .find(|ws| ws.is_focused)
        .map(|ws| ws.id);
    set_interactive(&window, false);
    gtk4::glib::timeout_add_local_once(Duration::from_millis(60), move || {
        let focused = crate::niri::focus_window(target);
        gtk4::glib::timeout_add_local_once(Duration::from_millis(90), move || {
            let msg = if focused {
                state
                    .borrow_mut()
                    .live
                    .as_mut()
                    .map(|world| world.park_focused(seq, verb))
                    .unwrap_or_default()
            } else {
                format!("focus-window {target} failed — cannot park")
            };
            if let Some(ws) = home_workspace {
                crate::niri::niri_action(niri_ipc::Action::FocusWorkspace {
                    reference: niri_ipc::WorkspaceReferenceArg::Id(ws),
                });
            }
            set_interactive(&window, true);
            let mut s = state.borrow_mut();
            s.message = msg;
            regen_live(&mut s);
            rebuild(&list_box, &header_box, &footer, &mut s);
        });
    });
}

fn build_proto_ui(app: &Application, live: bool, popup: bool) {
    let docked = live && !popup;
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(SIDEBAR_WIDTH)
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_keyboard_mode(if docked {
        KeyboardMode::None
    } else {
        KeyboardMode::Exclusive
    });
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Bottom, true);
    // Live mode is a Waybar-style dock: it stays mapped and removes its width
    // from niri's usable area. Mock mode remains a transient overlay.
    window.set_exclusive_zone(if docked { SIDEBAR_WIDTH } else { 0 });
    window.set_size_request(SIDEBAR_WIDTH, -1);
    if docked {
        window.add_css_class("proto-docked");
    }

    let provider = gtk4::CssProvider::new();
    provider.load_from_data(PROTO_CSS);
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().unwrap(),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let (threads, world, scope, message) = if live {
        let world = LiveWorld::new();
        let now = crate::state::now();
        let threads: Vec<ProtoThread> = world
            .threads()
            .iter()
            .map(|t| proto_from_live(t, now))
            .collect();
        // Default to the global all-areas view for now (user call, 2026-08-04);
        // g narrows to the focused workspace's area.
        let scope = Scope::Global;
        let message = "live — ⏎ summon/go-to · p park · n new pi thread".to_string();
        (threads, Some(world), scope, message)
    } else {
        (
            mock_threads(),
            None,
            Scope::Area("work".to_string()),
            "prototype — static mock data".to_string(),
        )
    };
    let state = Rc::new(RefCell::new(ProtoState {
        threads,
        live: world,
        popup,
        scope,
        selected: None,
        visible: Vec::new(),
        settled_expanded: true,
        archived_expanded: false,
        confirm_archive: None,
        rename_buffer: None,
        message,
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
    let help_text = if docked {
        "j/k move · J/K reorder · 1-9 jump · ⏎ summon · s settle · p park · a archive · r rename · m read · n new · g scope · Tab/z shelves · q release"
    } else {
        "j/k move · J/K reorder · 1-9 jump · ⏎ summon · s settle · p park · a archive · r rename · m read · n new · g scope · Tab/z shelves · q close"
    };
    let help = Label::new(Some(help_text));
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

        // Rename mode captures every key until commit/cancel.
        if s.rename_buffer.is_some() {
            match (ch, name.as_deref()) {
                (_, Some("escape")) => {
                    s.rename_buffer = None;
                    s.message = "rename cancelled".into();
                }
                (_, Some("return")) => {
                    let title = s
                        .rename_buffer
                        .take()
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if title.is_empty() {
                        s.message = "rename cancelled (empty)".into();
                    } else if let Some(id) = s.selected {
                        if s.live.is_some() {
                            let msg = s.live.as_mut().unwrap().rename(id, title);
                            s.message = msg;
                        } else if let Some(t) = s.threads.iter_mut().find(|t| t.id == id) {
                            t.title = title;
                            s.message = "renamed".into();
                        }
                    }
                }
                (_, Some("backspace")) => {
                    s.rename_buffer.as_mut().unwrap().pop();
                }
                (Some(c), _) if !c.is_control() => {
                    s.rename_buffer.as_mut().unwrap().push(c);
                }
                _ => dirty = false,
            }
            if let Some(buf) = s.rename_buffer.as_deref() {
                s.message = format!("rename: {buf}▏  (⏎ save · Esc cancel)");
            }
            if dirty {
                regen_live(&mut s);
                rebuild(&list_for_keys, &header_for_keys, &footer_for_keys, &mut s);
            }
            return gtk4::glib::Propagation::Stop;
        }

        match (ch, name.as_deref()) {
            (Some('q'), _) | (_, Some("escape")) => {
                if s.live.is_some() {
                    if s.popup {
                        window_for_keys.set_visible(false);
                    } else {
                        set_interactive(&window_for_keys, false);
                    }
                } else {
                    window_for_keys.close();
                }
                return gtk4::glib::Propagation::Stop;
            }
            (Some('j'), _) | (_, Some("down")) => s.move_selection(1),
            (Some('k'), _) | (_, Some("up")) => s.move_selection(-1),
            // Shift+J/K: manual reorder — swap the selected active row with
            // its display neighbor (selection travels with the row). The one
            // reorder that isn't creation; activity still never moves rows.
            (Some(c @ ('J' | 'K')), _) => {
                let down = c == 'J';
                let ids: Vec<u64> = s.section(Lifecycle::Live).iter().map(|t| t.id).collect();
                let pos = s.selected.and_then(|id| ids.iter().position(|&v| v == id));
                match pos {
                    None => s.message = "reorder applies to active rows".into(),
                    Some(pos) => {
                        let neighbor = if down {
                            ids.get(pos + 1).copied()
                        } else {
                            pos.checked_sub(1).and_then(|p| ids.get(p).copied())
                        };
                        match (s.selected, neighbor) {
                            (Some(id), Some(nid)) => {
                                if s.live.is_some() {
                                    let msg = s.live.as_mut().unwrap().swap_order(id, nid);
                                    s.message = msg;
                                } else {
                                    let find = |threads: &[ProtoThread], want: u64| {
                                        threads.iter().find(|t| t.id == want).map(|t| t.created)
                                    };
                                    if let (Some(ca), Some(cb)) =
                                        (find(&s.threads, id), find(&s.threads, nid))
                                    {
                                        for t in s.threads.iter_mut() {
                                            if t.id == id {
                                                t.created = cb;
                                            } else if t.id == nid {
                                                t.created = ca;
                                            }
                                        }
                                        s.message = "reordered".into();
                                    }
                                }
                            }
                            _ => s.message = "already at the edge".into(),
                        }
                    }
                }
            }
            (Some(c), _) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as usize) - ('1' as usize);
                if let Some(&id) = s.visible.get(idx) {
                    s.selected = Some(id);
                }
            }
            (_, Some("return")) => {
                if s.live.is_some() {
                    if let Some(seq) = s.selected {
                        schedule_summon(
                            state_for_keys.clone(),
                            window_for_keys.clone(),
                            seq,
                            s.popup,
                        );
                        return gtk4::glib::Propagation::Stop;
                    }
                } else if let Some(t) = s.selected_thread_mut() {
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
                if s.live.is_some() {
                    if let Some(seq) = s.selected {
                        // Settle ⇒ park (03): hide the window when settling a
                        // thread that still shows one. Un-settle is bit-only —
                        // summon, not un-settle, is what brings windows back.
                        let park_target = {
                            let world = s.live.as_ref().unwrap();
                            world
                                .threads()
                                .iter()
                                .find(|t| t.seq == seq)
                                .filter(|t| {
                                    t.settled_at.is_none() && t.archived_at.is_none() && !t.parked
                                })
                                .and_then(|t| t.window_id)
                        };
                        let msg = s.live.as_mut().unwrap().toggle_settle(seq);
                        s.message = msg;
                        if let Some(target) = park_target {
                            // In-place first (no focus change); the dance
                            // only when the matcher misses.
                            let parked = s
                                .live
                                .as_mut()
                                .unwrap()
                                .park_in_place(seq, "settled + parked");
                            match parked {
                                Ok(msg) => s.message = msg,
                                Err(_) => schedule_park(
                                    state_for_keys.clone(),
                                    window_for_keys.clone(),
                                    list_for_keys.clone(),
                                    header_for_keys.clone(),
                                    footer_for_keys.clone(),
                                    seq,
                                    target,
                                    "settled + parked",
                                ),
                            }
                        }
                    }
                } else if let Some(t) = s.selected_thread_mut() {
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
                if s.live.is_some() {
                    if let Some(seq) = s.selected {
                        enum Plan {
                            Unpark,
                            Dance(u64),
                            Msg(String),
                        }
                        let plan = {
                            let world = s.live.as_ref().unwrap();
                            match world.threads().iter().find(|t| t.seq == seq) {
                                None => Plan::Msg("no such thread".into()),
                                Some(t) if t.parked => Plan::Unpark,
                                Some(t) => match t.window_id {
                                    Some(id) => Plan::Dance(id),
                                    None => Plan::Msg("no window — nothing to park (cold)".into()),
                                },
                            }
                        };
                        match plan {
                            Plan::Unpark => {
                                // Unpark = summon-here; scratchpad-show --id is
                                // exact and grab-safe, keep the sidebar open.
                                let msg = s.live.as_mut().unwrap().summon(seq);
                                s.message = msg;
                            }
                            Plan::Msg(msg) => s.message = msg,
                            Plan::Dance(target) => {
                                // In-place first (no focus change); the dance
                                // only when the matcher misses.
                                let parked = s.live.as_mut().unwrap().park_in_place(seq, "parked");
                                match parked {
                                    Ok(msg) => s.message = msg,
                                    Err(_) => {
                                        s.message = "parking (focus dance)…".into();
                                        schedule_park(
                                            state_for_keys.clone(),
                                            window_for_keys.clone(),
                                            list_for_keys.clone(),
                                            header_for_keys.clone(),
                                            footer_for_keys.clone(),
                                            seq,
                                            target,
                                            "parked",
                                        );
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(t) = s.selected_thread_mut() {
                    t.parked = !t.parked;
                    s.message = format!(
                        "park: windows {} (spatial only — row unchanged)",
                        if t.parked { "hidden" } else { "shown" }
                    );
                }
            }
            (Some('a'), _) => {
                let selected = s.selected;
                if s.live.is_some() {
                    if let Some(id) = selected {
                        let is_archived = s
                            .live
                            .as_ref()
                            .unwrap()
                            .threads()
                            .iter()
                            .find(|t| t.seq == id)
                            .is_some_and(|t| t.archived_at.is_some());
                        if is_archived || confirm == selected {
                            let msg = s.live.as_mut().unwrap().toggle_archive(id);
                            s.message = msg;
                        } else {
                            s.confirm_archive = selected;
                            s.message =
                                "archive reclaims worktree + runtime — press a again to confirm"
                                    .into();
                        }
                    }
                } else if let Some(t) = s.selected_thread_mut() {
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
                if s.live.is_some() {
                    if let Some(id) = s.selected {
                        let msg = s.live.as_mut().unwrap().toggle_read(id);
                        s.message = msg;
                    }
                } else if let Some(t) = s.selected_thread_mut() {
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
                s.scope = match &s.scope {
                    Scope::Area(_) => Scope::Global,
                    Scope::Global => {
                        if s.live.is_some() {
                            // Area = the focused workspace's name; unnamed
                            // workspaces have no area, so scope stays global.
                            focused_area().map(Scope::Area).unwrap_or(Scope::Global)
                        } else {
                            Scope::Area("work".to_string())
                        }
                    }
                };
                s.message = match &s.scope {
                    Scope::Global => "scope: all areas (non-area workspace fallback)".into(),
                    Scope::Area(a) => format!("scope: area '{a}' (follows focused workspace)"),
                };
            }
            (Some('n'), _) => {
                if s.live.is_some() {
                    let msg = s.live.as_ref().unwrap().new_thread();
                    s.message = msg;
                } else {
                    s.message = "new thread: live mode only (t simulates instead)".into();
                }
            }
            (Some('r'), _) => {
                // Blank form — retyping beats backspacing a long title.
                if s.selected.is_some() {
                    s.rename_buffer = Some(String::new());
                    s.message = "rename: ▏  (⏎ save · Esc cancel)".into();
                } else {
                    s.message = "nothing selected".into();
                }
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
                let count = s.section(Lifecycle::Archived).len();
                s.archived_expanded = !s.archived_expanded;
                s.message = if count == 0 {
                    "no archived threads in scope".into()
                } else {
                    format!(
                        "archived shelf {} ({count})",
                        if s.archived_expanded {
                            "expanded"
                        } else {
                            "collapsed"
                        }
                    )
                };
            }
            _ => dirty = false,
        }

        if dirty {
            regen_live(&mut s);
            rebuild(&list_for_keys, &header_for_keys, &footer_for_keys, &mut s);
        }
        gtk4::glib::Propagation::Stop
    });
    window.add_controller(key_controller);

    // Tick the Working durations once a second; in live mode also re-join the
    // real sessions/windows every other tick.
    let state_for_timer = state.clone();
    let list_for_timer = list_box.clone();
    let header_for_timer = header_box.clone();
    let footer_for_timer = footer.clone();
    let window_for_timer = window.clone();
    let mut tick: u64 = 0;
    gtk4::glib::timeout_add_local(Duration::from_secs(1), move || {
        tick += 1;
        let mut s = state_for_timer.borrow_mut();
        // The live process also owns the Waybar snapshot. Keep its join fresh
        // while the sidebar is hidden; only the GTK rebuild is visibility-
        // gated. LiveWorld::refresh writes the snapshot every other tick.
        if s.live.is_some() && tick.is_multiple_of(2) {
            s.live.as_mut().unwrap().refresh();
        }
        if !window_for_timer.is_visible() {
            return gtk4::glib::ControlFlow::Continue;
        }
        if s.live.is_some() {
            regen_live(&mut s);
            rebuild(
                &list_for_timer,
                &header_for_timer,
                &footer_for_timer,
                &mut s,
            );
            return gtk4::glib::ControlFlow::Continue;
        }
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

    // Every summon (map) re-syncs: fresh world join, scope follows the focused
    // workspace, stale confirmations dropped.
    let state_for_map = state.clone();
    let list_for_map = list_box.clone();
    let header_for_map = header_box.clone();
    let footer_for_map = footer.clone();
    window.connect_map(move |_| {
        let mut s = state_for_map.borrow_mut();
        s.confirm_archive = None;
        if s.live.is_some() {
            s.live.as_mut().unwrap().refresh();
            s.scope = Scope::Global;
            regen_live(&mut s);
        }
        rebuild(&list_for_map, &header_for_map, &footer_for_map, &mut s);
    });

    window.present();
    if live && popup {
        window.set_visible(false);
    }
}

pub fn run(live: bool, popup: bool) -> gtk4::glib::ExitCode {
    // Single-instance: re-invocations (e.g. the Mod+S bind) forward activation
    // to the running instance. Live mode stays mapped as a passive dock and
    // activation toggles its exclusive keyboard command mode; q/Esc releases
    // the grab. Popup and mock modes retain the old visibility toggle.
    let app = Application::builder()
        .application_id("com.thrawny.agent-switch.sidebar-proto")
        .build();
    app.connect_activate(move |app| {
        if let Some(window) = app.windows().first() {
            if live && !popup {
                set_interactive(window, window.keyboard_mode() != KeyboardMode::Exclusive);
            } else if window.is_visible() {
                window.set_visible(false);
            } else {
                window.present();
            }
        } else {
            if live {
                // Keep the process alive while the window is hidden.
                std::mem::forget(app.hold());
            }
            build_proto_ui(app, live, popup);
        }
    });
    app.run_with_args::<&str>(&[])
}
