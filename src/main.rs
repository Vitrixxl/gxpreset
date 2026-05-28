use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

const BASE_URL: &str = "https://musical-artifacts.com";
const METER_SAMPLE_RATE: usize = 48_000;
const METER_FRAME_SAMPLES: usize = 2400;
const MAX_VOLUME_HISTORY: usize = 512;
const VOLUME_DB_GATE: f64 = -38.0;
const VOLUME_DB_CEILING: f64 = 0.0;
const VISUALIZER_MAX_HEIGHT_RATIO: f64 = 0.72;
const PEDAL_BANK_PREFIX: &str = "gxpreset - ";

#[derive(Clone, Debug, Default)]
struct Artifact {
    id: String,
    name: String,
    author: String,
    description: String,
    filename: String,
    download_url: String,
    page_url: String,
    size: String,
    downloads: String,
}

#[derive(Clone, Debug, Default)]
struct AudioNode {
    name: String,
    ports: Vec<String>,
}

#[derive(Clone, Debug)]
struct AudioState {
    outputs: Vec<AudioNode>,
    inputs: Vec<AudioNode>,
    links: HashMap<String, HashSet<String>>,
    out_selected: usize,
    in_selected: usize,
    meter_selected: usize,
    focus: AudioFocus,
    picking_target: bool,
    loading: bool,
    err: String,
    volume_level: f64,
    volume_history: Vec<f64>,
    meter_source: String,
    meter_target: String,
    meter_err: String,
}

impl Default for AudioState {
    fn default() -> Self {
        Self {
            outputs: Vec::new(),
            inputs: Vec::new(),
            links: HashMap::new(),
            out_selected: 0,
            in_selected: 0,
            meter_selected: 0,
            focus: AudioFocus::Connections,
            picking_target: false,
            loading: true,
            err: String::new(),
            volume_level: 0.0,
            volume_history: Vec::new(),
            meter_source: String::new(),
            meter_target: String::new(),
            meter_err: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioFocus {
    Connections,
    Meter,
}

#[derive(Clone, Debug, Default)]
struct GuitarixState {
    banks: Vec<String>,
    presets: Vec<String>,
    presets_bank: String,
    loading_bank: String,
    bank_selected: usize,
    preset_selected: usize,
    focus: usize,
    loading: bool,
    confirm_delete: bool,
    err: String,
    current_bank: String,
    current_preset: String,
}

#[derive(Clone, Debug, Default)]
struct DownloaderStats {
    queued: usize,
    active: usize,
    pending: usize,
    done: usize,
    failed: usize,
    skipped: usize,
}

#[derive(Clone, Debug)]
struct SystemDependency {
    command: &'static str,
    package: &'static str,
    usage: &'static str,
}

#[derive(Clone, Debug, Default)]
struct DependencyStatus {
    missing: Vec<SystemDependency>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AppConfig {
    last_meter_source: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PedalPreset {
    bank: String,
    preset: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PedalGroup {
    name: String,
    #[serde(default)]
    guitarix_bank: String,
    presets: Vec<PedalPreset>,
    current: usize,
    #[serde(default)]
    sync_key: String,
}

#[derive(Clone, Debug)]
struct PedalBankSync {
    bank: String,
    path: PathBuf,
    count: usize,
    removed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PedalsFile {
    groups: Vec<PedalGroup>,
    selected: usize,
}

#[derive(Clone, Debug)]
struct PedalsState {
    groups: Vec<PedalGroup>,
    selected: usize,
    focus: PedalFocus,
    picking_preset: bool,
    picker_focus: usize,
    picker_bank_selected: usize,
    picker_preset_selected: usize,
    picker_bank: String,
    picker_presets: Vec<String>,
    picker_loading: bool,
    picker_err: String,
}

impl Default for PedalsState {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            selected: 0,
            focus: PedalFocus::Groups,
            picking_preset: false,
            picker_focus: 0,
            picker_bank_selected: 0,
            picker_preset_selected: 0,
            picker_bank: String::new(),
            picker_presets: Vec::new(),
            picker_loading: false,
            picker_err: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PedalFocus {
    Groups,
    Presets,
}

#[derive(Clone, Debug)]
struct Args {
    dest: PathBuf,
    query: String,
    order: String,
    page: usize,
    workers: usize,
    force: bool,
    once: bool,
    install_all: bool,
    deps_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Library,
    Audio,
    Pedals,
    Recordings,
    Guitarix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Browse,
    Search,
    RenameRecording,
    NewPedalGroup,
    RenamePedalGroup,
}

#[derive(Clone, Debug, Default)]
struct RecordingItem {
    path: PathBuf,
    name: String,
    size: u64,
}

#[derive(Clone, Debug, Default)]
struct RecordingsState {
    items: Vec<RecordingItem>,
    selected: usize,
    loading: bool,
    err: String,
}

struct App {
    client: Client,
    tx: Sender<AppEvent>,
    active_tab: Tab,
    config: AppConfig,
    deps: DependencyStatus,
    query: String,
    order: String,
    page: usize,
    url: String,
    items: Vec<Artifact>,
    selected: usize,
    loading: bool,
    err: String,
    mode: Mode,
    input: String,
    log: Vec<String>,
    help: bool,
    crawling: bool,
    quitting: bool,
    dest: PathBuf,
    force: bool,
    download_seen: Arc<Mutex<HashSet<String>>>,
    stats: DownloaderStats,
    audio: AudioState,
    pedals: PedalsState,
    recordings: RecordingsState,
    guitarix: GuitarixState,
    meter: Option<MeterControl>,
    meter_seq: u64,
    recording: Option<RecordingControl>,
    playback: Option<PlaybackControl>,
}

struct MeterControl {
    id: u64,
    source: String,
    target: String,
    cancel: Arc<AtomicBool>,
}

struct RecordingControl {
    path: PathBuf,
    started: Instant,
    child: Child,
}

struct PlaybackControl {
    path: PathBuf,
    child: Child,
}

#[derive(Debug)]
enum AppEvent {
    Fetch(Result<FetchResult, String>),
    Audio(Result<AudioSnapshot, String>),
    AudioAction {
        action: String,
        result: Result<(), String>,
    },
    Guitarix(Result<GuitarixSnapshot, String>),
    Preset {
        bank: String,
        preset: String,
        result: Result<(), String>,
    },
    BankDelete {
        bank: String,
        path: String,
        warn: String,
        result: Result<(), String>,
    },
    Download(DownloadEvent),
    Meter {
        id: u64,
        source: String,
        target: String,
        level: f64,
        err: String,
    },
    Recordings(Result<Vec<RecordingItem>, String>),
    PedalPickerPresets {
        bank: String,
        result: Result<Vec<String>, String>,
    },
}

#[derive(Debug)]
struct FetchResult {
    items: Vec<Artifact>,
    raw_url: String,
    query: String,
    order: String,
    page: usize,
}

#[derive(Debug)]
struct AudioSnapshot {
    outputs: Vec<AudioNode>,
    inputs: Vec<AudioNode>,
    links: HashMap<String, HashSet<String>>,
}

#[derive(Debug)]
struct GuitarixSnapshot {
    bank: String,
    banks: Vec<String>,
    presets: Vec<String>,
}

#[derive(Debug)]
enum DownloadEvent {
    Queued { artifact: Artifact },
    Duplicate { artifact: Artifact },
    Started,
    Saved { artifact: Artifact, path: String },
    Exists { artifact: Artifact, path: String },
    Failed { artifact: Artifact, err: String },
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let deps = check_system_dependencies();
    if args.deps_only {
        print_system_dependencies(&deps);
        return Ok(());
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent("gxpreset-rs/0.3 (+https://musical-artifacts.com)")
        .build()?;

    if args.once {
        let result = fetch_artifacts(&client, &args.query, &args.order, args.page)?;
        print_page(
            &args.query,
            &args.order,
            args.page,
            &result.raw_url,
            &result.items,
        );
        return Ok(());
    }

    if args.install_all {
        let result = fetch_artifacts(&client, &args.query, &args.order, args.page)?;
        println!("{}\n{} .gx file(s)", result.raw_url, result.items.len());
        for item in result.items {
            match download_artifact(&client, &args.dest, args.force, item.clone()) {
                Ok((path, skipped)) if skipped => println!("exists: {}", path),
                Ok((path, _)) => println!("saved: {}", path),
                Err(err) => println!(
                    "failed: {}: {}",
                    first_non_empty(&[&item.filename, &item.name, &item.download_url]),
                    err
                ),
            }
        }
        return Ok(());
    }

    run_tui(client, args, deps)
}

fn run_tui(client: Client, args: Args, deps: DependencyStatus) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (tx, rx) = unbounded();
    let config = load_app_config();
    let pedals = load_pedals_file();
    let mut app = App {
        client,
        tx: tx.clone(),
        active_tab: Tab::Audio,
        config,
        deps,
        query: args.query,
        order: args.order,
        page: args.page,
        url: String::new(),
        items: Vec::new(),
        selected: 0,
        loading: true,
        err: String::new(),
        mode: Mode::Browse,
        input: String::new(),
        log: Vec::new(),
        help: true,
        crawling: false,
        quitting: false,
        dest: args.dest,
        force: args.force,
        download_seen: Arc::new(Mutex::new(HashSet::new())),
        stats: DownloaderStats::default(),
        audio: AudioState::default(),
        pedals,
        recordings: RecordingsState {
            loading: true,
            ..RecordingsState::default()
        },
        guitarix: GuitarixState {
            loading: true,
            ..GuitarixState::default()
        },
        meter: None,
        meter_seq: 0,
        recording: None,
        playback: None,
    };

    spawn_fetch(&app);
    spawn_audio_refresh(&app);
    spawn_recordings_refresh(&app);
    spawn_guitarix_refresh(&app, String::new());

    let result = tui_loop(&mut terminal, &mut app, rx);
    app.stop_meter();
    app.stop_recording();
    app.stop_playback();
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: Receiver<AppEvent>,
) -> Result<()> {
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let frame_time = Duration::from_millis(50);
    let mut dirty = true;
    loop {
        while event::poll(Duration::from_millis(2))? {
            if let CEvent::Key(key) = event::read()? {
                if handle_key(app, key) {
                    dirty = true;
                }
            }
        }

        while let Ok(event) = rx.try_recv() {
            apply_event(app, event);
            dirty = true;
        }

        if app.active_tab == Tab::Audio {
            if ensure_meter_stream(app) {
                dirty = true;
            }
        }

        if app.poll_processes() {
            dirty = true;
        }

        if dirty && last_draw.elapsed() >= frame_time {
            terminal.draw(|frame| render(frame, app))?;
            last_draw = Instant::now();
            dirty = false;
        }

        if app.quitting {
            break;
        }
        thread::sleep(Duration::from_millis(4));
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if app.mode == Mode::Search {
        return handle_search_key(app, key);
    }
    if app.mode == Mode::RenameRecording {
        return handle_rename_recording_key(app, key);
    }
    if app.mode == Mode::NewPedalGroup || app.mode == Mode::RenamePedalGroup {
        return handle_pedal_group_input_key(app, key);
    }
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.quitting = true;
            return true;
        }
        KeyCode::Char('q') => {
            app.quitting = true;
            return true;
        }
        KeyCode::Char('?') => {
            app.help = !app.help;
            return true;
        }
        KeyCode::Tab => {
            switch_tab(app, 1);
            return true;
        }
        KeyCode::BackTab => {
            switch_tab(app, -1);
            return true;
        }
        KeyCode::Char(',') | KeyCode::Char('[') => {
            switch_active_pedal(app, -1);
            return true;
        }
        KeyCode::Char(';') | KeyCode::Char(']') => {
            switch_active_pedal(app, 1);
            return true;
        }
        _ => {}
    }

    match app.active_tab {
        Tab::Library => handle_library_key(app, key),
        Tab::Audio => handle_audio_key(app, key),
        Tab::Pedals => handle_pedals_key(app, key),
        Tab::Recordings => handle_recordings_key(app, key),
        Tab::Guitarix => handle_guitarix_key(app, key),
    }
}

fn switch_tab(app: &mut App, direction: i8) {
    app.active_tab = match (app.active_tab, direction.signum()) {
        (Tab::Audio, 1) => Tab::Pedals,
        (Tab::Pedals, 1) => Tab::Library,
        (Tab::Library, 1) => Tab::Recordings,
        (Tab::Recordings, 1) => Tab::Guitarix,
        (Tab::Guitarix, 1) => Tab::Audio,
        (Tab::Audio, -1) => Tab::Guitarix,
        (Tab::Guitarix, -1) => Tab::Recordings,
        (Tab::Recordings, -1) => Tab::Library,
        (Tab::Library, -1) => Tab::Pedals,
        (Tab::Pedals, -1) => Tab::Audio,
        _ => app.active_tab,
    };
    if app.active_tab != Tab::Audio {
        app.stop_meter();
    }
}

fn handle_search_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => {
            app.query = app.input.trim().to_string();
            app.page = 1;
            app.mode = Mode::Browse;
            app.loading = true;
            spawn_fetch(app);
        }
        KeyCode::Esc => {
            app.mode = Mode::Browse;
            app.input.clear();
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => app.input.clear(),
        KeyCode::Char(c) => app.input.push(c),
        _ => {}
    }
    true
}

fn handle_library_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => app.selected = app.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            if app.selected + 1 < app.items.len() {
                app.selected += 1;
            }
        }
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = app.items.len().saturating_sub(1),
        KeyCode::Enter | KeyCode::Char('d') => {
            if let Some(item) = app.items.get(app.selected).cloned() {
                enqueue_download(app, item);
            }
        }
        KeyCode::Char('a') => {
            for item in app.items.clone() {
                enqueue_download(app, item);
            }
        }
        KeyCode::Char('/') => {
            app.input.clear();
            app.mode = Mode::Search;
        }
        KeyCode::Char('n') => {
            app.page += 1;
            app.loading = true;
            spawn_fetch(app);
        }
        KeyCode::Char('p') => {
            app.page = app.page.saturating_sub(1).max(1);
            app.loading = true;
            spawn_fetch(app);
        }
        KeyCode::Char('o') => {
            app.order = next_order(&app.order);
            app.loading = true;
            spawn_fetch(app);
        }
        KeyCode::Char('r') => {
            app.loading = true;
            spawn_fetch(app);
        }
        KeyCode::Char('c') => {
            app.crawling = true;
            let pages = 5;
            for page in 1..=pages {
                let mut clone = app.clone_for_thread();
                clone.page = page;
                spawn_fetch_for_crawl(clone);
            }
        }
        _ => return false,
    }
    true
}

fn handle_audio_key(app: &mut App, key: KeyEvent) -> bool {
    if app.audio.picking_target {
        return handle_audio_picker_key(app, key);
    }
    match key.code {
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('s') => {
            app.audio.focus = AudioFocus::Connections
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('m') => {
            app.audio.focus = AudioFocus::Meter
        }
        KeyCode::Char('r') => {
            app.audio.loading = true;
            spawn_audio_refresh(app);
        }
        KeyCode::Char('R') => toggle_recording(app),
        KeyCode::Enter | KeyCode::Char('c') => {
            if app.audio.focus == AudioFocus::Connections {
                app.audio.picking_target = true;
            }
        }
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('x') => app.audio.picking_target = false,
        KeyCode::Up | KeyCode::Char('k') => match app.audio.focus {
            AudioFocus::Connections => {
                app.audio.out_selected = app.audio.out_selected.saturating_sub(1)
            }
            AudioFocus::Meter => {
                app.audio.meter_selected = app.audio.meter_selected.saturating_sub(1);
                app.stop_meter();
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.audio.focus {
            AudioFocus::Connections => {
                if app.audio.out_selected + 1 < app.audio.outputs.len() {
                    app.audio.out_selected += 1;
                }
            }
            AudioFocus::Meter => {
                if app.audio.meter_selected + 1 < app.audio.outputs.len() {
                    app.audio.meter_selected += 1;
                    app.stop_meter();
                }
            }
        },
        _ => return false,
    }
    true
}

fn handle_recordings_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.recordings.selected = app.recordings.selected.saturating_sub(1)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.recordings.selected + 1 < app.recordings.items.len() {
                app.recordings.selected += 1;
            }
        }
        KeyCode::Home => app.recordings.selected = 0,
        KeyCode::End => {
            app.recordings.selected = app.recordings.items.len().saturating_sub(1);
        }
        KeyCode::Char('r') => {
            app.recordings.loading = true;
            spawn_recordings_refresh(app);
        }
        KeyCode::Enter | KeyCode::Char('p') => start_playback(app),
        KeyCode::Char('s') => app.stop_playback(),
        KeyCode::Char('e') => start_rename_recording(app),
        KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => delete_selected_recording(app),
        _ => return false,
    }
    true
}

fn handle_pedals_key(app: &mut App, key: KeyEvent) -> bool {
    if app.pedals.picking_preset {
        return handle_pedal_picker_key(app, key);
    }
    match key.code {
        KeyCode::Left | KeyCode::Char('h') => app.pedals.focus = PedalFocus::Groups,
        KeyCode::Right | KeyCode::Char('l') => app.pedals.focus = PedalFocus::Presets,
        KeyCode::Up | KeyCode::Char('k') => match app.pedals.focus {
            PedalFocus::Groups => app.pedals.selected = app.pedals.selected.saturating_sub(1),
            PedalFocus::Presets => {
                if let Some(group) = app.pedals.groups.get_mut(app.pedals.selected) {
                    group.current = group.current.saturating_sub(1);
                }
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.pedals.focus {
            PedalFocus::Groups => {
                if app.pedals.selected + 1 < app.pedals.groups.len() {
                    app.pedals.selected += 1;
                }
            }
            PedalFocus::Presets => {
                if let Some(group) = app.pedals.groups.get_mut(app.pedals.selected) {
                    if group.current + 1 < group.presets.len() {
                        group.current += 1;
                    }
                }
            }
        },
        KeyCode::Home => match app.pedals.focus {
            PedalFocus::Groups => app.pedals.selected = 0,
            PedalFocus::Presets => {
                if let Some(group) = app.pedals.groups.get_mut(app.pedals.selected) {
                    group.current = 0;
                }
            }
        },
        KeyCode::End => match app.pedals.focus {
            PedalFocus::Groups => app.pedals.selected = app.pedals.groups.len().saturating_sub(1),
            PedalFocus::Presets => {
                if let Some(group) = app.pedals.groups.get_mut(app.pedals.selected) {
                    group.current = group.presets.len().saturating_sub(1);
                }
            }
        },
        KeyCode::Char('n') => {
            app.input.clear();
            app.mode = Mode::NewPedalGroup;
        }
        KeyCode::Char('e') => start_rename_pedal_group(app),
        KeyCode::Char('a') => start_pedal_picker(app),
        KeyCode::Enter | KeyCode::Char('s') => activate_current_pedal(app),
        KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => {
            delete_selected_pedal_item(app)
        }
        _ => return false,
    }
    persist_pedals(app);
    true
}

fn handle_pedal_picker_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => app.pedals.picking_preset = false,
        KeyCode::Left | KeyCode::Char('h') => app.pedals.picker_focus = 0,
        KeyCode::Right | KeyCode::Char('l') => app.pedals.picker_focus = 1,
        KeyCode::Up | KeyCode::Char('k') => {
            if app.pedals.picker_focus == 0 {
                let index = app.pedals.picker_bank_selected.saturating_sub(1);
                select_pedal_picker_bank(app, index);
            } else {
                app.pedals.picker_preset_selected =
                    app.pedals.picker_preset_selected.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.pedals.picker_focus == 0 {
                if app.pedals.picker_bank_selected + 1 < app.guitarix.banks.len() {
                    select_pedal_picker_bank(app, app.pedals.picker_bank_selected + 1);
                }
            } else if app.pedals.picker_preset_selected + 1 < app.pedals.picker_presets.len() {
                app.pedals.picker_preset_selected += 1;
            }
        }
        KeyCode::Home => {
            if app.pedals.picker_focus == 0 {
                select_pedal_picker_bank(app, 0);
            } else {
                app.pedals.picker_preset_selected = 0;
            }
        }
        KeyCode::End => {
            if app.pedals.picker_focus == 0 {
                select_pedal_picker_bank(app, app.guitarix.banks.len().saturating_sub(1));
            } else {
                app.pedals.picker_preset_selected =
                    app.pedals.picker_presets.len().saturating_sub(1);
            }
        }
        KeyCode::Enter | KeyCode::Char('a') => add_picker_preset_to_group(app),
        _ => return false,
    }
    true
}

fn handle_pedal_group_input_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => match app.mode {
            Mode::NewPedalGroup => finish_new_pedal_group(app),
            Mode::RenamePedalGroup => finish_rename_pedal_group(app),
            _ => {}
        },
        KeyCode::Esc => {
            app.input.clear();
            app.mode = Mode::Browse;
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => app.input.clear(),
        KeyCode::Char(c) => app.input.push(c),
        _ => return false,
    }
    true
}

fn handle_rename_recording_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => finish_rename_recording(app),
        KeyCode::Esc => {
            app.input.clear();
            app.mode = Mode::Browse;
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => app.input.clear(),
        KeyCode::Char(c) => app.input.push(c),
        _ => return false,
    }
    true
}

fn toggle_recording(app: &mut App) {
    if app.recording.is_some() {
        app.stop_recording();
        return;
    }

    let node = app.selected_meter_source();
    if node.name.is_empty() {
        app.add_log("record failed: no meter source selected");
        return;
    }
    let target = meter_target_for_node(&node);
    if target.is_empty() {
        app.add_log("record failed: no capture target");
        return;
    }

    let dir = recordings_dir();
    if let Err(err) = fs::create_dir_all(&dir) {
        app.add_log(format!("record failed: {}", err));
        return;
    }
    let path = dir.join(format!("rec-{}.wav", unix_timestamp_ms()));
    match spawn_record_command(&target, &path) {
        Ok(child) => {
            app.recording = Some(RecordingControl {
                path: path.clone(),
                started: Instant::now(),
                child,
            });
            app.audio.meter_source = node.name;
            app.audio.meter_target = target;
            app.add_log(format!("recording: {}", path.display()));
        }
        Err(err) => app.add_log(format!("record failed: {}", err)),
    }
}

fn start_playback(app: &mut App) {
    let Some(item) = app.selected_recording() else {
        app.add_log("playback failed: no recording selected");
        return;
    };
    app.stop_playback();
    match spawn_playback_command(&item.path) {
        Ok(child) => {
            app.playback = Some(PlaybackControl {
                path: item.path.clone(),
                child,
            });
            app.add_log(format!("playing: {}", item.path.display()));
        }
        Err(err) => app.add_log(format!("playback failed: {}", err)),
    }
}

fn start_rename_recording(app: &mut App) {
    let Some(item) = app.selected_recording() else {
        app.add_log("rename failed: no recording selected");
        return;
    };
    app.input = item
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&item.name)
        .to_string();
    app.mode = Mode::RenameRecording;
}

fn finish_rename_recording(app: &mut App) {
    let Some(item) = app.selected_recording() else {
        app.mode = Mode::Browse;
        app.input.clear();
        return;
    };
    let basename = sanitize_recording_name(&app.input);
    let dest = item.path.with_file_name(format!("{}.wav", basename));
    app.mode = Mode::Browse;
    app.input.clear();
    if dest == item.path {
        return;
    }
    if dest.exists() {
        app.add_log(format!("rename failed: {} already exists", dest.display()));
        return;
    }
    if app
        .playback
        .as_ref()
        .is_some_and(|playback| playback.path == item.path)
    {
        app.stop_playback();
    }
    match fs::rename(&item.path, &dest) {
        Ok(()) => {
            app.add_log(format!("renamed: {}", dest.display()));
            app.recordings.loading = true;
            spawn_recordings_refresh(app);
        }
        Err(err) => app.add_log(format!("rename failed: {}", err)),
    }
}

fn delete_selected_recording(app: &mut App) {
    let Some(item) = app.selected_recording() else {
        app.add_log("delete failed: no recording selected");
        return;
    };
    if app
        .playback
        .as_ref()
        .is_some_and(|playback| playback.path == item.path)
    {
        app.stop_playback();
    }
    if app
        .recording
        .as_ref()
        .is_some_and(|recording| recording.path == item.path)
    {
        app.add_log("delete skipped: recording is still active");
        return;
    }
    match fs::remove_file(&item.path) {
        Ok(()) => {
            app.add_log(format!("deleted recording: {}", item.path.display()));
            app.recordings.loading = true;
            spawn_recordings_refresh(app);
        }
        Err(err) => app.add_log(format!("delete failed: {}", err)),
    }
}

fn finish_new_pedal_group(app: &mut App) {
    let name = sanitize_group_name(&app.input);
    app.input.clear();
    app.mode = Mode::Browse;
    if name.is_empty() {
        app.add_log("pedal group skipped: empty name");
        return;
    }
    app.pedals.groups.push(PedalGroup {
        guitarix_bank: generated_pedal_bank_name(&name),
        name,
        presets: Vec::new(),
        current: 0,
        sync_key: String::new(),
    });
    app.pedals.selected = app.pedals.groups.len().saturating_sub(1);
    if let Err(err) = save_pedals_state(&app.pedals) {
        app.add_log(format!("pedal save failed: {}", err));
    }
}

fn persist_pedals(app: &mut App) {
    if let Err(err) = save_pedals_state(&app.pedals) {
        app.add_log(format!("pedal save failed: {}", err));
    }
}

fn start_rename_pedal_group(app: &mut App) {
    if let Some(group) = app.pedals.groups.get(app.pedals.selected) {
        app.input = group.name.clone();
        app.mode = Mode::RenamePedalGroup;
    } else {
        app.add_log("rename pedal group failed: no group selected");
    }
}

fn finish_rename_pedal_group(app: &mut App) {
    let name = sanitize_group_name(&app.input);
    app.input.clear();
    app.mode = Mode::Browse;
    if name.is_empty() {
        app.add_log("rename pedal group skipped: empty name");
        return;
    }
    if let Some(group) = app.pedals.groups.get_mut(app.pedals.selected) {
        group.name = name;
        group.sync_key.clear();
        if let Err(err) = save_pedals_state(&app.pedals) {
            app.add_log(format!("pedal save failed: {}", err));
        }
        sync_selected_pedal_group(app);
    }
}

fn start_pedal_picker(app: &mut App) {
    if app.pedals.groups.is_empty() {
        app.add_log("add pedal preset failed: create a group first");
        return;
    }
    app.pedals.picking_preset = true;
    app.pedals.picker_focus = 0;
    app.pedals.picker_bank_selected = app
        .pedals
        .picker_bank_selected
        .min(app.guitarix.banks.len().saturating_sub(1));
    if app.guitarix.banks.is_empty() {
        app.pedals.picker_bank.clear();
        app.pedals.picker_presets.clear();
        app.pedals.picker_loading = true;
        spawn_guitarix_refresh(app, String::new());
    } else {
        select_pedal_picker_bank(app, app.pedals.picker_bank_selected);
    }
}

fn select_pedal_picker_bank(app: &mut App, index: usize) {
    if app.guitarix.banks.is_empty() {
        app.pedals.picker_bank.clear();
        app.pedals.picker_presets.clear();
        app.pedals.picker_loading = false;
        return;
    }
    let index = index.min(app.guitarix.banks.len().saturating_sub(1));
    let bank = app.guitarix.banks[index].clone();
    if app.pedals.picker_bank == bank && !app.pedals.picker_presets.is_empty() {
        app.pedals.picker_bank_selected = index;
        return;
    }
    app.pedals.picker_bank_selected = index;
    app.pedals.picker_bank = bank.clone();
    app.pedals.picker_preset_selected = 0;
    app.pedals.picker_presets.clear();
    app.pedals.picker_err.clear();
    app.pedals.picker_loading = true;
    spawn_pedal_picker_presets(app, bank);
}

fn add_picker_preset_to_group(app: &mut App) {
    let bank = app.pedals.picker_bank.clone();
    let preset = app
        .pedals
        .picker_presets
        .get(app.pedals.picker_preset_selected)
        .cloned()
        .unwrap_or_default();
    if bank.is_empty() || preset.is_empty() {
        app.add_log("add pedal preset failed: no preset selected");
        return;
    }
    if let Some(group) = app.pedals.groups.get_mut(app.pedals.selected) {
        group.presets.push(PedalPreset { bank, preset });
        group.current = group.presets.len().saturating_sub(1);
        group.sync_key.clear();
        app.pedals.picking_preset = false;
        if let Err(err) = save_pedals_state(&app.pedals) {
            app.add_log(format!("pedal save failed: {}", err));
        }
        sync_selected_pedal_group(app);
    }
}

fn delete_selected_pedal_item(app: &mut App) {
    let mut removed_group_bank = String::new();
    match app.pedals.focus {
        PedalFocus::Groups => {
            if app.pedals.groups.is_empty() {
                return;
            }
            let removed = app.pedals.groups.remove(app.pedals.selected);
            removed_group_bank = removed.guitarix_bank.clone();
            app.pedals.selected = app
                .pedals
                .selected
                .min(app.pedals.groups.len().saturating_sub(1));
            app.add_log(format!("deleted pedal group: {}", removed.name));
        }
        PedalFocus::Presets => {
            if let Some(group) = app.pedals.groups.get_mut(app.pedals.selected) {
                if group.presets.is_empty() {
                    return;
                }
                let removed = group
                    .presets
                    .remove(group.current.min(group.presets.len() - 1));
                group.current = group.current.min(group.presets.len().saturating_sub(1));
                group.sync_key.clear();
                app.add_log(format!(
                    "deleted pedal preset: {} / {}",
                    removed.bank, removed.preset
                ));
            }
        }
    }
    if let Err(err) = save_pedals_state(&app.pedals) {
        app.add_log(format!("pedal save failed: {}", err));
    }
    if !removed_group_bank.is_empty() {
        match remove_generated_pedal_bank(&removed_group_bank, &app.dest) {
            Ok(Some(path)) => {
                app.add_log(format!("removed pedal bank: {}", path.display()));
                spawn_guitarix_bank_reparse(app, String::new());
            }
            Ok(None) => {}
            Err(err) => app.add_log(format!("remove pedal bank failed: {}", err)),
        }
    } else if app.pedals.focus == PedalFocus::Presets {
        sync_selected_pedal_group(app);
    }
}

fn activate_current_pedal(app: &mut App) {
    let group_index = app.pedals.selected;
    let preset_index = app
        .pedals
        .groups
        .get(group_index)
        .map(|group| group.current)
        .unwrap_or_default();
    activate_pedal_preset(app, group_index, preset_index);
}

fn switch_active_pedal(app: &mut App, direction: isize) {
    let group_index = app.pedals.selected;
    let Some(group) = app.pedals.groups.get(group_index) else {
        app.add_log("pedal switch skipped: no group selected");
        return;
    };
    let len = group.presets.len();
    if len == 0 {
        app.add_log(format!("pedal switch skipped: {} is empty", group.name));
        return;
    }
    let current = group.current.min(len - 1);
    let next = if direction >= 0 {
        (current + 1) % len
    } else if current == 0 {
        len - 1
    } else {
        current - 1
    };
    activate_pedal_preset(app, group_index, next);
}

fn activate_pedal_preset(app: &mut App, group_index: usize, preset_index: usize) {
    if group_index >= app.pedals.groups.len() {
        app.add_log("pedal load failed: no group selected");
        return;
    }
    if app.pedals.groups[group_index].presets.is_empty() {
        let group_name = app.pedals.groups[group_index].name.clone();
        app.add_log(format!("pedal load skipped: {} is empty", group_name));
        return;
    }
    if !ensure_pedal_group_materialized(app, group_index) {
        return;
    }
    let (group_name, bank, source, generated_preset) = {
        let group = &mut app.pedals.groups[group_index];
        let preset_index = preset_index.min(group.presets.len().saturating_sub(1));
        group.current = preset_index;
        let source = group.presets[preset_index].clone();
        (
            group.name.clone(),
            pedal_group_bank_name(group),
            source.clone(),
            generated_pedal_preset_name(preset_index, &source),
        )
    };
    if let Err(err) = save_pedals_state(&app.pedals) {
        app.add_log(format!("pedal save failed: {}", err));
    }
    app.add_log(format!(
        "pedal {}: {} / {}",
        group_name, source.bank, source.preset
    ));
    spawn_set_preset(app, bank, generated_preset);
}

fn ensure_pedal_group_materialized(app: &mut App, group_index: usize) -> bool {
    let needs_sync = app
        .pedals
        .groups
        .get(group_index)
        .is_some_and(|group| pedal_group_needs_sync(group, &app.dest));
    let mut reloaded = false;
    if needs_sync {
        match sync_pedal_group_by_index(app, group_index) {
            Ok(sync) => {
                log_pedal_sync_result(app, &sync);
                if sync.count > 0 {
                    match guitarix_bank_check_reparse() {
                        Ok(_) => reloaded = true,
                        Err(err) => app.add_log(format!("guitarix bank reload failed: {}", err)),
                    }
                }
            }
            Err(err) => {
                app.add_log(format!("pedal bank sync failed: {}", err));
                return false;
            }
        }
    }
    let bank = app
        .pedals
        .groups
        .get(group_index)
        .map(pedal_group_bank_name)
        .unwrap_or_default();
    if !bank.is_empty() && reloaded {
        spawn_guitarix_refresh(app, bank);
    } else if !bank.is_empty() && !app.guitarix.banks.iter().any(|known| known == &bank) {
        match guitarix_bank_check_reparse() {
            Ok(_) => spawn_guitarix_refresh(app, bank),
            Err(err) => app.add_log(format!("guitarix bank reload failed: {}", err)),
        }
    }
    true
}

fn sync_selected_pedal_group(app: &mut App) {
    if app.pedals.groups.is_empty() {
        return;
    }
    let group_index = app.pedals.selected.min(app.pedals.groups.len() - 1);
    match sync_pedal_group_by_index(app, group_index) {
        Ok(sync) => {
            log_pedal_sync_result(app, &sync);
            if sync.count > 0 || sync.removed {
                let preferred = if sync.count > 0 {
                    sync.bank.clone()
                } else {
                    String::new()
                };
                spawn_guitarix_bank_reparse(app, preferred);
            }
        }
        Err(err) => app.add_log(format!("pedal bank sync failed: {}", err)),
    }
}

fn sync_pedal_group_by_index(app: &mut App, group_index: usize) -> Result<PedalBankSync> {
    let dir = app.dest.clone();
    let sync = {
        let group = app
            .pedals
            .groups
            .get_mut(group_index)
            .ok_or_else(|| anyhow::anyhow!("no pedal group selected"))?;
        materialize_pedal_group_bank(group, &dir)?
    };
    save_pedals_state(&app.pedals)?;
    Ok(sync)
}

fn log_pedal_sync_result(app: &mut App, sync: &PedalBankSync) {
    if sync.count > 0 {
        app.add_log(format!(
            "pedal bank ready: {} ({} presets)",
            sync.bank, sync.count
        ));
    } else if sync.removed {
        app.add_log(format!("pedal bank removed: {}", sync.path.display()));
    }
}

fn handle_audio_picker_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => app.audio.picking_target = false,
        KeyCode::Up | KeyCode::Char('k') => {
            app.audio.in_selected = app.audio.in_selected.saturating_sub(1)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.audio.in_selected + 1 < app.audio.inputs.len() {
                app.audio.in_selected += 1;
            }
        }
        KeyCode::Home => app.audio.in_selected = 0,
        KeyCode::End => app.audio.in_selected = app.audio.inputs.len().saturating_sub(1),
        KeyCode::Char('r') => {
            app.audio.picking_target = false;
            app.audio.loading = true;
            spawn_audio_refresh(app);
        }
        KeyCode::Enter | KeyCode::Char('c') => {
            let out = app.selected_output();
            let input = app.selected_input();
            app.audio.picking_target = false;
            if !out.name.is_empty() && !input.name.is_empty() {
                spawn_audio_action(app, out, input, false);
            }
        }
        KeyCode::Char(' ') => {
            let out = app.selected_output();
            let input = app.selected_input();
            if !out.name.is_empty() && !input.name.is_empty() {
                let disconnect = app.nodes_connected(&out, &input);
                spawn_audio_action(app, out, input, disconnect);
            }
        }
        KeyCode::Char('x') | KeyCode::Backspace => {
            let out = app.selected_output();
            let input = app.selected_input();
            app.audio.picking_target = false;
            if !out.name.is_empty() && !input.name.is_empty() {
                spawn_audio_action(app, out, input, true);
            }
        }
        _ => return false,
    }
    true
}

fn handle_guitarix_key(app: &mut App, key: KeyEvent) -> bool {
    if app.guitarix.confirm_delete {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let bank = app.selected_bank();
                app.guitarix.confirm_delete = false;
                if !bank.is_empty() {
                    spawn_delete_bank(app, bank);
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => app.guitarix.confirm_delete = false,
            _ => return false,
        }
        return true;
    }
    match key.code {
        KeyCode::Left | KeyCode::Char('h') => app.guitarix.focus = 0,
        KeyCode::Right | KeyCode::Char('l') => app.guitarix.focus = 1,
        KeyCode::Up | KeyCode::Char('k') => {
            if app.guitarix.focus == 0 {
                app.guitarix.bank_selected = app.guitarix.bank_selected.saturating_sub(1);
                request_guitarix_refresh(app, app.selected_bank());
            } else {
                app.guitarix.preset_selected = app.guitarix.preset_selected.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.guitarix.focus == 0 {
                if app.guitarix.bank_selected + 1 < app.guitarix.banks.len() {
                    app.guitarix.bank_selected += 1;
                    request_guitarix_refresh(app, app.selected_bank());
                }
            } else if app.guitarix.preset_selected + 1 < app.guitarix.presets.len() {
                app.guitarix.preset_selected += 1;
            }
        }
        KeyCode::Char('r') => {
            request_guitarix_refresh(app, app.selected_bank());
        }
        KeyCode::Enter | KeyCode::Char('s') => {
            let bank = app.selected_bank();
            let preset = app.selected_preset();
            if !bank.is_empty() && !preset.is_empty() {
                spawn_set_preset(app, bank, preset);
            }
        }
        KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => {
            if app.guitarix.focus == 0 && !app.selected_bank().is_empty() {
                app.guitarix.confirm_delete = true;
            }
        }
        _ => return false,
    }
    true
}

impl App {
    fn clone_for_thread(&self) -> ThreadCtx {
        ThreadCtx {
            client: self.client.clone(),
            tx: self.tx.clone(),
            query: self.query.clone(),
            order: self.order.clone(),
            page: self.page,
            dest: self.dest.clone(),
            force: self.force,
        }
    }

    fn add_log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > 80 {
            self.log.drain(0..self.log.len() - 80);
        }
    }

    fn stop_meter(&mut self) {
        if let Some(meter) = &self.meter {
            meter.cancel.store(true, Ordering::Relaxed);
        }
        self.meter = None;
    }

    fn stop_recording(&mut self) {
        if let Some(mut recording) = self.recording.take() {
            interrupt_child(&mut recording.child);
            self.add_log(format!("record saved: {}", recording.path.display()));
            self.recordings.loading = true;
            spawn_recordings_refresh(self);
        }
    }

    fn stop_playback(&mut self) {
        if let Some(mut playback) = self.playback.take() {
            interrupt_child(&mut playback.child);
            self.add_log(format!("playback stopped: {}", playback.path.display()));
        }
    }

    fn poll_processes(&mut self) -> bool {
        let recording_done = if let Some(recording) = self.recording.as_mut() {
            match recording.child.try_wait() {
                Ok(Some(status)) => Some((recording.path.clone(), status.to_string())),
                Ok(None) => None,
                Err(err) => Some((recording.path.clone(), err.to_string())),
            }
        } else {
            None
        };
        if let Some((path, status)) = recording_done {
            self.recording = None;
            self.add_log(format!("recording ended: {} ({})", path.display(), status));
            self.recordings.loading = true;
            spawn_recordings_refresh(self);
            return true;
        }

        let playback_done = if let Some(playback) = self.playback.as_mut() {
            match playback.child.try_wait() {
                Ok(Some(_)) => Some(playback.path.clone()),
                Ok(None) => None,
                Err(_) => Some(playback.path.clone()),
            }
        } else {
            None
        };
        if let Some(path) = playback_done {
            self.playback = None;
            self.add_log(format!("playback done: {}", path.display()));
            return true;
        }
        false
    }

    fn selected_output(&self) -> AudioNode {
        self.audio
            .outputs
            .get(self.audio.out_selected)
            .cloned()
            .unwrap_or_default()
    }

    fn selected_input(&self) -> AudioNode {
        self.audio
            .inputs
            .get(self.audio.in_selected)
            .cloned()
            .unwrap_or_default()
    }

    fn selected_meter_source(&self) -> AudioNode {
        self.audio
            .outputs
            .get(self.audio.meter_selected)
            .cloned()
            .unwrap_or_default()
    }

    fn selected_meter_source_name(&self) -> String {
        self.selected_meter_source().name
    }

    fn selected_recording(&self) -> Option<RecordingItem> {
        self.recordings.items.get(self.recordings.selected).cloned()
    }

    fn selected_bank(&self) -> String {
        self.guitarix
            .banks
            .get(self.guitarix.bank_selected)
            .cloned()
            .unwrap_or_default()
    }

    fn selected_preset(&self) -> String {
        self.guitarix
            .presets
            .get(self.guitarix.preset_selected)
            .cloned()
            .unwrap_or_default()
    }

    fn nodes_connected(&self, out: &AudioNode, input: &AudioNode) -> bool {
        out.ports.iter().any(|out_port| {
            input.ports.iter().any(|in_port| {
                self.audio
                    .links
                    .get(out_port)
                    .is_some_and(|targets| targets.contains(in_port))
            })
        })
    }

    fn linked_targets(&self, out: &AudioNode) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut targets = Vec::new();
        for out_port in &out.ports {
            if let Some(links) = self.audio.links.get(out_port) {
                for in_port in links {
                    let name = node_name_for_port(&self.audio.inputs, in_port)
                        .unwrap_or_else(|| in_port.clone());
                    if seen.insert(name.clone()) {
                        targets.push(name);
                    }
                }
            }
        }
        targets.sort();
        targets
    }
}

#[derive(Clone)]
struct ThreadCtx {
    client: Client,
    tx: Sender<AppEvent>,
    query: String,
    order: String,
    page: usize,
    dest: PathBuf,
    force: bool,
}

fn apply_event(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::Fetch(result) => {
            app.loading = false;
            app.crawling = false;
            match result {
                Ok(result) => {
                    app.err.clear();
                    app.items = result.items;
                    app.url = result.raw_url;
                    app.query = result.query;
                    app.order = result.order;
                    app.page = result.page;
                    app.selected = app.selected.min(app.items.len().saturating_sub(1));
                }
                Err(err) => app.err = err,
            }
        }
        AppEvent::Audio(result) => {
            app.audio.loading = false;
            match result {
                Ok(snapshot) => {
                    app.audio.err.clear();
                    app.audio.outputs = snapshot.outputs;
                    app.audio.inputs = snapshot.inputs;
                    app.audio.links = snapshot.links;
                    app.audio.out_selected = app
                        .audio
                        .out_selected
                        .min(app.audio.outputs.len().saturating_sub(1));
                    app.audio.in_selected = app
                        .audio
                        .in_selected
                        .min(app.audio.inputs.len().saturating_sub(1));
                    app.audio.meter_selected = app
                        .audio
                        .meter_selected
                        .min(app.audio.outputs.len().saturating_sub(1));
                    if let Some(index) =
                        audio_node_index_by_name(&app.audio.outputs, &app.config.last_meter_source)
                    {
                        app.audio.meter_selected = index;
                    }
                }
                Err(err) => app.audio.err = err,
            }
        }
        AppEvent::AudioAction { action, result } => {
            match result {
                Ok(()) => app.add_log(format!("{} ok", action)),
                Err(err) => app.add_log(format!("{} failed: {}", action, err)),
            }
            app.audio.loading = true;
            spawn_audio_refresh(app);
        }
        AppEvent::Guitarix(result) => match result {
            Ok(snapshot) => {
                app.guitarix.err.clear();
                app.guitarix.banks = snapshot.banks;
                app.guitarix.bank_selected = app
                    .guitarix
                    .bank_selected
                    .min(app.guitarix.banks.len().saturating_sub(1));
                let selected_bank = app.selected_bank();
                if app.guitarix.loading_bank.is_empty()
                    || snapshot.bank == selected_bank
                    || selected_bank.is_empty()
                {
                    app.guitarix.presets = snapshot.presets;
                    app.guitarix.presets_bank = snapshot.bank;
                    app.guitarix.loading = false;
                    app.guitarix.loading_bank.clear();
                    app.guitarix.preset_selected = app
                        .guitarix
                        .preset_selected
                        .min(app.guitarix.presets.len().saturating_sub(1));
                }
                if app.pedals.picking_preset
                    && app.pedals.picker_bank.is_empty()
                    && !app.guitarix.banks.is_empty()
                {
                    select_pedal_picker_bank(app, 0);
                }
            }
            Err(err) => {
                app.guitarix.loading = false;
                app.guitarix.loading_bank.clear();
                app.guitarix.err = err;
            }
        },
        AppEvent::Preset {
            bank,
            preset,
            result,
        } => match result {
            Ok(()) => {
                app.guitarix.current_bank = bank.clone();
                app.guitarix.current_preset = preset.clone();
                app.add_log(format!("guitarix preset: {} / {}", bank, preset));
            }
            Err(err) => app.add_log(format!("guitarix preset failed: {}", err)),
        },
        AppEvent::BankDelete {
            bank,
            path,
            warn,
            result,
        } => {
            match result {
                Ok(()) => {
                    app.add_log(format!("deleted bank: {} ({})", bank, path));
                    if !warn.is_empty() {
                        app.add_log(warn);
                    }
                }
                Err(err) => app.add_log(format!("delete bank failed: {}", err)),
            }
            app.guitarix.loading = true;
            app.guitarix.loading_bank.clear();
            app.guitarix.presets.clear();
            app.guitarix.presets_bank.clear();
            spawn_guitarix_refresh(app, String::new());
        }
        AppEvent::Download(event) => apply_download_event(app, event),
        AppEvent::Meter {
            id,
            source,
            target,
            level,
            err,
        } => {
            if app.meter.as_ref().is_none_or(|meter| meter.id != id) {
                return;
            }
            if !err.is_empty() {
                app.audio.meter_err = err;
                app.stop_meter();
                return;
            }
            app.audio.meter_err.clear();
            app.audio.meter_source = source;
            app.audio.meter_target = target;
            let shown = smooth_volume(app.audio.volume_level, level);
            app.audio.volume_level = shown;
            app.audio.volume_history.insert(0, shown);
            if app.audio.volume_history.len() > MAX_VOLUME_HISTORY {
                app.audio.volume_history.truncate(MAX_VOLUME_HISTORY);
            }
        }
        AppEvent::Recordings(result) => {
            app.recordings.loading = false;
            match result {
                Ok(items) => {
                    app.recordings.err.clear();
                    app.recordings.items = items;
                    app.recordings.selected = app
                        .recordings
                        .selected
                        .min(app.recordings.items.len().saturating_sub(1));
                }
                Err(err) => app.recordings.err = err,
            }
        }
        AppEvent::PedalPickerPresets { bank, result } => {
            if app.pedals.picker_bank != bank {
                return;
            }
            app.pedals.picker_loading = false;
            match result {
                Ok(presets) => {
                    app.pedals.picker_err.clear();
                    app.pedals.picker_presets = presets;
                    app.pedals.picker_preset_selected = app
                        .pedals
                        .picker_preset_selected
                        .min(app.pedals.picker_presets.len().saturating_sub(1));
                }
                Err(err) => app.pedals.picker_err = err,
            }
        }
    }
}

fn apply_download_event(app: &mut App, event: DownloadEvent) {
    match event {
        DownloadEvent::Queued { artifact } => {
            app.stats.queued += 1;
            app.stats.pending += 1;
            app.add_log(format!(
                "queued: {}",
                first_non_empty(&[&artifact.filename, &artifact.name])
            ));
        }
        DownloadEvent::Duplicate { artifact } => app.add_log(format!(
            "already queued: {}",
            first_non_empty(&[&artifact.filename, &artifact.name])
        )),
        DownloadEvent::Started => {
            app.stats.pending = app.stats.pending.saturating_sub(1);
            app.stats.active += 1;
        }
        DownloadEvent::Saved { artifact, path } => {
            app.stats.active = app.stats.active.saturating_sub(1);
            app.stats.done += 1;
            app.add_log(format!(
                "saved: {}",
                first_non_empty(&[&artifact.filename, &artifact.name, &path])
            ));
        }
        DownloadEvent::Exists { artifact, path } => {
            app.stats.active = app.stats.active.saturating_sub(1);
            app.stats.skipped += 1;
            app.add_log(format!(
                "exists: {}",
                first_non_empty(&[&artifact.filename, &artifact.name, &path])
            ));
        }
        DownloadEvent::Failed { artifact, err } => {
            app.stats.active = app.stats.active.saturating_sub(1);
            app.stats.failed += 1;
            app.add_log(format!(
                "failed: {}: {}",
                first_non_empty(&[&artifact.filename, &artifact.name]),
                err
            ));
        }
    }
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        dest: default_bank_dir(),
        query: String::new(),
        order: "created_at".to_string(),
        page: 1,
        workers: 4,
        force: false,
        once: false,
        install_all: false,
        deps_only: false,
    };
    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-dir" | "--dir" => {
                args.dest = PathBuf::from(iter.next().context("-dir requires a value")?)
            }
            "-search" | "--search" => {
                args.query = iter.next().context("-search requires a value")?
            }
            "-order" | "--order" => args.order = iter.next().context("-order requires a value")?,
            "-page" | "--page" => {
                args.page = iter
                    .next()
                    .context("-page requires a value")?
                    .parse()
                    .unwrap_or(1)
            }
            "-workers" | "--workers" => {
                args.workers = iter
                    .next()
                    .context("-workers requires a value")?
                    .parse()
                    .unwrap_or(4)
                    .max(1)
            }
            "-force" | "--force" => args.force = true,
            "-once" | "--once" => args.once = true,
            "-install-all" | "--install-all" => args.install_all = true,
            "-deps" | "--deps" => args.deps_only = true,
            "-h" | "--help" => {
                println!("gxpreset [-dir DIR] [-search QUERY] [-order ORDER] [-page N] [-workers N] [-force] [-once] [-install-all] [-deps]");
                std::process::exit(0);
            }
            other => return Err(anyhow::anyhow!("unknown argument: {}", other)),
        }
    }
    args.page = args.page.max(1);
    Ok(args)
}

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let area = area.inner(Margin {
        horizontal: 4,
        vertical: 2,
    });
    if area.width < 20 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("terminal too small").style(error_style()),
            area,
        );
        return;
    }
    let footer_h = if app.help { 5 } else { 4 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(1),
            Constraint::Length(footer_h),
        ])
        .split(area);
    render_header(frame, app, chunks[0]);
    match app.active_tab {
        Tab::Library => render_library(frame, app, chunks[1]),
        Tab::Audio => render_audio(frame, app, chunks[1]),
        Tab::Pedals => render_pedals(frame, app, chunks[1]),
        Tab::Recordings => render_recordings(frame, app, chunks[1]),
        Tab::Guitarix => render_guitarix(frame, app, chunks[1]),
    }
    render_footer(frame, app, chunks[2]);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let state = if app.loading {
        Span::styled("LOADING", active_badge_style())
    } else if !app.err.is_empty() {
        Span::styled("ERROR", error_style())
    } else if app.crawling {
        Span::styled("CRAWL", active_badge_style())
    } else {
        Span::styled("READY", success_style())
    };
    let tabs = vec![
        tab_span(app, Tab::Audio, "Audio"),
        Span::raw(" "),
        tab_span(app, Tab::Pedals, "Pedals"),
        Span::raw(" "),
        tab_span(app, Tab::Library, "Library"),
        Span::raw(" "),
        tab_span(app, Tab::Recordings, "Records"),
        Span::raw(" "),
        tab_span(app, Tab::Guitarix, "Guitarix"),
    ];
    let lines = vec![
        Line::from(vec![
            Span::styled("gxpreset", title_style()),
            Span::raw(" "),
            Span::styled("Guitarix rig control", muted_style()),
        ]),
        Line::from(tabs),
        header_status_line(app, state),
        header_path_line(app, area.width),
    ];
    frame.render_widget(Paragraph::new(lines).block(panel_block("", false)), area);
}

fn header_status_line(app: &App, state: Span<'static>) -> Line<'static> {
    if app.active_tab != Tab::Library {
        return Line::from(pedal_status_spans(app));
    }
    let query = if app.query.is_empty() {
        "all guitarix"
    } else {
        &app.query
    };
    Line::from(vec![
        state,
        Span::raw("  "),
        Span::styled(query.to_string(), accent_style()),
        Span::raw("  "),
        Span::styled(format!("page {}", app.page), badge_style()),
        Span::raw(" "),
        Span::styled(format!("order {}", app.order), badge_style()),
        Span::raw(" "),
        Span::styled(format!("{} files", app.items.len()), badge_style()),
    ])
}

fn header_path_line(app: &App, width: u16) -> Line<'static> {
    if app.active_tab != Tab::Library {
        return Line::from("");
    }
    Line::from(Span::styled(
        truncate(
            &app.dest.display().to_string(),
            width.saturating_sub(4) as usize,
        ),
        muted_style(),
    ))
}

fn pedal_status_spans(app: &App) -> Vec<Span<'static>> {
    if let Some(group) = app.pedals.groups.get(app.pedals.selected) {
        let current = group.current.min(group.presets.len().saturating_sub(1));
        if let Some(preset) = group.presets.get(current) {
            return vec![
                Span::styled("PEDAL", active_badge_style()),
                Span::raw(" "),
                Span::styled(group.name.clone(), accent_style()),
                Span::raw(" "),
                Span::styled(
                    format!("{}/{}", current + 1, group.presets.len()),
                    badge_style(),
                ),
                Span::raw(" "),
                Span::raw(format!("{} / {}", preset.bank, preset.preset)),
                Span::raw(" "),
                Span::styled(", prev  ; next", muted_style()),
            ];
        }
        return vec![
            Span::styled("PEDAL", active_badge_style()),
            Span::raw(" "),
            Span::styled(group.name.clone(), accent_style()),
            Span::raw(" "),
            Span::styled("empty", muted_style()),
        ];
    }
    vec![
        Span::styled("PEDAL", active_badge_style()),
        Span::raw(" "),
        Span::styled("none", muted_style()),
    ]
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("queued {}", app.stats.queued), badge_style()),
        Span::raw(" "),
        Span::styled(format!("active {}", app.stats.active), active_badge_style()),
        Span::raw(" "),
        Span::styled(format!("pending {}", app.stats.pending), badge_style()),
        Span::raw(" "),
        Span::styled(format!("done {}", app.stats.done), success_style()),
        Span::raw(" "),
        Span::styled(format!("skipped {}", app.stats.skipped), muted_style()),
        Span::raw(" "),
        Span::styled(
            format!("failed {}", app.stats.failed),
            if app.stats.failed > 0 {
                error_style()
            } else {
                muted_style()
            },
        ),
    ])];
    lines[0].spans.push(Span::raw(" "));
    lines[0].spans.extend(pedal_status_spans(app));
    if let Some(recording) = &app.recording {
        lines[0].spans.push(Span::raw(" "));
        lines[0].spans.push(Span::styled(
            format!("rec {}", format_elapsed(recording.started.elapsed())),
            error_style(),
        ));
    }
    if let Some(playback) = &app.playback {
        lines[0].spans.push(Span::raw(" "));
        lines[0].spans.push(Span::styled(
            format!(
                "play {}",
                playback
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("recording.wav")
            ),
            active_badge_style(),
        ));
    }
    if !app.deps.missing.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("install: {}", app.deps.install_command()),
            accent_style(),
        )));
    }
    for line in app.log.iter().rev().take(2).rev() {
        lines.push(Line::from(Span::styled(
            truncate(line, area.width.saturating_sub(4) as usize),
            muted_style(),
        )));
    }
    let help = if app.help {
        help_line(app)
    } else {
        "? help".to_string()
    };
    lines.push(Line::from(Span::styled(help, muted_style())));
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Status", false)),
        area,
    );
}

fn render_library(frame: &mut Frame, app: &App, area: Rect) {
    if app.mode == Mode::Search {
        let body = vec![
            Line::from("Type a search and press Enter. Esc cancels."),
            Line::from(""),
            Line::from(Span::styled(format!(" {}_ ", app.input), selected_style())),
        ];
        frame.render_widget(
            Paragraph::new(body).block(panel_block("Search", true)),
            area,
        );
        return;
    }
    if area.width >= 104 {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Min(20)])
            .split(area);
        render_presets_list(frame, app, chunks[0], true);
        render_artifact_detail(frame, app, chunks[1]);
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Min(5)])
            .split(area);
        render_presets_list(frame, app, chunks[0], true);
        render_artifact_detail(frame, app, chunks[1]);
    }
}

fn render_presets_list(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let mut lines = Vec::new();
    if app.loading && app.items.is_empty() {
        lines.push(Line::from(Span::styled(" loading ", active_badge_style())));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Fetching presets from musical-artifacts.com",
            muted_style(),
        )));
    } else if app.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "No downloadable .gx files found.",
            muted_style(),
        )));
    } else {
        let max_rows = inner_h.max(1).min(app.items.len());
        let top = if app.selected >= max_rows {
            app.selected - max_rows + 1
        } else {
            0
        };
        for i in top..(top + max_rows).min(app.items.len()) {
            let item = &app.items[i];
            let meta = non_empty_join(
                &[&item.author, &item.size, &downloads_label(&item.downloads)],
                " | ",
            );
            let name_w = area.width.saturating_sub(35).max(18) as usize;
            let line = format!(
                "{:2}. {:width$} {}",
                i + 1,
                truncate(&item.name, name_w),
                truncate(&meta, 28),
                width = name_w
            );
            lines.push(Line::from(Span::styled(
                line,
                if i == app.selected {
                    selected_style()
                } else if focused {
                    item_style()
                } else {
                    muted_style()
                },
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Presets", focused)),
        area,
    );
}

fn render_artifact_detail(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if app.loading {
        lines.push(Line::from(Span::styled("loading", active_badge_style())));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Network request in progress.",
            muted_style(),
        )));
    } else if !app.err.is_empty() {
        lines.push(Line::from(Span::styled(app.err.clone(), error_style())));
    } else if let Some(item) = app.items.get(app.selected) {
        let width = area.width.saturating_sub(4) as usize;
        lines.push(Line::from(Span::styled(
            truncate(&item.name, width),
            accent_style().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for (k, v) in [
            ("Author", &item.author),
            ("File", &item.filename),
            ("Size", &item.size),
            ("Downloads", &item.downloads),
            ("Page", &item.page_url),
        ] {
            if !v.is_empty() {
                lines.push(label_line(k, v, width));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Description", muted_style())));
        let desc = if item.description.is_empty() {
            "(No description available)"
        } else {
            &item.description
        };
        for line in wrap_text(desc, width, 8) {
            lines.push(Line::from(line));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No preset selected.",
            muted_style(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel_block("Details", false)),
        area,
    );
}

fn render_audio(frame: &mut Frame, app: &App, area: Rect) {
    let conn_h = if area.height >= 22 {
        11
    } else {
        area.height.saturating_sub(8).clamp(6, 10)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(conn_h)])
        .split(area);
    render_meter(frame, app, chunks[0]);
    render_connections(frame, app, chunks[1]);
}

fn render_connections(frame: &mut Frame, app: &App, area: Rect) {
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    frame.render_widget(
        Block::default()
            .title(" Connections ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(
                if app.audio.focus == AudioFocus::Connections || app.audio.picking_target {
                    accent_style()
                } else {
                    border_style()
                },
            ),
        area,
    );
    let top_h = inner.height.min(7);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(top_h), Constraint::Min(2)])
        .split(inner);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);
    render_audio_nodes(
        frame,
        "Sources",
        &app.audio.outputs,
        app.audio.out_selected,
        top[0],
        app.audio.focus == AudioFocus::Connections && !app.audio.picking_target,
        &app.audio,
    );
    if app.audio.picking_target {
        render_targets(frame, app, top[1]);
    } else {
        render_selected_source(frame, app, top[1]);
    }
    let routes = app
        .audio
        .outputs
        .iter()
        .filter_map(|out| {
            let targets = app.linked_targets(out);
            (!targets.is_empty()).then(|| {
                Line::from(format!(
                    "{} -> {}",
                    truncate(&out.name, 34),
                    truncate(&targets.join(", "), chunks[1].width as usize / 2)
                ))
            })
        })
        .collect::<Vec<_>>();
    let lines = if routes.is_empty() {
        vec![Line::from(Span::styled(
            "No audio routes connected.",
            muted_style(),
        ))]
    } else {
        routes
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().title("Routes")),
        chunks[1],
    );
}

fn render_audio_nodes(
    frame: &mut Frame,
    title: &str,
    nodes: &[AudioNode],
    selected: usize,
    area: Rect,
    focused: bool,
    audio: &AudioState,
) {
    let mut lines = vec![Line::from(Span::styled(title.to_string(), accent_style()))];
    if audio.loading && nodes.is_empty() {
        lines.push(Line::from(Span::styled(" loading ", active_badge_style())));
    } else if !audio.err.is_empty() && nodes.is_empty() {
        lines.push(Line::from(Span::styled(audio.err.clone(), error_style())));
    } else if nodes.is_empty() {
        lines.push(Line::from(Span::styled("No nodes found.", muted_style())));
    } else {
        let max_rows = area.height.saturating_sub(1) as usize;
        let top = if selected >= max_rows {
            selected - max_rows + 1
        } else {
            0
        };
        for i in top..(top + max_rows).min(nodes.len()) {
            let node = &nodes[i];
            let tag = if node.ports.len() == 1 {
                "1 port".to_string()
            } else {
                format!("{} ports", node.ports.len())
            };
            let width = area.width.saturating_sub(15).max(8) as usize;
            let line = format!(
                "{:2}. {:width$} {}",
                i + 1,
                truncate(&node.name, width),
                tag,
                width = width
            );
            lines.push(Line::from(Span::styled(
                line,
                if i == selected {
                    selected_style()
                } else if focused {
                    item_style()
                } else {
                    muted_style()
                },
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_targets(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![Line::from(Span::styled("Choose target", accent_style()))];
    let out = app.selected_output();
    let max_rows = area.height.saturating_sub(2) as usize;
    let top = if app.audio.in_selected >= max_rows {
        app.audio.in_selected - max_rows + 1
    } else {
        0
    };
    for i in top..(top + max_rows).min(app.audio.inputs.len()) {
        let node = &app.audio.inputs[i];
        let mark = if !out.name.is_empty() && app.nodes_connected(&out, node) {
            "[x]"
        } else {
            "[ ]"
        };
        let width = area.width.saturating_sub(18).max(8) as usize;
        let line = format!(
            "{} {:2}. {:width$} {}",
            mark,
            i + 1,
            truncate(&node.name, width),
            node.ports.len(),
            width = width
        );
        lines.push(Line::from(Span::styled(
            line,
            if i == app.audio.in_selected {
                selected_style()
            } else {
                item_style()
            },
        )));
    }
    lines.push(Line::from(Span::styled(
        "space toggle  enter/c connect  x disconnect",
        muted_style(),
    )));
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_selected_source(frame: &mut Frame, app: &App, area: Rect) {
    let out = app.selected_output();
    let mut lines = vec![Line::from(Span::styled("Selected source", accent_style()))];
    if out.name.is_empty() {
        lines.push(Line::from(Span::styled("Select a source.", muted_style())));
    } else {
        lines.push(label_line("Source", &out.name, area.width as usize));
        let targets = app.linked_targets(&out);
        if targets.is_empty() {
            lines.push(Line::from(Span::styled(
                "No target connected.",
                muted_style(),
            )));
        } else {
            lines.push(Line::from(Span::styled("Targets", muted_style())));
            for target in targets.iter().take(area.height.saturating_sub(4) as usize) {
                lines.push(Line::from(format!(
                    "  -> {}",
                    truncate(target, area.width.saturating_sub(6) as usize)
                )));
            }
        }
        lines.push(Line::from(Span::styled(
            "enter opens target picker",
            muted_style(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_meter(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Block::default()
            .title(" Visualizer ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if app.audio.focus == AudioFocus::Meter {
                accent_style()
            } else {
                border_style()
            }),
        area,
    );
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.width >= 84 {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(42), Constraint::Min(20)])
            .split(inner);
        render_audio_nodes(
            frame,
            "Listen source",
            &app.audio.outputs,
            app.audio.meter_selected,
            chunks[0],
            app.audio.focus == AudioFocus::Meter,
            &app.audio,
        );
        render_volume_visualizer(frame, app, chunks[1]);
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(4)])
            .split(inner);
        render_audio_nodes(
            frame,
            "Listen source",
            &app.audio.outputs,
            app.audio.meter_selected,
            chunks[0],
            app.audio.focus == AudioFocus::Meter,
            &app.audio,
        );
        render_volume_visualizer(frame, app, chunks[1]);
    }
}

fn render_volume_visualizer(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    let source = if app.audio.meter_source.is_empty() {
        app.selected_meter_source_name()
    } else {
        app.audio.meter_source.clone()
    };
    lines.push(label_line("Meter", &source, area.width as usize));
    if !app.audio.meter_target.is_empty() && app.audio.meter_target != source {
        lines.push(Line::from(Span::styled(
            format!(
                "capture: {}",
                truncate(&app.audio.meter_target, area.width as usize)
            ),
            muted_style(),
        )));
    }
    if let Some(recording) = &app.recording {
        lines.push(Line::from(vec![
            Span::styled("record: ", muted_style()),
            Span::styled(
                format!(
                    "REC {}  {}",
                    format_elapsed(recording.started.elapsed()),
                    truncate(
                        recording
                            .path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("recording.wav"),
                        area.width.saturating_sub(16) as usize
                    )
                ),
                error_style(),
            ),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("record: ", muted_style()),
            Span::styled("off", muted_style()),
            Span::raw("  "),
            Span::styled("R start", accent_style()),
        ]));
    }
    if !app.audio.meter_err.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "meter: {}",
                truncate(&app.audio.meter_err, area.width as usize)
            ),
            muted_style(),
        )));
    }
    let wave_h = area.height.saturating_sub(lines.len() as u16).max(1) as usize;
    lines.extend(volume_wave_lines(
        &app.audio.volume_history,
        area.width as usize,
        wave_h,
    ));
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_pedals(frame: &mut Frame, app: &App, area: Rect) {
    if app.mode == Mode::NewPedalGroup || app.mode == Mode::RenamePedalGroup {
        let title = if app.mode == Mode::NewPedalGroup {
            "New Pedal Group"
        } else {
            "Rename Pedal Group"
        };
        let body = vec![
            Line::from("Enter a group name. Esc cancels."),
            Line::from(""),
            Line::from(Span::styled(format!(" {}_ ", app.input), selected_style())),
        ];
        frame.render_widget(Paragraph::new(body).block(panel_block(title, true)), area);
        return;
    }
    if app.pedals.picking_preset {
        render_pedal_picker(frame, app, area);
        return;
    }
    if area.width >= 104 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(7)])
            .split(area);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
            .split(rows[0]);
        render_pedal_groups(frame, app, cols[0]);
        render_pedal_presets(frame, app, cols[1]);
        render_pedal_detail(frame, app, rows[1]);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(40),
                Constraint::Min(6),
            ])
            .split(area);
        render_pedal_groups(frame, app, rows[0]);
        render_pedal_presets(frame, app, rows[1]);
        render_pedal_detail(frame, app, rows[2]);
    }
}

fn render_pedal_groups(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if app.pedals.groups.is_empty() {
        lines.push(Line::from(Span::styled("No pedal groups.", muted_style())));
        lines.push(Line::from(Span::styled("n creates one.", muted_style())));
    } else {
        let max_rows = area.height.saturating_sub(2) as usize;
        let top = if app.pedals.selected >= max_rows {
            app.pedals.selected - max_rows + 1
        } else {
            0
        };
        for i in top..(top + max_rows).min(app.pedals.groups.len()) {
            let group = &app.pedals.groups[i];
            let count = if group.presets.len() == 1 {
                "1 preset".to_string()
            } else {
                format!("{} presets", group.presets.len())
            };
            let width = area.width.saturating_sub(17).max(8) as usize;
            let line = format!(
                "{:2}. {:width$} {}",
                i + 1,
                truncate(&group.name, width),
                count,
                width = width
            );
            lines.push(Line::from(Span::styled(
                line,
                if i == app.pedals.selected {
                    selected_style()
                } else if app.pedals.focus == PedalFocus::Groups {
                    item_style()
                } else {
                    muted_style()
                },
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(
            "Groups",
            app.pedals.focus == PedalFocus::Groups,
        )),
        area,
    );
}

fn render_pedal_presets(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if let Some(group) = app.pedals.groups.get(app.pedals.selected) {
        if group.presets.is_empty() {
            lines.push(Line::from(Span::styled(
                "No presets in this group.",
                muted_style(),
            )));
            lines.push(Line::from(Span::styled(
                "a opens the preset picker.",
                muted_style(),
            )));
        } else {
            let selected = group.current.min(group.presets.len().saturating_sub(1));
            let max_rows = area.height.saturating_sub(2) as usize;
            let top = if selected >= max_rows {
                selected - max_rows + 1
            } else {
                0
            };
            for i in top..(top + max_rows).min(group.presets.len()) {
                let preset = &group.presets[i];
                let mark = if i == selected { ">" } else { " " };
                let width = area.width.saturating_sub(12).max(8) as usize;
                let text = format!("{} {} / {}", mark, preset.bank, preset.preset);
                lines.push(Line::from(Span::styled(
                    format!("{:2}. {}", i + 1, truncate(&text, width)),
                    if i == selected {
                        selected_style()
                    } else if app.pedals.focus == PedalFocus::Presets {
                        item_style()
                    } else {
                        muted_style()
                    },
                )));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "Create a group first.",
            muted_style(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(
            "Group Presets",
            app.pedals.focus == PedalFocus::Presets,
        )),
        area,
    );
}

fn render_pedal_detail(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if let Some(group) = app.pedals.groups.get(app.pedals.selected) {
        lines.push(label_line("Group", &group.name, area.width as usize));
        lines.push(label_line(
            "Guitarix bank",
            &pedal_group_bank_name(group),
            area.width as usize,
        ));
        if let Some(preset) = group
            .presets
            .get(group.current.min(group.presets.len().saturating_sub(1)))
        {
            lines.push(label_line(
                "Current",
                &format!("{} / {}", preset.bank, preset.preset),
                area.width as usize,
            ));
            lines.push(label_line(
                "Position",
                &format!(
                    "{}/{}",
                    group.current.min(group.presets.len().saturating_sub(1)) + 1,
                    group.presets.len()
                ),
                area.width as usize,
            ));
        } else {
            lines.push(label_line("Current", "empty", area.width as usize));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No group selected.",
            muted_style(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "n new  e rename  a add preset  enter/s load  , previous  ; next  x delete",
        muted_style(),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel_block("Pedal Control", false)),
        area,
    );
}

fn render_pedal_picker(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(3)])
        .split(area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(rows[0]);
    render_string_list(
        frame,
        "Banks",
        &app.guitarix.banks,
        app.pedals.picker_bank_selected,
        cols[0],
        app.pedals.picker_focus == 0,
        app.guitarix.loading && app.guitarix.banks.is_empty(),
        &app.guitarix.err,
    );
    render_string_list(
        frame,
        "Presets",
        &app.pedals.picker_presets,
        app.pedals.picker_preset_selected,
        cols[1],
        app.pedals.picker_focus == 1,
        app.pedals.picker_loading,
        &app.pedals.picker_err,
    );
    let target = app
        .pedals
        .groups
        .get(app.pedals.selected)
        .map(|group| group.name.clone())
        .unwrap_or_else(|| "no group".to_string());
    let lines = vec![
        label_line("Add to", &target, rows[1].width as usize),
        Line::from(Span::styled(
            "h/l focus  up/down select  enter/a add  esc cancel",
            muted_style(),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Preset Picker", true)),
        rows[1],
    );
}

fn render_recordings(frame: &mut Frame, app: &App, area: Rect) {
    if area.width >= 96 {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);
        render_recordings_list(frame, app, chunks[0]);
        render_recording_detail(frame, app, chunks[1]);
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Min(6)])
            .split(area);
        render_recordings_list(frame, app, chunks[0]);
        render_recording_detail(frame, app, chunks[1]);
    }
}

fn render_recordings_list(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if app.recordings.loading && app.recordings.items.is_empty() {
        lines.push(Line::from(Span::styled(" loading ", active_badge_style())));
        lines.push(Line::from(Span::styled(
            recordings_dir().display().to_string(),
            muted_style(),
        )));
    } else if !app.recordings.err.is_empty() && app.recordings.items.is_empty() {
        lines.push(Line::from(Span::styled(
            app.recordings.err.clone(),
            error_style(),
        )));
    } else if app.recordings.items.is_empty() {
        lines.push(Line::from(Span::styled("No recordings.", muted_style())));
        lines.push(Line::from(Span::styled(
            "Use R in the Audio tab to record the selected meter source.",
            muted_style(),
        )));
    } else {
        let max_rows = area.height.saturating_sub(2) as usize;
        let top = if app.recordings.selected >= max_rows {
            app.recordings.selected - max_rows + 1
        } else {
            0
        };
        for i in top..(top + max_rows).min(app.recordings.items.len()) {
            let item = &app.recordings.items[i];
            let size = human_size(item.size);
            let width = area.width.saturating_sub(18).max(8) as usize;
            let line = format!(
                "{:2}. {:width$} {}",
                i + 1,
                truncate(&item.name, width),
                size,
                width = width
            );
            lines.push(Line::from(Span::styled(
                line,
                if i == app.recordings.selected {
                    selected_style()
                } else {
                    item_style()
                },
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Recordings", true)),
        area,
    );
}

fn render_recording_detail(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if app.mode == Mode::RenameRecording {
        lines.push(Line::from(Span::styled("Rename", accent_style())));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {}_ ", app.input),
            selected_style(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "enter save  esc cancel",
            muted_style(),
        )));
    } else if let Some(item) = app.selected_recording() {
        lines.push(label_line("File", &item.name, area.width as usize));
        lines.push(label_line(
            "Size",
            &human_size(item.size),
            area.width as usize,
        ));
        lines.push(label_line(
            "Path",
            &item.path.display().to_string(),
            area.width as usize,
        ));
        if let Some(playback) = &app.playback {
            let status = if playback.path == item.path {
                "playing selected"
            } else {
                "playing another file"
            };
            lines.push(label_line("Playback", status, area.width as usize));
        } else {
            lines.push(label_line("Playback", "stopped", area.width as usize));
        }
        if let Some(recording) = &app.recording {
            lines.push(label_line(
                "Recording",
                &format!(
                    "{} {}",
                    format_elapsed(recording.started.elapsed()),
                    recording.path.display()
                ),
                area.width as usize,
            ));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "enter/p play  s stop  e rename  x delete",
            muted_style(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "No recording selected.",
            muted_style(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel_block("Details", false)),
        area,
    );
}

fn render_guitarix(frame: &mut Frame, app: &App, area: Rect) {
    if area.width >= 104 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(7)])
            .split(area);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(rows[0]);
        render_string_list(
            frame,
            "Banks",
            &app.guitarix.banks,
            app.guitarix.bank_selected,
            cols[0],
            app.guitarix.focus == 0,
            app.guitarix.loading,
            &app.guitarix.err,
        );
        render_guitarix_presets(frame, cols[1], app.guitarix.focus == 1, app);
        render_guitarix_detail(frame, app, rows[1]);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(35),
                Constraint::Percentage(35),
                Constraint::Min(6),
            ])
            .split(area);
        render_string_list(
            frame,
            "Banks",
            &app.guitarix.banks,
            app.guitarix.bank_selected,
            rows[0],
            app.guitarix.focus == 0,
            app.guitarix.loading,
            &app.guitarix.err,
        );
        render_guitarix_presets(frame, rows[1], app.guitarix.focus == 1, app);
        render_guitarix_detail(frame, app, rows[2]);
    }
}

fn render_guitarix_presets(frame: &mut Frame, area: Rect, focused: bool, app: &App) {
    if guitarix_presets_loading(app) {
        let lines = vec![
            Line::from(Span::styled(" Loading the presets ", active_badge_style())),
            Line::from(Span::styled(
                truncate(&app.selected_bank(), area.width.saturating_sub(2) as usize),
                muted_style(),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(panel_block("Presets", focused)),
            area,
        );
        return;
    }
    render_string_list(
        frame,
        "Presets",
        &app.guitarix.presets,
        app.guitarix.preset_selected,
        area,
        focused,
        app.guitarix.loading,
        &app.guitarix.err,
    );
}

fn guitarix_presets_loading(app: &App) -> bool {
    let selected_bank = app.selected_bank();
    (app.guitarix.loading
        && (app.guitarix.loading_bank.is_empty() || app.guitarix.loading_bank == selected_bank))
        || (!selected_bank.is_empty() && app.guitarix.presets_bank != selected_bank)
}

fn render_string_list(
    frame: &mut Frame,
    title: &str,
    items: &[String],
    selected: usize,
    area: Rect,
    focused: bool,
    loading: bool,
    err: &str,
) {
    let mut lines = Vec::new();
    if loading && items.is_empty() {
        lines.push(Line::from(Span::styled(" loading ", active_badge_style())));
        lines.push(Line::from(Span::styled(
            "Querying 127.0.0.1:7000",
            muted_style(),
        )));
    } else if !err.is_empty() && items.is_empty() {
        lines.push(Line::from(Span::styled(err.to_string(), error_style())));
    } else if items.is_empty() {
        lines.push(Line::from(Span::styled("No entries.", muted_style())));
    } else {
        let max_rows = area.height.saturating_sub(2) as usize;
        let top = if selected >= max_rows {
            selected - max_rows + 1
        } else {
            0
        };
        for i in top..(top + max_rows).min(items.len()) {
            let line = format!(
                "{:2}. {}",
                i + 1,
                truncate(&items[i], area.width.saturating_sub(8) as usize)
            );
            lines.push(Line::from(Span::styled(
                line,
                if i == selected {
                    selected_style()
                } else if focused {
                    item_style()
                } else {
                    muted_style()
                },
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(title, focused)),
        area,
    );
}

fn render_guitarix_detail(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if !app.guitarix.err.is_empty() {
        lines.push(Line::from(Span::styled(
            app.guitarix.err.clone(),
            error_style(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Auto-start command: PIPEWIRE_LATENCY=128/48000 pw-jack guitarix -N -p 7000",
            muted_style(),
        )));
    } else if app.guitarix.confirm_delete {
        lines.push(Line::from(Span::styled("Delete bank?", error_style())));
        lines.push(Line::from(format!("Bank: {}", app.selected_bank())));
        lines.push(Line::from(Span::styled(
            "y/enter delete  n/esc cancel",
            muted_style(),
        )));
    } else {
        lines.push(label_line("RPC", "127.0.0.1:7000", area.width as usize));
        lines.push(label_line(
            "Bank",
            &app.selected_bank(),
            area.width as usize,
        ));
        lines.push(label_line(
            "Preset",
            &app.selected_preset(),
            area.width as usize,
        ));
        if !app.guitarix.current_preset.is_empty() {
            lines.push(label_line(
                "Loaded",
                &format!(
                    "{} / {}",
                    app.guitarix.current_bank, app.guitarix.current_preset
                ),
                area.width as usize,
            ));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Guitarix auto-starts if RPC is not reachable. enter/s loads the selected preset.",
            muted_style(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Guitarix RPC", false)),
        area,
    );
}

fn spawn_fetch(app: &App) {
    let ctx = app.clone_for_thread();
    thread::spawn(move || {
        let result = fetch_artifacts(&ctx.client, &ctx.query, &ctx.order, ctx.page)
            .map_err(|err| err.to_string());
        let _ = ctx.tx.send(AppEvent::Fetch(result));
    });
}

fn spawn_fetch_for_crawl(ctx: ThreadCtx) {
    thread::spawn(
        move || match fetch_artifacts(&ctx.client, &ctx.query, &ctx.order, ctx.page) {
            Ok(result) => {
                for item in result.items {
                    let _ = ctx.tx.send(AppEvent::Download(DownloadEvent::Queued {
                        artifact: item.clone(),
                    }));
                    let tx = ctx.tx.clone();
                    let client = ctx.client.clone();
                    let dest = ctx.dest.clone();
                    let force = ctx.force;
                    thread::spawn(move || {
                        let _ = tx.send(AppEvent::Download(DownloadEvent::Started));
                        match download_artifact(&client, &dest, force, item.clone()) {
                            Ok((path, true)) => {
                                let _ = tx.send(AppEvent::Download(DownloadEvent::Exists {
                                    artifact: item,
                                    path,
                                }));
                            }
                            Ok((path, false)) => {
                                let _ = tx.send(AppEvent::Download(DownloadEvent::Saved {
                                    artifact: item,
                                    path,
                                }));
                            }
                            Err(err) => {
                                let _ = tx.send(AppEvent::Download(DownloadEvent::Failed {
                                    artifact: item,
                                    err: err.to_string(),
                                }));
                            }
                        }
                    });
                }
            }
            Err(err) => {
                let artifact = Artifact {
                    name: format!("crawl page {}", ctx.page),
                    ..Artifact::default()
                };
                let _ = ctx.tx.send(AppEvent::Download(DownloadEvent::Failed {
                    artifact,
                    err: err.to_string(),
                }));
            }
        },
    );
}

fn spawn_audio_refresh(app: &App) {
    let tx = app.tx.clone();
    thread::spawn(move || {
        let result = audio_snapshot().map_err(|err| err.to_string());
        let _ = tx.send(AppEvent::Audio(result));
    });
}

fn spawn_audio_action(app: &App, out: AudioNode, input: AudioNode, disconnect: bool) {
    let tx = app.tx.clone();
    thread::spawn(move || {
        let action = if disconnect { "disconnect" } else { "connect" }.to_string();
        let result = run_audio_action(&out, &input, disconnect).map_err(|err| err.to_string());
        let _ = tx.send(AppEvent::AudioAction { action, result });
    });
}

fn spawn_recordings_refresh(app: &App) {
    let tx = app.tx.clone();
    thread::spawn(move || {
        let result = list_recordings().map_err(|err| err.to_string());
        let _ = tx.send(AppEvent::Recordings(result));
    });
}

fn spawn_guitarix_refresh(app: &App, preferred_bank: String) {
    let tx = app.tx.clone();
    thread::spawn(move || {
        let result = guitarix_snapshot(&preferred_bank).map_err(|err| err.to_string());
        let _ = tx.send(AppEvent::Guitarix(result));
    });
}

fn spawn_guitarix_bank_reparse(app: &App, preferred_bank: String) {
    let tx = app.tx.clone();
    thread::spawn(move || {
        let _ = guitarix_bank_check_reparse();
        let result = guitarix_snapshot(&preferred_bank).map_err(|err| err.to_string());
        let _ = tx.send(AppEvent::Guitarix(result));
    });
}

fn spawn_pedal_picker_presets(app: &App, bank: String) {
    let tx = app.tx.clone();
    thread::spawn(move || {
        let result = guitarix_presets(&bank).map_err(|err| err.to_string());
        let _ = tx.send(AppEvent::PedalPickerPresets { bank, result });
    });
}

fn request_guitarix_refresh(app: &mut App, bank: String) {
    app.guitarix.loading = true;
    app.guitarix.loading_bank = bank.clone();
    app.guitarix.presets.clear();
    app.guitarix.presets_bank.clear();
    app.guitarix.preset_selected = 0;
    spawn_guitarix_refresh(app, bank);
}

fn spawn_set_preset(app: &App, bank: String, preset: String) {
    let tx = app.tx.clone();
    thread::spawn(move || {
        let result = guitarix_set_preset(&bank, &preset).map_err(|err| err.to_string());
        let _ = tx.send(AppEvent::Preset {
            bank,
            preset,
            result,
        });
    });
}

fn spawn_delete_bank(app: &App, bank: String) {
    let tx = app.tx.clone();
    let dir = app.dest.clone();
    thread::spawn(move || {
        let (path, warn, result) = match delete_guitarix_bank(&bank, &dir) {
            Ok((path, warn)) => (path, warn, Ok(())),
            Err(err) => (String::new(), String::new(), Err(err.to_string())),
        };
        let _ = tx.send(AppEvent::BankDelete {
            bank,
            path,
            warn,
            result,
        });
    });
}

fn enqueue_download(app: &mut App, item: Artifact) {
    let key = first_non_empty(&[&item.download_url, &item.filename]).to_string();
    if key.is_empty() {
        return;
    }
    {
        let mut seen = app.download_seen.lock().unwrap();
        if !seen.insert(key) {
            let _ = app.tx.send(AppEvent::Download(DownloadEvent::Duplicate {
                artifact: item,
            }));
            return;
        }
    }
    let _ = app.tx.send(AppEvent::Download(DownloadEvent::Queued {
        artifact: item.clone(),
    }));
    let tx = app.tx.clone();
    let client = app.client.clone();
    let dest = app.dest.clone();
    let force = app.force;
    thread::spawn(move || {
        let _ = tx.send(AppEvent::Download(DownloadEvent::Started));
        match download_artifact(&client, &dest, force, item.clone()) {
            Ok((path, true)) => {
                let _ = tx.send(AppEvent::Download(DownloadEvent::Exists {
                    artifact: item,
                    path,
                }));
            }
            Ok((path, false)) => {
                let _ = tx.send(AppEvent::Download(DownloadEvent::Saved {
                    artifact: item,
                    path,
                }));
            }
            Err(err) => {
                let _ = tx.send(AppEvent::Download(DownloadEvent::Failed {
                    artifact: item,
                    err: err.to_string(),
                }));
            }
        }
    });
}

fn ensure_meter_stream(app: &mut App) -> bool {
    if app.active_tab != Tab::Audio {
        app.stop_meter();
        return false;
    }
    let node = app.selected_meter_source();
    let source = node.name.clone();
    if source.is_empty() || is_midi_name(&source) {
        app.stop_meter();
        app.audio.meter_source.clear();
        app.audio.meter_target.clear();
        return true;
    }
    let target = meter_target_for_node(&node);
    if app.config.last_meter_source != source {
        app.config.last_meter_source = source.clone();
        let _ = save_app_config(&app.config);
    }
    if app
        .meter
        .as_ref()
        .is_some_and(|m| m.source == source && m.target == target)
    {
        return false;
    }
    app.stop_meter();
    app.meter_seq += 1;
    let id = app.meter_seq;
    let cancel = Arc::new(AtomicBool::new(false));
    app.meter = Some(MeterControl {
        id,
        source: source.clone(),
        target: target.clone(),
        cancel: cancel.clone(),
    });
    app.audio.meter_source = source.clone();
    app.audio.meter_target = target.clone();
    app.audio.meter_err.clear();
    app.audio.volume_level = 0.0;
    app.audio.volume_history.clear();
    let tx = app.tx.clone();
    thread::spawn(move || run_meter_stream(id, source, target, cancel, tx));
    true
}

fn fetch_artifacts(client: &Client, query: &str, order: &str, page: usize) -> Result<FetchResult> {
    let raw_url = search_url(query, order, page)?;
    let body = client.get(&raw_url).send()?.error_for_status()?.text()?;
    Ok(FetchResult {
        items: parse_artifacts(&body),
        raw_url,
        query: query.to_string(),
        order: order.to_string(),
        page,
    })
}

fn search_url(query: &str, order: &str, page: usize) -> Result<String> {
    let mut url = Url::parse(&format!("{}/artifacts", BASE_URL))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("apps", "guitarix");
        if !query.trim().is_empty() {
            pairs.append_pair("q", query.trim());
        }
        if !order.is_empty() {
            pairs.append_pair("order", order);
        }
        if page > 1 {
            pairs.append_pair("page", &page.to_string());
        }
    }
    Ok(url.to_string())
}

fn parse_artifacts(body: &str) -> Vec<Artifact> {
    let mut items = Vec::new();
    let needle = "artifact-item col-sm-12";
    let mut start = 0;
    while let Some(rel) = body[start..].find(needle) {
        let i = start + rel;
        let next = body[i + needle.len()..].find(needle);
        let end = next.map(|n| i + needle.len() + n).unwrap_or(body.len());
        if let Some(item) = parse_artifact_block(&body[i..end]) {
            items.push(item);
        }
        start = end;
    }
    items
}

fn parse_artifact_block(block: &str) -> Option<Artifact> {
    let btn = block.find("btn-download")?;
    let tag_start = block[..btn].rfind("<a ")?;
    let tag_end = btn + block[btn..].find('>')?;
    let tag = &block[tag_start..=tag_end];
    let download_url = absolutize(&get_attr(tag, "href"));
    let filename = clean_filename(&first_non_empty(&[
        &get_attr(tag, "download"),
        &get_attr(tag, "title"),
        &path_base_from_url(&download_url),
    ]));
    if download_url.is_empty() || filename.is_empty() || !filename.to_lowercase().ends_with(".gx") {
        return None;
    }
    let mut a = Artifact {
        download_url,
        filename,
        ..Artifact::default()
    };
    let name_re =
        Regex::new(r#"(?s)<h2 class='artifact-name'>\s*<a href="/artifacts/([0-9]+)">([^<]+)</a>"#)
            .ok()?;
    if let Some(caps) = name_re.captures(block) {
        a.id = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        a.name = clean_text(caps.get(2).map(|m| m.as_str()).unwrap_or(""));
    }
    if a.id.is_empty() {
        let id_re = Regex::new(r#"/artifacts/([0-9]+)(?:/|$)"#).ok()?;
        if let Some(caps) = id_re.captures(&a.download_url) {
            a.id = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        }
    }
    if a.name.is_empty() {
        a.name = a.filename.trim_end_matches(".gx").to_string();
    }
    if !a.id.is_empty() {
        a.page_url = format!("{}/artifacts/{}", BASE_URL, a.id);
    }
    if let Ok(re) = Regex::new(r#"(?s)\bby\s*<a[^>]*>([^<]+)</a>"#) {
        if let Some(caps) = re.captures(block) {
            a.author = clean_text(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
        }
    }
    if let Ok(re) = Regex::new(r#"(?s)<div class='artifact-description'>\s*(.*?)\s*</div>"#) {
        if let Some(caps) = re.captures(block) {
            a.description = clean_text(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
        }
    }
    if let Some(anchor_end) = block[tag_end + 1..].find("</a>") {
        let inner = &block[tag_end + 1..tag_end + 1 + anchor_end];
        let txt = clean_text(inner);
        if let Ok(size_re) = Regex::new(r#"\(([^)]+)\)"#) {
            if let Some(caps) = size_re.captures(&txt) {
                a.size = clean_text(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
            }
        }
        if let Ok(count_re) = Regex::new(r#"^[0-9][0-9,.\s]*"#) {
            if let Some(m) = count_re.find(&txt) {
                a.downloads = m.as_str().trim().to_string();
            }
        }
    }
    Some(a)
}

fn download_artifact(
    client: &Client,
    dest: &Path,
    force: bool,
    mut item: Artifact,
) -> Result<(String, bool)> {
    if item.download_url.is_empty() {
        return Err(anyhow::anyhow!("missing download URL"));
    }
    if item.filename.is_empty() {
        item.filename = clean_filename(&path_base_from_url(&item.download_url));
    }
    if item.filename.is_empty() || !item.filename.to_lowercase().ends_with(".gx") {
        return Err(anyhow::anyhow!("not a .gx filename: {}", item.filename));
    }
    fs::create_dir_all(dest)?;
    let out = dest.join(&item.filename);
    if !force && out.exists() {
        return Ok((out.display().to_string(), true));
    }
    let mut resp = client.get(&item.download_url).send()?.error_for_status()?;
    let tmp = out.with_extension("gx.tmp");
    let mut file = fs::File::create(&tmp)?;
    io::copy(&mut resp, &mut file)?;
    fs::rename(&tmp, &out)?;
    Ok((out.display().to_string(), false))
}

fn audio_snapshot() -> Result<AudioSnapshot> {
    let output_ports = pipewire_ports("-o")?;
    let input_ports = pipewire_ports("-i")?;
    let links = pipewire_links().unwrap_or_default();
    Ok(AudioSnapshot {
        outputs: group_ports(output_ports),
        inputs: group_ports(input_ports),
        links,
    })
}

fn pipewire_ports(direction: &str) -> Result<Vec<String>> {
    let out = command_output("pw-link", &[direction])?;
    let mut lines = split_clean_lines(&out)
        .into_iter()
        .filter(|line| !is_midi_name(line))
        .collect::<Vec<_>>();
    lines.sort();
    Ok(lines)
}

fn pipewire_links() -> Result<HashMap<String, HashSet<String>>> {
    let out = command_output("pw-link", &["-l"])?;
    let mut links: HashMap<String, HashSet<String>> = HashMap::new();
    let mut current = String::new();
    for raw in out.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(index) = line.find("|->") {
            if !current.is_empty() {
                let target = line[index + 3..].trim();
                if !target.is_empty() {
                    links
                        .entry(current.clone())
                        .or_default()
                        .insert(target.to_string());
                }
            }
        } else {
            current = line.to_string();
        }
    }
    Ok(links)
}

fn run_audio_action(out: &AudioNode, input: &AudioNode, disconnect: bool) -> Result<()> {
    let pairs = pair_ports(&out.ports, &input.ports);
    if pairs.is_empty() {
        return Err(anyhow::anyhow!("no compatible ports"));
    }
    let mut errors = Vec::new();
    for (out_port, in_port) in pairs {
        let args = if disconnect {
            vec!["-d".to_string(), out_port, in_port]
        } else {
            vec![out_port, in_port]
        };
        let ref_args = args.iter().map(String::as_str).collect::<Vec<_>>();
        if let Err(err) = command_output("pw-link", &ref_args) {
            errors.push(err.to_string());
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(errors.join("; ")))
    }
}

fn group_ports(ports: Vec<String>) -> Vec<AudioNode> {
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for port in ports {
        grouped.entry(node_name(&port)).or_default().push(port);
    }
    let mut nodes = grouped
        .into_iter()
        .map(|(name, mut ports)| {
            ports.sort();
            AudioNode { name, ports }
        })
        .collect::<Vec<_>>();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    nodes
}

fn node_name(port: &str) -> String {
    port.rsplit_once(':')
        .map(|(node, _)| node.to_string())
        .unwrap_or_else(|| port.to_string())
}

fn meter_target_for_node(node: &AudioNode) -> String {
    let mut preferred = String::new();
    for port in &node.ports {
        if !is_monitor_port(port) {
            continue;
        }
        if preferred.is_empty() {
            preferred = port.clone();
        }
        if channel_key(port) == "FL" || port.to_lowercase().ends_with("monitor_fl") {
            return port.clone();
        }
    }
    if preferred.is_empty() {
        node.name.clone()
    } else {
        preferred
    }
}

fn is_monitor_port(port: &str) -> bool {
    port.rsplit_once(':')
        .map(|(_, p)| p)
        .unwrap_or(port)
        .to_lowercase()
        .contains("monitor")
}

fn pair_ports(outputs: &[String], inputs: &[String]) -> Vec<(String, String)> {
    if outputs.is_empty() || inputs.is_empty() {
        return Vec::new();
    }
    let mut in_by_chan = HashMap::new();
    for input in inputs {
        let ch = channel_key(input);
        if !ch.is_empty() {
            in_by_chan.insert(ch, input.clone());
        }
    }
    let mut pairs = Vec::new();
    let mut used_in = HashSet::new();
    for out in outputs {
        let ch = channel_key(out);
        if ch.is_empty() {
            continue;
        }
        if let Some(input) = in_by_chan.get(&ch) {
            pairs.push((out.clone(), input.clone()));
            used_in.insert(input.clone());
        }
    }
    if !pairs.is_empty() {
        return pairs;
    }
    if outputs.len() == 1 {
        return inputs
            .iter()
            .map(|input| (outputs[0].clone(), input.clone()))
            .collect();
    }
    if inputs.len() == 1 {
        return outputs
            .iter()
            .map(|out| (out.clone(), inputs[0].clone()))
            .collect();
    }
    for (out, input) in outputs.iter().zip(inputs.iter()) {
        if !used_in.contains(input) {
            pairs.push((out.clone(), input.clone()));
        }
    }
    pairs
}

fn channel_key(port: &str) -> String {
    let name = port
        .rsplit_once(':')
        .map(|(_, p)| p)
        .unwrap_or(port)
        .to_uppercase();
    for ch in ["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR", "MONO"] {
        if name == ch || name.ends_with(&format!("_{}", ch)) || name.ends_with(&format!("-{}", ch))
        {
            return ch.to_string();
        }
    }
    String::new()
}

fn node_name_for_port(nodes: &[AudioNode], port: &str) -> Option<String> {
    nodes
        .iter()
        .find(|node| node.ports.iter().any(|candidate| candidate == port))
        .map(|node| node.name.clone())
}

fn audio_node_index_by_name(nodes: &[AudioNode], name: &str) -> Option<usize> {
    if name.trim().is_empty() {
        return None;
    }
    nodes.iter().position(|node| node.name == name)
}

fn run_meter_stream(
    id: u64,
    source: String,
    target: String,
    cancel: Arc<AtomicBool>,
    tx: Sender<AppEvent>,
) {
    let path = match command_path("pw-cat") {
        Ok(path) => path,
        Err(err) => {
            let _ = tx.send(AppEvent::Meter {
                id,
                source,
                target,
                level: 0.0,
                err: err.to_string(),
            });
            return;
        }
    };
    let mut last_err = String::new();
    for args in meter_command_specs(&target) {
        match run_meter_command(&path, &args, id, &source, &target, &cancel, &tx) {
            Ok(()) => return,
            Err((frames, err)) => {
                last_err = err;
                if frames > 0 {
                    break;
                }
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return;
        }
    }
    if !last_err.is_empty() {
        let _ = tx.send(AppEvent::Meter {
            id,
            source,
            target,
            level: 0.0,
            err: last_err,
        });
    }
}

fn meter_command_specs(target: &str) -> Vec<Vec<String>> {
    vec![
        vec![
            "--record",
            "--raw",
            "--target",
            target,
            "--rate",
            &METER_SAMPLE_RATE.to_string(),
            "--channels",
            "1",
            "--format",
            "s16",
            "-",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        vec!["-r", "--target", target, "-"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    ]
}

fn run_meter_command(
    path: &str,
    args: &[String],
    id: u64,
    source: &str,
    target: &str,
    cancel: &Arc<AtomicBool>,
    tx: &Sender<AppEvent>,
) -> std::result::Result<(), (usize, String)> {
    let mut child = Command::new(path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| (0, err.to_string()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| (0, "missing stdout".to_string()))?;
    let mut buf = vec![0u8; METER_FRAME_SAMPLES * 2];
    let mut first_chunk = true;
    let mut frames = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            return Ok(());
        }
        match stdout.read_exact(&mut buf) {
            Ok(()) => {
                let mut data = &buf[..];
                if first_chunk {
                    data = strip_wav_header(data);
                    first_chunk = false;
                }
                if data.len() < 2 {
                    continue;
                }
                match volume_from_pcm(data) {
                    Ok(level) => {
                        frames += 1;
                        let _ = tx.send(AppEvent::Meter {
                            id,
                            source: source.to_string(),
                            target: target.to_string(),
                            level,
                            err: String::new(),
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(AppEvent::Meter {
                            id,
                            source: source.to_string(),
                            target: target.to_string(),
                            level: 0.0,
                            err: err.to_string(),
                        });
                    }
                }
            }
            Err(err) => {
                let mut stderr = String::new();
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut stderr);
                }
                let _ = child.wait();
                let command = format!("pw-cat {}", args.join(" "));
                let err = if stderr.trim().is_empty() {
                    format!("{}: {}", command, err)
                } else {
                    format!("{}: {}: {}", command, err, truncate(stderr.trim(), 180))
                };
                return Err((frames, err));
            }
        }
    }
}

fn strip_wav_header(data: &[u8]) -> &[u8] {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return data;
    }
    let mut offset = 12usize;
    while offset + 8 <= data.len() {
        let chunk_id = &data[offset..offset + 4];
        let chunk_size =
            u32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()) as usize;
        offset += 8;
        if chunk_id == b"data" {
            return if offset <= data.len() {
                &data[offset..]
            } else {
                &[]
            };
        }
        offset += chunk_size + (chunk_size % 2);
    }
    &[]
}

fn volume_from_pcm(data: &[u8]) -> Result<f64> {
    if data.len() < 2 {
        return Err(anyhow::anyhow!("no PCM data"));
    }
    let count = data.chunks_exact(2).len();
    if count == 0 {
        return Err(anyhow::anyhow!("no PCM samples"));
    }
    let mut mean = 0.0;
    for chunk in data.chunks_exact(2) {
        let v = i16::from_le_bytes([chunk[0], chunk[1]]) as f64 / 32768.0;
        mean += v;
    }
    mean /= count as f64;
    let mut sum = 0.0;
    for chunk in data.chunks_exact(2) {
        let v = i16::from_le_bytes([chunk[0], chunk[1]]) as f64 / 32768.0;
        let centered = v - mean;
        sum += centered * centered;
    }
    let rms = (sum / count as f64).sqrt();
    let db = 20.0 * (rms + 1e-7).log10();
    if db <= VOLUME_DB_GATE {
        return Ok(0.0);
    }
    let span = VOLUME_DB_CEILING - VOLUME_DB_GATE;
    let linear = ((db - VOLUME_DB_GATE) / span).clamp(0.0, 1.0);
    Ok(linear.powf(1.45))
}

fn smooth_volume(previous: f64, next: f64) -> f64 {
    let mixed = if next > previous {
        previous * 0.15 + next * 0.85
    } else {
        previous * 0.62 + next * 0.38
    };
    mixed.clamp(0.0, 1.0)
}

fn list_recordings() -> Result<Vec<RecordingItem>> {
    let dir = recordings_dir();
    fs::create_dir_all(&dir)?;
    let mut items = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
        {
            continue;
        }
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        items.push(RecordingItem {
            name: path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("recording.wav")
                .to_string(),
            path,
            size: meta.len(),
        });
    }
    items.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(items)
}

fn spawn_record_command(target: &str, path: &Path) -> Result<Child> {
    Command::new(command_path("pw-cat")?)
        .arg("-r")
        .arg("--target")
        .arg(target)
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start pw-cat recording")
}

fn spawn_playback_command(path: &Path) -> Result<Child> {
    let mut errors = Vec::new();
    if let Ok(cmd) = command_path("pw-cat") {
        match Command::new(&cmd)
            .arg("-p")
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(err) => errors.push(format!("pw-cat: {}", err)),
        }
    }
    if let Ok(cmd) = command_path("pw-play") {
        match Command::new(&cmd)
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(err) => errors.push(format!("pw-play: {}", err)),
        }
    }
    if errors.is_empty() {
        Err(anyhow::anyhow!("pw-cat/pw-play not found"))
    } else {
        Err(anyhow::anyhow!(errors.join("; ")))
    }
}

fn interrupt_child(child: &mut Child) {
    let pid = child.id().to_string();
    let _ = Command::new("kill")
        .arg("-INT")
        .arg(&pid)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(30)),
            Err(_) => return,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn recordings_dir() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gxpreset")
        .join("recordings")
}

fn unix_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn sanitize_recording_name(input: &str) -> String {
    let mut out = String::new();
    for c in input.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else if c.is_whitespace() {
            out.push('_');
        }
    }
    let out = out.trim_matches('.').trim_matches('_').to_string();
    let out = out.trim_end_matches(".wav").trim_end_matches(".WAV");
    if out.is_empty() {
        "recording".to_string()
    } else {
        out.to_string()
    }
}

fn sanitize_group_name(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect()
}

fn pedal_group_bank_name(group: &PedalGroup) -> String {
    if group.guitarix_bank.trim().is_empty() {
        generated_pedal_bank_name(&group.name)
    } else {
        group.guitarix_bank.clone()
    }
}

fn generated_pedal_bank_name(name: &str) -> String {
    let label = sanitize_guitarix_label(name, "Pedal Group");
    format!("{}{}", PEDAL_BANK_PREFIX, label)
}

fn generated_pedal_preset_name(index: usize, preset: &PedalPreset) -> String {
    let source = sanitize_guitarix_label(&format!("{} - {}", preset.bank, preset.preset), "Preset");
    format!("{:02} {}", index + 1, limit_chars(&source, 86))
}

fn sanitize_guitarix_label(input: &str, fallback: &str) -> String {
    let out = input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    if out.is_empty() {
        fallback.to_string()
    } else {
        limit_chars(&out, 96)
    }
}

fn limit_chars(input: &str, limit: usize) -> String {
    input.chars().take(limit).collect()
}

fn pedal_group_sync_key(group: &PedalGroup) -> String {
    let mut key = generated_pedal_bank_name(&group.name);
    key.push('\n');
    for preset in &group.presets {
        key.push_str(&preset.bank);
        key.push('\t');
        key.push_str(&preset.preset);
        key.push('\n');
    }
    key
}

fn pedal_group_needs_sync(group: &PedalGroup, dir: &Path) -> bool {
    if group.presets.is_empty() {
        return false;
    }
    let bank = pedal_group_bank_name(group);
    group.sync_key != pedal_group_sync_key(group) || !generated_pedal_bank_path(dir, &bank).exists()
}

fn materialize_pedal_group_bank(group: &mut PedalGroup, dir: &Path) -> Result<PedalBankSync> {
    fs::create_dir_all(dir)?;
    let old_bank = group.guitarix_bank.clone();
    let bank = generated_pedal_bank_name(&group.name);
    let path = generated_pedal_bank_path(dir, &bank);

    if group.presets.is_empty() {
        group.guitarix_bank = bank.clone();
        group.sync_key = pedal_group_sync_key(group);
        let removed = if !old_bank.is_empty() {
            remove_generated_pedal_bank(&old_bank, dir)?.is_some()
        } else {
            remove_generated_pedal_bank(&bank, dir)?.is_some()
        };
        return Ok(PedalBankSync {
            bank,
            path,
            count: 0,
            removed,
        });
    }

    let content = build_pedal_bank_content(group, dir)?;
    if !old_bank.is_empty() && old_bank != bank {
        let _ = remove_generated_pedal_bank(&old_bank, dir);
    }

    let filename = generated_pedal_bank_filename(&bank);
    fs::write(&path, serde_json::to_string_pretty(&content)? + "\n")?;
    upsert_guitarix_banklist_entry(dir, &bank, &filename)?;
    group.guitarix_bank = bank.clone();
    group.sync_key = pedal_group_sync_key(group);

    Ok(PedalBankSync {
        bank,
        path,
        count: group.presets.len(),
        removed: false,
    })
}

fn build_pedal_bank_content(group: &PedalGroup, dir: &Path) -> Result<Value> {
    let mut header = None;
    let mut values = Vec::with_capacity(2 + group.presets.len() * 2);
    for (index, preset) in group.presets.iter().enumerate() {
        let source = read_guitarix_preset_data(dir, &preset.bank, &preset.preset)
            .with_context(|| format!("copy preset {} / {}", preset.bank, preset.preset))?;
        if header.is_none() {
            header = Some(source.0);
        }
        values.push(Value::String(generated_pedal_preset_name(index, preset)));
        values.push(source.1);
    }

    let mut out = Vec::with_capacity(values.len() + 2);
    out.push(Value::String("gx_head_file_version".to_string()));
    out.push(header.unwrap_or_else(default_guitarix_file_version));
    out.extend(values);
    Ok(Value::Array(out))
}

fn read_guitarix_preset_data(dir: &Path, bank: &str, preset: &str) -> Result<(Value, Value)> {
    let path = resolve_guitarix_bank_file(bank, dir)?;
    let data = fs::read_to_string(&path)
        .with_context(|| format!("read Guitarix bank {}", path.display()))?;
    let value: Value = serde_json::from_str(&data)
        .with_context(|| format!("parse Guitarix bank {}", path.display()))?;
    let items = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Guitarix bank is not a JSON array: {}", path.display()))?;
    let header = items
        .get(1)
        .cloned()
        .unwrap_or_else(default_guitarix_file_version);

    for pair in items.chunks_exact(2) {
        if pair
            .first()
            .and_then(Value::as_str)
            .is_some_and(|name| name == preset)
        {
            return Ok((header, pair[1].clone()));
        }
    }
    Err(anyhow::anyhow!(
        "preset {:?} not found in bank {:?}",
        preset,
        bank
    ))
}

fn default_guitarix_file_version() -> Value {
    json!([1, 2, "0.44.1"])
}

fn generated_pedal_bank_filename(bank: &str) -> String {
    format!("{}.gx", clean_filename(bank))
}

fn generated_pedal_bank_path(dir: &Path, bank: &str) -> PathBuf {
    dir.join(generated_pedal_bank_filename(bank))
}

fn is_generated_pedal_bank(bank: &str) -> bool {
    bank.starts_with(PEDAL_BANK_PREFIX)
}

fn remove_generated_pedal_bank(bank: &str, dir: &Path) -> Result<Option<PathBuf>> {
    if bank.trim().is_empty() || !is_generated_pedal_bank(bank) {
        return Ok(None);
    }
    let filename = generated_pedal_bank_filename(bank);
    let path = generated_pedal_bank_path(dir, bank);
    let removed_file = match fs::remove_file(&path) {
        Ok(()) => true,
        Err(err) if err.kind() == io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };
    let removed_entry = remove_guitarix_banklist_entry(dir, bank, &filename)?;
    if removed_file || removed_entry {
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

fn upsert_guitarix_banklist_entry(dir: &Path, bank: &str, filename: &str) -> Result<()> {
    let path = dir.join("banklist.js");
    let mut entries = read_guitarix_banklist(&path)?;
    entries.retain(|entry| !banklist_entry_matches(entry, bank, filename));
    entries.insert(
        0,
        json!([bank, filename, 1, 0, [1, 2], unix_timestamp_seconds()]),
    );
    write_guitarix_banklist(&path, entries)
}

fn remove_guitarix_banklist_entry(dir: &Path, bank: &str, filename: &str) -> Result<bool> {
    let path = dir.join("banklist.js");
    let mut entries = read_guitarix_banklist(&path)?;
    let before = entries.len();
    entries.retain(|entry| !banklist_entry_matches(entry, bank, filename));
    let removed = entries.len() != before;
    if removed {
        write_guitarix_banklist(&path, entries)?;
    }
    Ok(removed)
}

fn read_guitarix_banklist(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(path)?;
    if data.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(&data)
        .with_context(|| format!("parse Guitarix banklist {}", path.display()))?;
    Ok(value.as_array().cloned().unwrap_or_default())
}

fn write_guitarix_banklist(path: &Path, entries: Vec<Value>) -> Result<()> {
    fs::write(
        path,
        serde_json::to_string_pretty(&Value::Array(entries))? + "\n",
    )?;
    Ok(())
}

fn banklist_entry_matches(entry: &Value, bank: &str, filename: &str) -> bool {
    let Some(items) = entry.as_array() else {
        return false;
    };
    let entry_bank = items.first().and_then(Value::as_str).unwrap_or("");
    let entry_file = items.get(1).and_then(Value::as_str).unwrap_or("");
    entry_bank == bank
        || entry_file == filename
        || normalized_key(entry_bank) == normalized_key(bank)
        || normalized_key(entry_file) == normalized_key(filename)
}

fn unix_timestamp_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn guitarix_snapshot(preferred_bank: &str) -> Result<GuitarixSnapshot> {
    let banks = guitarix_banks()?;
    if banks.is_empty() {
        return Err(anyhow::anyhow!("no Guitarix banks returned"));
    }
    let bank = if preferred_bank.is_empty() || !banks.iter().any(|b| b == preferred_bank) {
        banks[0].clone()
    } else {
        preferred_bank.to_string()
    };
    let presets = guitarix_presets(&bank)?;
    Ok(GuitarixSnapshot {
        bank,
        banks,
        presets,
    })
}

fn guitarix_banks() -> Result<Vec<String>> {
    let raw = guitarix_call("banks", json!([]))?;
    extract_names(&raw)
}

fn guitarix_presets(bank: &str) -> Result<Vec<String>> {
    let raw = guitarix_call("presets", json!([bank]))?;
    if let Ok(presets) = serde_json::from_value::<Vec<String>>(raw.clone()) {
        return Ok(presets);
    }
    extract_names(&raw)
}

fn guitarix_set_preset(bank: &str, preset: &str) -> Result<()> {
    guitarix_notify("setpreset", json!([bank, preset]))
}

fn guitarix_bank_check_reparse() -> Result<bool> {
    let raw = guitarix_call("bank_check_reparse", json!([]))?;
    Ok(raw.as_bool().unwrap_or(false))
}

fn guitarix_call(method: &str, params: Value) -> Result<Value> {
    let data = guitarix_rpc(
        json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": "gxpreset",
        }),
        true,
    )?;
    let value: Value = serde_json::from_str(&data)?;
    if !value.get("error").unwrap_or(&Value::Null).is_null() {
        return Err(anyhow::anyhow!("guitarix rpc error: {}", value["error"]));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

fn guitarix_notify(method: &str, params: Value) -> Result<()> {
    let _ = guitarix_rpc(
        json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }),
        false,
    )?;
    Ok(())
}

fn guitarix_rpc(payload: Value, wait_response: bool) -> Result<String> {
    let mut conn = match TcpStream::connect_timeout(
        &"127.0.0.1:7000".parse().unwrap(),
        Duration::from_millis(900),
    ) {
        Ok(conn) => conn,
        Err(err) => {
            if let Err(start_err) = ensure_guitarix_running() {
                return Err(anyhow::anyhow!(
                    "connect Guitarix RPC 127.0.0.1:7000: {}; auto-start failed: {}",
                    err,
                    start_err
                ));
            }
            TcpStream::connect_timeout(
                &"127.0.0.1:7000".parse().unwrap(),
                Duration::from_millis(900),
            )?
        }
    };
    conn.set_read_timeout(Some(Duration::from_millis(1200)))?;
    conn.set_write_timeout(Some(Duration::from_millis(1200)))?;
    let encoded = serde_json::to_string(&payload)?;
    conn.write_all(encoded.as_bytes())?;
    conn.write_all(b"\n")?;
    if !wait_response {
        return Ok(String::new());
    }
    let mut data = String::new();
    let _ = conn.read_to_string(&mut data);
    extract_json_object(&data).ok_or_else(|| anyhow::anyhow!("empty Guitarix RPC response"))
}

fn ensure_guitarix_running() -> Result<()> {
    if guitarix_rpc_ready(Duration::from_millis(120)) {
        return Ok(());
    }
    Command::new(command_path("pw-jack")?)
        .arg("guitarix")
        .arg("-N")
        .arg("-p")
        .arg("7000")
        .env("PIPEWIRE_LATENCY", "128/48000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start pw-jack guitarix -N -p 7000")?;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if guitarix_rpc_ready(Duration::from_millis(180)) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(180));
    }
    Err(anyhow::anyhow!(
        "Guitarix auto-started but RPC did not become ready on 127.0.0.1:7000"
    ))
}

fn guitarix_rpc_ready(timeout: Duration) -> bool {
    TcpStream::connect_timeout(&"127.0.0.1:7000".parse().unwrap(), timeout).is_ok()
}

fn extract_names(raw: &Value) -> Result<Vec<String>> {
    let mut names = Vec::new();
    collect_json_names(raw, &mut names);
    names.sort();
    names.dedup();
    Ok(names)
}

fn collect_json_names(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::String(s) if !s.trim().is_empty() => names.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_json_names(item, names);
            }
        }
        Value::Object(map) => {
            for key in ["name", "bank", "title", "label"] {
                if let Some(Value::String(s)) = map.get(key) {
                    if !s.trim().is_empty() {
                        names.push(s.clone());
                        return;
                    }
                }
            }
        }
        _ => {}
    }
}

fn delete_guitarix_bank(bank: &str, dir: &Path) -> Result<(String, String)> {
    let path = resolve_guitarix_bank_file(bank, dir)?;
    fs::remove_file(&path)?;
    Ok((path.display().to_string(), String::new()))
}

fn resolve_guitarix_bank_file(bank: &str, dir: &Path) -> Result<PathBuf> {
    let candidates = [
        format!("{}.gx", bank),
        format!("{}.gx", clean_filename(bank)),
        format!("{}.gx", bank.replace(' ', "_")),
    ];
    for candidate in candidates {
        let path = dir.join(clean_filename(&candidate));
        if path.exists() {
            return Ok(path);
        }
    }
    let key = normalized_key(bank);
    for entry in fs::read_dir(dir).unwrap_or_else(|_| fs::read_dir(".").unwrap()) {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "gx") {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if normalized_key(stem) == key {
                return Ok(path);
            }
        }
    }
    Err(anyhow::anyhow!(
        "bank file not found for {:?} in {}",
        bank,
        dir.display()
    ))
}

fn command_path(name: &str) -> Result<String> {
    if let Ok(path) = which_like(name) {
        return Ok(path);
    }
    for candidate in [
        format!("/usr/bin/{}", name),
        format!("/usr/sbin/{}", name),
        format!("/bin/{}", name),
        format!("/sbin/{}", name),
    ] {
        if Path::new(&candidate).exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow::anyhow!("{} not found in PATH", name))
}

fn which_like(name: &str) -> Result<String> {
    let path = env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let candidate = Path::new(dir).join(name);
        if candidate.exists() {
            return Ok(candidate.display().to_string());
        }
    }
    Err(anyhow::anyhow!("not found"))
}

fn command_output(name: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(command_path(name)?).args(args).output()?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "{} {}: {}",
            name,
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn required_system_dependencies() -> Vec<SystemDependency> {
    vec![
        SystemDependency {
            command: "pw-link",
            package: "pipewire-bin",
            usage: "PipeWire routing",
        },
        SystemDependency {
            command: "pw-cat",
            package: "pipewire-bin",
            usage: "audio meter, recording and playback",
        },
        SystemDependency {
            command: "pw-jack",
            package: "pipewire-jack",
            usage: "launch Guitarix through PipeWire JACK",
        },
        SystemDependency {
            command: "wireplumber",
            package: "pipewire-audio",
            usage: "PipeWire audio session manager",
        },
        SystemDependency {
            command: "guitarix",
            package: "guitarix",
            usage: "amp/effects engine and preset RPC",
        },
    ]
}

fn check_system_dependencies() -> DependencyStatus {
    DependencyStatus {
        missing: required_system_dependencies()
            .into_iter()
            .filter(|dep| command_path(dep.command).is_err())
            .collect(),
    }
}

impl DependencyStatus {
    fn install_command(&self) -> String {
        let mut seen = HashSet::new();
        let packages = self
            .missing
            .iter()
            .filter_map(|dep| {
                if seen.insert(dep.package) {
                    Some(dep.package)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if packages.is_empty() {
            String::new()
        } else {
            format!(
                "sudo apt update && sudo apt install -y {}",
                packages.join(" ")
            )
        }
    }
}

fn print_system_dependencies(status: &DependencyStatus) {
    if status.missing.is_empty() {
        println!("All system dependencies are installed.");
        return;
    }
    println!("Missing system dependencies:");
    for dep in &status.missing {
        println!("- {} ({}): {}", dep.command, dep.package, dep.usage);
    }
    println!("\n{}", status.install_command());
}

fn load_app_config() -> AppConfig {
    let path = app_config_path();
    fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

fn save_app_config(config: &AppConfig) -> Result<()> {
    let path = app_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(config)? + "\n")?;
    Ok(())
}

fn load_pedals_file() -> PedalsState {
    let file = fs::read_to_string(pedals_config_path())
        .ok()
        .and_then(|data| serde_json::from_str::<PedalsFile>(&data).ok())
        .unwrap_or_default();
    let selected = file.selected.min(file.groups.len().saturating_sub(1));
    PedalsState {
        groups: file.groups,
        selected,
        ..PedalsState::default()
    }
}

fn save_pedals_state(pedals: &PedalsState) -> Result<()> {
    let path = pedals_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = PedalsFile {
        groups: pedals.groups.clone(),
        selected: pedals.selected.min(pedals.groups.len().saturating_sub(1)),
    };
    fs::write(path, serde_json::to_string_pretty(&file)? + "\n")?;
    Ok(())
}

fn app_config_path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gxpreset")
        .join("config.json")
}

fn pedals_config_path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gxpreset")
        .join("pedals.json")
}

fn default_bank_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("guitarix")
        .join("banks")
}

fn print_page(query: &str, order: &str, page: usize, raw_url: &str, items: &[Artifact]) {
    println!(
        "\nGuitarix presets: page {} order={} search={:?}\n{}",
        page, order, query, raw_url
    );
    if items.is_empty() {
        println!("No downloadable .gx files found on this page.");
        return;
    }
    for (i, item) in items.iter().enumerate() {
        let meta = non_empty_join(
            &[&item.author, &item.size, &downloads_label(&item.downloads)],
            " | ",
        );
        println!("{:2}. {:42} {}", i + 1, truncate(&item.name, 42), meta);
    }
}

fn volume_wave_lines(history: &[f64], width: usize, height: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let height = height.max(1);
    let center = (height.saturating_sub(1)) as f64 / 2.0;
    let radius = center.max((height as f64 - center - 1.0).abs()).max(1.0);
    let bar_style = Style::default().fg(Color::Rgb(59, 130, 246));
    let mut lines = Vec::with_capacity(height);
    for y in 0..height {
        let mut row = String::with_capacity(width);
        for x in 0..width {
            let Some(raw_value) = history.get(x).copied() else {
                row.push(' ');
                continue;
            };
            let value = raw_value.clamp(0.0, 1.0) * VISUALIZER_MAX_HEIGHT_RATIO;
            if value <= 0.0001 {
                row.push(if (y as f64 - center).abs() < 0.5 {
                    '─'
                } else {
                    ' '
                });
                continue;
            }
            let amplitude = value * radius;
            let dist = (y as f64 - center).abs();
            row.push(volume_unicode_char(
                amplitude + 0.5 - dist,
                y as f64,
                center,
            ));
        }
        lines.push(Line::from(Span::styled(row, bar_style)));
    }
    lines
}

fn volume_unicode_char(coverage: f64, row: f64, center: f64) -> char {
    if coverage < 0.08 {
        return ' ';
    }
    if coverage >= 0.92 || (row - center).abs() < f64::EPSILON {
        '█'
    } else if row < center {
        '▄'
    } else {
        '▀'
    }
}

fn format_elapsed(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1}M", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

fn tab_span(app: &App, tab: Tab, label: &str) -> Span<'static> {
    Span::styled(
        label.to_string(),
        if app.active_tab == tab {
            active_badge_style()
        } else {
            badge_style()
        },
    )
}

fn panel_block(title: &str, focused: bool) -> Block<'static> {
    Block::default()
        .title(if title.is_empty() {
            String::new()
        } else {
            format!(" {} ", title)
        })
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            accent_style()
        } else {
            border_style()
        })
}

fn title_style() -> Style {
    Style::default()
        .fg(Color::Rgb(11, 19, 32))
        .bg(Color::Rgb(94, 234, 212))
        .add_modifier(Modifier::BOLD)
}
fn badge_style() -> Style {
    Style::default()
        .fg(Color::Rgb(226, 232, 240))
        .bg(Color::Rgb(51, 65, 85))
        .add_modifier(Modifier::BOLD)
}
fn active_badge_style() -> Style {
    Style::default()
        .fg(Color::Rgb(11, 19, 32))
        .bg(Color::Rgb(251, 191, 36))
        .add_modifier(Modifier::BOLD)
}
fn selected_style() -> Style {
    Style::default()
        .fg(Color::Rgb(11, 19, 32))
        .bg(Color::Rgb(147, 197, 253))
        .add_modifier(Modifier::BOLD)
}
fn success_style() -> Style {
    Style::default().fg(Color::Rgb(134, 239, 172))
}
fn error_style() -> Style {
    Style::default()
        .fg(Color::Rgb(252, 165, 165))
        .add_modifier(Modifier::BOLD)
}
fn muted_style() -> Style {
    Style::default().fg(Color::Rgb(100, 116, 139))
}
fn accent_style() -> Style {
    Style::default().fg(Color::Rgb(94, 234, 212))
}
fn item_style() -> Style {
    Style::default().fg(Color::Rgb(226, 232, 240))
}
fn border_style() -> Style {
    Style::default().fg(Color::Rgb(51, 65, 85))
}

fn label_line(label: &str, value: &str, width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{}: ", label), muted_style()),
        Span::raw(truncate(value, width.saturating_sub(label.len() + 2))),
    ])
}

fn help_line(app: &App) -> String {
    match app.active_tab {
        Tab::Audio => "tab/shift-tab view  ,/; pedal prev/next  h/l focus  up/down select  enter picker  R record  r refresh  q quit".to_string(),
        Tab::Pedals => "tab/shift-tab view  ,/; pedal prev/next  h/l focus  n new  e rename  a add  enter load  x delete  q quit".to_string(),
        Tab::Recordings => "tab/shift-tab view  ,/; pedal prev/next  up/down select  enter/p play  s stop  e rename  x delete  r refresh  q quit".to_string(),
        Tab::Guitarix => "tab/shift-tab view  ,/; pedal prev/next  h banks  l presets  enter/s switch preset  x delete bank  r refresh  q quit".to_string(),
        Tab::Library => "tab/shift-tab view  ,/; pedal prev/next  up/down select  enter/d download  a all visible  / search  n/p page  o order  c crawl  r refresh  q quit".to_string(),
    }
}

fn next_order(current: &str) -> String {
    let orders = [
        "created_at",
        "most_downloaded",
        "top_rated",
        "name",
        "updated_at",
    ];
    let index = orders
        .iter()
        .position(|order| *order == current)
        .unwrap_or(0);
    orders[(index + 1) % orders.len()].to_string()
}

fn clean_text(s: &str) -> String {
    let stripped = Regex::new(r"(?s)<[^>]+>").unwrap().replace_all(s, " ");
    html_escape::decode_html_entities(&stripped)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn get_attr(tag: &str, name: &str) -> String {
    for quote in ['"', '\''] {
        let needle = format!("{}={}", name, quote);
        if let Some(start) = tag.find(&needle) {
            let rest = &tag[start + needle.len()..];
            if let Some(end) = rest.find(quote) {
                return html_escape::decode_html_entities(&rest[..end]).to_string();
            }
        }
    }
    String::new()
}

fn absolutize(raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else if raw.starts_with('/') {
        format!("{}{}", BASE_URL, raw)
    } else {
        raw.to_string()
    }
}

fn clean_filename(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | ' ') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn path_base_from_url(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(|mut s| s.next_back().map(str::to_string))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| raw.rsplit('/').next().unwrap_or(raw).to_string())
}

fn normalized_key(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn first_non_empty<'a>(values: &[&'a str]) -> &'a str {
    values
        .iter()
        .copied()
        .find(|value| !value.trim().is_empty())
        .unwrap_or("")
}

fn non_empty_join(values: &[&str], sep: &str) -> String {
    values
        .iter()
        .copied()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

fn downloads_label(downloads: &str) -> String {
    if downloads.trim().is_empty() {
        String::new()
    } else {
        format!("{} downloads", downloads.trim())
    }
}

fn truncate(s: &str, limit: usize) -> String {
    let mut chars = s.chars();
    let mut out = String::new();
    for _ in 0..limit {
        if let Some(ch) = chars.next() {
            out.push(ch);
        } else {
            return s.to_string();
        }
    }
    if chars.next().is_some() && limit > 1 {
        out.pop();
        out.push('…');
    }
    out
}

fn wrap_text(s: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in s.split_whitespace() {
        if !current.is_empty() && current.len() + 1 + word.len() > width {
            lines.push(current);
            current = String::new();
            if lines.len() >= max_lines {
                return lines;
            }
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    lines
}

fn split_clean_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_midi_name(name: &str) -> bool {
    name.to_lowercase().contains("midi")
}

fn extract_json_object(data: &str) -> Option<String> {
    let start = data.find('{')?;
    let end = data.rfind('}')?;
    (end >= start).then(|| data[start..=end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("gxpreset-{}-{}", name, unix_timestamp_ms()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn materializes_pedal_group_as_guitarix_bank() {
        let dir = test_dir("pedal-bank");
        fs::write(
            dir.join("Source.gx"),
            r#"[
  "gx_head_file_version", [1, 2, "0.44.1"],
  "Clean", {"engine": {"amp.on_off": 0}},
  "Lead", {"engine": {"amp.on_off": 1}}
]"#,
        )
        .unwrap();
        fs::write(
            dir.join("banklist.js"),
            r#"[["Source", "Source.gx", 1, 0, [1, 2], 1]]"#,
        )
        .unwrap();

        let mut group = PedalGroup {
            name: "Live".to_string(),
            guitarix_bank: String::new(),
            presets: vec![
                PedalPreset {
                    bank: "Source".to_string(),
                    preset: "Clean".to_string(),
                },
                PedalPreset {
                    bank: "Source".to_string(),
                    preset: "Lead".to_string(),
                },
            ],
            current: 0,
            sync_key: String::new(),
        };

        let sync = materialize_pedal_group_bank(&mut group, &dir).unwrap();
        assert_eq!(sync.bank, "gxpreset - Live");
        assert_eq!(sync.count, 2);
        assert_eq!(group.sync_key, pedal_group_sync_key(&group));

        let generated: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("gxpreset - Live.gx")).unwrap())
                .unwrap();
        let items = generated.as_array().unwrap();
        assert_eq!(items[2].as_str(), Some("01 Source - Clean"));
        assert_eq!(items[4].as_str(), Some("02 Source - Lead"));

        let banklist: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("banklist.js")).unwrap()).unwrap();
        let first = banklist.as_array().unwrap()[0].as_array().unwrap();
        assert_eq!(first[0].as_str(), Some("gxpreset - Live"));
        assert_eq!(first[1].as_str(), Some("gxpreset - Live.gx"));

        let _ = fs::remove_dir_all(dir);
    }
}
