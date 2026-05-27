package main

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"html"
	"io"
	"math"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

const baseURL = "https://musical-artifacts.com"

const (
	spectrumRangeCount = 10
	meterSpectrumBands = 24
	meterSampleRate    = 48000
	meterFrameSamples  = 4096
)

var (
	stripTagsRe = regexp.MustCompile(`(?s)<[^>]+>`)
	nameRe      = regexp.MustCompile(`(?s)<h2 class='artifact-name'>\s*<a href="/artifacts/([0-9]+)">([^<]+)</a>`)
	h1NameRe    = regexp.MustCompile(`(?s)<h1 class='artifact-name'>\s*(.*?)\s*</h1>`)
	authorRe    = regexp.MustCompile(`(?s)\bby\s*<a[^>]*>([^<]+)</a>`)
	descRe      = regexp.MustCompile(`(?s)<div class='artifact-description'>\s*(.*?)\s*</div>`)
	sizeRe      = regexp.MustCompile(`\(([^)]+)\)`)
	countRe     = regexp.MustCompile(`^[0-9][0-9,.\s]*`)
	idInURLRe   = regexp.MustCompile(`/artifacts/([0-9]+)(?:/|$)`)
)

var guitarixLaunchMu sync.Mutex

var (
	appStyle = lipgloss.NewStyle().
			Padding(2, 4)
	titleStyle = lipgloss.NewStyle().
			Bold(true).
			Foreground(lipgloss.Color("#0B1320")).
			Background(lipgloss.Color("#5EEAD4")).
			Padding(0, 1)
	subtitleStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("#94A3B8"))
	badgeStyle = lipgloss.NewStyle().
			Bold(true).
			Foreground(lipgloss.Color("#E2E8F0")).
			Background(lipgloss.Color("#334155")).
			Padding(0, 1)
	activeBadgeStyle = lipgloss.NewStyle().
				Bold(true).
				Foreground(lipgloss.Color("#0B1320")).
				Background(lipgloss.Color("#FBBF24")).
				Padding(0, 1)
	successStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("#86EFAC"))
	errorStyle = lipgloss.NewStyle().
			Bold(true).
			Foreground(lipgloss.Color("#FCA5A5"))
	mutedStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("#64748B"))
	accentStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("#5EEAD4"))
	selectedStyle = lipgloss.NewStyle().
			Bold(true).
			Foreground(lipgloss.Color("#0B1320")).
			Background(lipgloss.Color("#93C5FD")).
			Padding(0, 1)
	itemStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("#E2E8F0"))
	panelStyle = lipgloss.NewStyle().
			Border(lipgloss.RoundedBorder()).
			BorderForeground(lipgloss.Color("#334155")).
			Padding(0, 1)
	focusPanelStyle = panelStyle.Copy().
			BorderForeground(lipgloss.Color("#5EEAD4"))
	logPanelStyle = panelStyle.Copy().
			BorderForeground(lipgloss.Color("#475569"))
	spectrumEmptyStyle = lipgloss.NewStyle().
				Foreground(lipgloss.Color("#64748B"))
	spectrumFillStyles = []lipgloss.Style{
		lipgloss.NewStyle().Foreground(lipgloss.Color("#22C55E")),
		lipgloss.NewStyle().Foreground(lipgloss.Color("#84CC16")),
		lipgloss.NewStyle().Foreground(lipgloss.Color("#EAB308")),
		lipgloss.NewStyle().Foreground(lipgloss.Color("#F97316")),
		lipgloss.NewStyle().Foreground(lipgloss.Color("#EF4444")),
	}
	spectrumLabels = []string{"55", "90", "150", "250", "420", "700", "1k2", "2k", "4k", "8k+"}
)

type Artifact struct {
	ID          string
	Name        string
	Author      string
	Description string
	Filename    string
	DownloadURL string
	PageURL     string
	Size        string
	Downloads   string
}

type fetchMsg struct {
	items  []Artifact
	rawURL string
	query  string
	order  string
	page   int
	err    error
}

type downloadEvent struct {
	kind     string
	artifact Artifact
	path     string
	err      error
}

type crawlDoneMsg struct {
	pages int
	count int
	err   error
}

type downloadsDoneMsg struct{}

type audioMsg struct {
	outputs []AudioNode
	inputs  []AudioNode
	links   map[string]map[string]bool
	err     error
}

type audioActionMsg struct {
	action string
	err    error
}

type meterMsg struct {
	streamID int
	source   string
	spectrum []float64
	err      error
}

type meterStoppedMsg struct {
	streamID int
	source   string
}

type guitarixMsg struct {
	banks   []string
	presets []string
	err     error
}

type guitarixPresetMsg struct {
	bank   string
	preset string
	err    error
}

type guitarixBankDeleteMsg struct {
	bank string
	path string
	warn string
	err  error
}

type mode int

const (
	modeBrowse mode = iota
	modeSearch
)

type tab int

const (
	tabLibrary tab = iota
	tabAudio
	tabGuitarix
)

const (
	audioFocusConnections = iota
	audioFocusMeter
)

type AudioNode struct {
	Name  string
	Ports []string
}

type AudioState struct {
	Outputs       []AudioNode
	Inputs        []AudioNode
	Links         map[string]map[string]bool
	OutSelected   int
	InSelected    int
	MeterSelected int
	Focus         int
	PickingTarget bool
	Loading       bool
	Err           string
	Spectrum      []float64
	SpectrumShown []float64
	MeterSource   string
	MeterErr      string
}

type GuitarixState struct {
	Banks          []string
	Presets        []string
	BankSelected   int
	PresetSelected int
	Focus          int
	Loading        bool
	ConfirmDelete  bool
	Err            string
	CurrentBank    string
	CurrentPreset  string
}

type AppConfig struct {
	LastMeterSource string `json:"last_meter_source,omitempty"`
}

type SystemDependency struct {
	Command string
	Package string
	Use     string
}

type SystemDependencyStatus struct {
	Missing []SystemDependency
}

type model struct {
	client     *http.Client
	downloader *Downloader
	activeTab  tab
	config     AppConfig
	deps       SystemDependencyStatus

	query string
	order string
	page  int
	url   string

	items    []Artifact
	selected int
	loading  bool
	err      string

	mode  mode
	input string

	width  int
	height int

	log      []string
	help     bool
	quitting bool
	crawling bool

	audio   AudioState
	guitarx GuitarixState

	meter    *MeterStream
	meterSeq int
}

type MeterStream struct {
	id     int
	source string
	events chan meterMsg
	cancel context.CancelFunc
}

type Downloader struct {
	client *http.Client
	dest   string
	force  bool
	queue  chan Artifact
	events chan downloadEvent
	wg     sync.WaitGroup
	stop   sync.Once

	mu      sync.Mutex
	seen    map[string]bool
	queued  int
	active  int
	done    int
	failed  int
	skipped int
}

type DownloaderStats struct {
	Queued  int
	Active  int
	Pending int
	Done    int
	Failed  int
	Skipped int
}

func main() {
	var dest string
	var query string
	var order string
	var page int
	var workers int
	var force bool
	var once bool
	var installAll bool
	var depsOnly bool

	flag.StringVar(&dest, "dir", defaultBankDir(), "destination Guitarix bank directory")
	flag.StringVar(&query, "search", "", "initial search query")
	flag.StringVar(&order, "order", "created_at", "order: created_at, updated_at, most_downloaded, top_rated, name")
	flag.IntVar(&page, "page", 1, "initial page")
	flag.IntVar(&workers, "workers", 4, "parallel download workers")
	flag.BoolVar(&force, "force", false, "overwrite existing .gx files")
	flag.BoolVar(&once, "once", false, "print current page and exit")
	flag.BoolVar(&installAll, "install-all", false, "download every .gx on the current page and exit")
	flag.BoolVar(&depsOnly, "deps", false, "print missing system dependencies and install command")
	flag.Parse()

	if workers < 1 {
		workers = 1
	}
	if page < 1 {
		page = 1
	}

	client := &http.Client{Timeout: 45 * time.Second}
	deps := checkSystemDependencies()

	if depsOnly {
		printSystemDependencies(deps)
		return
	}

	if once {
		items, rawURL, err := FetchArtifacts(client, query, order, page)
		if err != nil {
			fatalf("fetch failed: %v\n", err)
		}
		printPage(query, order, page, rawURL, items)
		return
	}

	if installAll {
		if err := installAllVisible(client, dest, workers, force, query, order, page); err != nil {
			fatalf("install failed: %v\n", err)
		}
		return
	}

	events := make(chan downloadEvent, 512)
	downloader := NewDownloader(client, dest, workers, force, events)
	defer downloader.Stop()
	config := loadAppConfig()

	m := model{
		client:     client,
		downloader: downloader,
		activeTab:  tabLibrary,
		config:     config,
		deps:       deps,
		query:      query,
		order:      order,
		page:       page,
		loading:    true,
		help:       true,
		audio: AudioState{
			Loading: true,
			Links:   make(map[string]map[string]bool),
		},
		guitarx: GuitarixState{
			Loading: true,
		},
	}

	p := tea.NewProgram(m, tea.WithAltScreen())
	if _, err := p.Run(); err != nil {
		fatalf("tui failed: %v\n", err)
	}
}

func (m model) Init() tea.Cmd {
	return tea.Batch(
		fetchCmd(m.client, m.query, m.order, m.page),
		audioRefreshCmd(),
		guitarixRefreshCmd(""),
		listenDownloadEvents(m.downloader.eventsChan()),
	)
}

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width = msg.Width
		m.height = msg.Height
		return m, nil
	case fetchMsg:
		m.loading = false
		m.crawling = false
		if msg.err != nil {
			m.err = msg.err.Error()
			return m, nil
		}
		m.err = ""
		m.items = msg.items
		m.url = msg.rawURL
		m.query = msg.query
		m.order = msg.order
		m.page = msg.page
		if m.selected >= len(m.items) {
			m.selected = len(m.items) - 1
		}
		if m.selected < 0 {
			m.selected = 0
		}
		return m, nil
	case downloadEvent:
		m.addLog(formatDownloadEvent(msg))
		return m, listenDownloadEvents(m.downloader.eventsChan())
	case audioMsg:
		m.audio.Loading = false
		if msg.err != nil {
			m.audio.Err = msg.err.Error()
			return m, nil
		}
		m.audio.Err = ""
		m.audio.Outputs = msg.outputs
		m.audio.Inputs = msg.inputs
		m.audio.Links = msg.links
		m.audio.OutSelected = clampIndex(m.audio.OutSelected, len(m.audio.Outputs))
		m.audio.InSelected = clampIndex(m.audio.InSelected, len(m.audio.Inputs))
		m.audio.MeterSelected = clampIndex(m.audio.MeterSelected, len(m.audio.Outputs))
		if index := audioNodeIndexByName(m.audio.Outputs, m.config.LastMeterSource); index >= 0 {
			m.audio.MeterSelected = index
		}
		if m.activeTab == tabAudio {
			return m.ensureMeterStream()
		}
		return m, nil
	case audioActionMsg:
		if msg.err != nil {
			m.addLog(msg.action + " failed: " + msg.err.Error())
		} else {
			m.addLog(msg.action + " ok")
		}
		m.audio.Loading = true
		return m, audioRefreshCmd()
	case meterMsg:
		if m.meter == nil || msg.streamID != m.meter.id {
			return m, nil
		}
		if msg.err != nil {
			m.audio.MeterErr = msg.err.Error()
			m = m.stopMeterStream()
			return m, nil
		}
		m.audio.MeterErr = ""
		m.audio.Spectrum = msg.spectrum
		m.audio.MeterSource = msg.source
		m.audio.SpectrumShown = smoothDisplaySpectrum(m.audio.SpectrumShown, msg.spectrum)
		return m, listenMeterStream(m.meter)
	case meterStoppedMsg:
		if m.meter != nil && msg.streamID == m.meter.id {
			if m.audio.MeterErr == "" {
				m.audio.MeterErr = "meter stream stopped"
			}
			m.meter = nil
		}
		return m, nil
	case guitarixMsg:
		m.guitarx.Loading = false
		if msg.err != nil {
			m.guitarx.Err = msg.err.Error()
			return m, nil
		}
		m.guitarx.Err = ""
		m.guitarx.Banks = msg.banks
		m.guitarx.BankSelected = clampIndex(m.guitarx.BankSelected, len(m.guitarx.Banks))
		m.guitarx.Presets = msg.presets
		m.guitarx.PresetSelected = clampIndex(m.guitarx.PresetSelected, len(m.guitarx.Presets))
		return m, nil
	case guitarixPresetMsg:
		if msg.err != nil {
			m.addLog("guitarix preset failed: " + msg.err.Error())
		} else {
			m.guitarx.CurrentBank = msg.bank
			m.guitarx.CurrentPreset = msg.preset
			m.addLog("guitarix preset: " + msg.bank + " / " + msg.preset)
		}
		return m, nil
	case guitarixBankDeleteMsg:
		m.guitarx.Loading = false
		m.guitarx.ConfirmDelete = false
		if msg.err != nil {
			m.addLog("delete bank failed: " + msg.bank + ": " + msg.err.Error())
			return m, nil
		}
		line := "deleted bank: " + msg.bank
		if msg.path != "" {
			line += " (" + msg.path + ")"
		}
		if msg.warn != "" {
			line += "; " + msg.warn
		}
		m.addLog(line)
		return m, guitarixRefreshCmd("")
	case crawlDoneMsg:
		m.crawling = false
		if msg.err != nil {
			m.addLog("crawl failed: " + msg.err.Error())
		} else {
			m.addLog(fmt.Sprintf("crawl queued %d files from %d page(s)", msg.count, msg.pages))
		}
		return m, nil
	case downloadsDoneMsg:
		return m, tea.Quit
	case tea.KeyMsg:
		if m.mode == modeSearch {
			return m.updateSearch(msg)
		}
		return m.updateBrowse(msg)
	}

	return m, nil
}

func (m model) updateBrowse(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "ctrl+c":
		m = m.stopMeterStream()
		return m, tea.Quit
	case "q":
		m = m.stopMeterStream()
		if m.downloader.HasWork() {
			m.quitting = true
			m.addLog("waiting for downloads before exit")
			return m, waitDownloadsCmd(m.downloader)
		}
		return m, tea.Quit
	case "tab":
		m.activeTab = (m.activeTab + 1) % 3
		return m.onTabActivated()
	case "shift+tab":
		m.activeTab = (m.activeTab + 2) % 3
		return m.onTabActivated()
	case "?":
		m.help = !m.help
		return m, nil
	}

	switch m.activeTab {
	case tabAudio:
		return m.updateAudio(msg)
	case tabGuitarix:
		return m.updateGuitarix(msg)
	default:
		return m.updateLibrary(msg)
	}
}

func (m model) updateLibrary(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "up", "k":
		if m.selected > 0 {
			m.selected--
		}
	case "down", "j":
		if m.selected < len(m.items)-1 {
			m.selected++
		}
	case "home":
		m.selected = 0
	case "end":
		if len(m.items) > 0 {
			m.selected = len(m.items) - 1
		}
	case "enter", "d":
		if len(m.items) > 0 {
			m.downloader.Enqueue(m.items[m.selected])
		}
	case "a":
		for _, item := range m.items {
			m.downloader.Enqueue(item)
		}
	case "/":
		m.mode = modeSearch
		m.input = m.query
	case "esc":
		m.err = ""
	case "n", "right", "pgdown":
		m.page++
		m.loading = true
		return m, fetchCmd(m.client, m.query, m.order, m.page)
	case "p", "left", "pgup":
		if m.page > 1 {
			m.page--
		}
		m.loading = true
		return m, fetchCmd(m.client, m.query, m.order, m.page)
	case "r":
		m.loading = true
		return m, fetchCmd(m.client, m.query, m.order, m.page)
	case "o":
		m.order = nextOrder(m.order)
		m.page = 1
		m.loading = true
		return m, fetchCmd(m.client, m.query, m.order, m.page)
	case "c":
		if !m.crawling {
			m.crawling = true
			return m, crawlCmd(m.client, m.downloader, m.query, m.order, 4)
		}
	}
	return m, nil
}

func (m model) updateAudio(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	if m.audio.PickingTarget {
		return m.updateAudioTargetPicker(msg)
	}

	switch msg.String() {
	case "left", "h":
		m.audio.Focus = audioFocusConnections
	case "right", "l":
		m.audio.Focus = audioFocusMeter
	case "up", "k":
		if m.audio.Focus == audioFocusConnections && m.audio.OutSelected > 0 {
			m.audio.OutSelected--
		}
		if m.audio.Focus == audioFocusMeter && m.audio.MeterSelected > 0 {
			m.audio.MeterSelected--
		}
	case "down", "j":
		if m.audio.Focus == audioFocusConnections && m.audio.OutSelected < len(m.audio.Outputs)-1 {
			m.audio.OutSelected++
		}
		if m.audio.Focus == audioFocusMeter && m.audio.MeterSelected < len(m.audio.Outputs)-1 {
			m.audio.MeterSelected++
		}
	case "home":
		if m.audio.Focus == audioFocusConnections {
			m.audio.OutSelected = 0
		} else {
			m.audio.MeterSelected = 0
		}
	case "end":
		if m.audio.Focus == audioFocusConnections {
			m.audio.OutSelected = max(0, len(m.audio.Outputs)-1)
		} else {
			m.audio.MeterSelected = max(0, len(m.audio.Outputs)-1)
		}
	case "r":
		m.audio.Loading = true
		m.audio.PickingTarget = false
		return m, audioRefreshCmd()
	case "enter", "c":
		if m.audio.Focus == audioFocusConnections && m.audio.selectedOutput().Name != "" {
			m.audio.PickingTarget = true
			m.audio.InSelected = clampIndex(m.audio.InSelected, len(m.audio.Inputs))
			return m, nil
		}
	case "m":
		m.audio.Focus = audioFocusMeter
	case "s":
		m.audio.Focus = audioFocusConnections
	case "x", "backspace", "esc":
		if m.audio.PickingTarget {
			m.audio.PickingTarget = false
		}
	}
	return m.ensureMeterStream()
}

func (m model) updateAudioTargetPicker(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "esc", "left", "h":
		m.audio.PickingTarget = false
		return m, nil
	case "up", "k":
		if m.audio.InSelected > 0 {
			m.audio.InSelected--
		}
	case "down", "j":
		if m.audio.InSelected < len(m.audio.Inputs)-1 {
			m.audio.InSelected++
		}
	case "home":
		m.audio.InSelected = 0
	case "end":
		m.audio.InSelected = max(0, len(m.audio.Inputs)-1)
	case "r":
		m.audio.PickingTarget = false
		m.audio.Loading = true
		return m, audioRefreshCmd()
	case "enter", "c":
		out, in := m.audio.selectedOutput(), m.audio.selectedInput()
		m.audio.PickingTarget = false
		if out.Name != "" && in.Name != "" {
			return m, audioConnectCmd(out, in)
		}
	case " ":
		out, in := m.audio.selectedOutput(), m.audio.selectedInput()
		if out.Name != "" && in.Name != "" {
			if m.audio.nodesConnected(out, in) {
				return m, audioDisconnectCmd(out, in)
			}
			return m, audioConnectCmd(out, in)
		}
	case "x", "backspace":
		out, in := m.audio.selectedOutput(), m.audio.selectedInput()
		m.audio.PickingTarget = false
		if out.Name != "" && in.Name != "" {
			return m, audioDisconnectCmd(out, in)
		}
	}
	return m, nil
}

func (m model) updateGuitarix(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	if m.guitarx.ConfirmDelete {
		switch msg.String() {
		case "y", "Y", "enter":
			bank := m.guitarx.selectedBank()
			m.guitarx.ConfirmDelete = false
			m.guitarx.Loading = true
			if bank != "" {
				return m, guitarixDeleteBankCmd(bank, m.downloader.dest)
			}
		case "n", "N", "esc":
			m.guitarx.ConfirmDelete = false
			return m, nil
		}
		return m, nil
	}

	switch msg.String() {
	case "left", "h":
		m.guitarx.Focus = 0
	case "right", "l":
		m.guitarx.Focus = 1
	case "up", "k":
		if m.guitarx.Focus == 0 && m.guitarx.BankSelected > 0 {
			m.guitarx.BankSelected--
			m.guitarx.PresetSelected = 0
			m.guitarx.Loading = true
			return m, guitarixRefreshCmd(m.guitarx.selectedBank())
		}
		if m.guitarx.Focus == 1 && m.guitarx.PresetSelected > 0 {
			m.guitarx.PresetSelected--
		}
	case "down", "j":
		if m.guitarx.Focus == 0 && m.guitarx.BankSelected < len(m.guitarx.Banks)-1 {
			m.guitarx.BankSelected++
			m.guitarx.PresetSelected = 0
			m.guitarx.Loading = true
			return m, guitarixRefreshCmd(m.guitarx.selectedBank())
		}
		if m.guitarx.Focus == 1 && m.guitarx.PresetSelected < len(m.guitarx.Presets)-1 {
			m.guitarx.PresetSelected++
		}
	case "home":
		if m.guitarx.Focus == 0 {
			m.guitarx.BankSelected = 0
			m.guitarx.PresetSelected = 0
			m.guitarx.Loading = true
			return m, guitarixRefreshCmd(m.guitarx.selectedBank())
		}
		m.guitarx.PresetSelected = 0
	case "end":
		if m.guitarx.Focus == 0 {
			m.guitarx.BankSelected = max(0, len(m.guitarx.Banks)-1)
			m.guitarx.PresetSelected = 0
			m.guitarx.Loading = true
			return m, guitarixRefreshCmd(m.guitarx.selectedBank())
		}
		m.guitarx.PresetSelected = max(0, len(m.guitarx.Presets)-1)
	case "r":
		m.guitarx.Loading = true
		return m, guitarixRefreshCmd(m.guitarx.selectedBank())
	case "enter", "s":
		bank := m.guitarx.selectedBank()
		preset := m.guitarx.selectedPreset()
		if bank != "" && preset != "" {
			return m, guitarixSetPresetCmd(bank, preset)
		}
	case "x", "delete", "backspace":
		if m.guitarx.Focus == 0 && m.guitarx.selectedBank() != "" {
			m.guitarx.ConfirmDelete = true
			return m, nil
		}
	}
	return m, nil
}

func (m model) onTabActivated() (tea.Model, tea.Cmd) {
	switch m.activeTab {
	case tabAudio:
		var cmds []tea.Cmd
		if len(m.audio.Outputs) == 0 && !m.audio.Loading {
			m.audio.Loading = true
			cmds = append(cmds, audioRefreshCmd())
		}
		next, cmd := m.ensureMeterStream()
		if cmd != nil {
			cmds = append(cmds, cmd)
		}
		return next, tea.Batch(cmds...)
	case tabGuitarix:
		m.audio.PickingTarget = false
		m = m.stopMeterStream()
		if len(m.guitarx.Banks) == 0 && !m.guitarx.Loading {
			m.guitarx.Loading = true
			return m, guitarixRefreshCmd("")
		}
	default:
		m.audio.PickingTarget = false
		m = m.stopMeterStream()
	}
	return m, nil
}

func (m model) ensureMeterStream() (model, tea.Cmd) {
	if m.activeTab != tabAudio {
		return m.stopMeterStream(), nil
	}
	source := m.audio.selectedMeterSourceName()
	if source == "" || isMidiName(source) {
		m = m.stopMeterStream()
		m.audio.MeterSource = ""
		return m, nil
	}
	if source != m.config.LastMeterSource {
		m.config.LastMeterSource = source
		if err := saveAppConfig(m.config); err != nil {
			m.addLog("config save failed: " + err.Error())
		}
	}
	if m.meter != nil && m.meter.source == source {
		return m, nil
	}
	m = m.stopMeterStream()
	m.meterSeq++
	stream := startMeterStream(m.meterSeq, source)
	m.meter = stream
	m.audio.MeterSource = source
	m.audio.MeterErr = ""
	return m, listenMeterStream(stream)
}

func (m model) stopMeterStream() model {
	if m.meter != nil {
		m.meter.cancel()
		m.meter = nil
	}
	return m
}

func (m model) updateSearch(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "enter":
		m.query = strings.TrimSpace(m.input)
		m.page = 1
		m.mode = modeBrowse
		m.loading = true
		return m, fetchCmd(m.client, m.query, m.order, m.page)
	case "esc":
		m.mode = modeBrowse
		m.input = ""
	case "backspace", "ctrl+h":
		if len(m.input) > 0 {
			r := []rune(m.input)
			m.input = string(r[:len(r)-1])
		}
	case "ctrl+u":
		m.input = ""
	case "ctrl+c":
		return m, tea.Quit
	default:
		if msg.Type == tea.KeyRunes {
			m.input += msg.String()
		}
	}
	return m, nil
}

func (m model) View() string {
	if m.quitting {
		return appStyle.Render("\n" +
			panel("Exit", "Waiting for downloads to finish...\n\n"+m.statusView(), safeWidth(m.width)-2, true) +
			"\n")
	}

	width := safeWidth(m.width)
	height := safeHeight(m.height)
	header := m.headerView(width)
	footer := m.footerView(width)
	contentHeight := height - lipgloss.Height(header) - lipgloss.Height(footer)
	if contentHeight < 1 {
		contentHeight = 1
	}
	content := fitHeight(m.contentView(width, contentHeight), contentHeight)
	content = lipgloss.PlaceVertical(contentHeight, lipgloss.Top, content)
	sections := []string{header, content, footer}
	return appStyle.Render(lipgloss.JoinVertical(lipgloss.Left, sections...))
}

func (m model) headerView(width int) string {
	left := titleStyle.Render("gxpreset")
	right := subtitleStyle.Render("Guitarix rig control")
	line := lipgloss.JoinHorizontal(lipgloss.Center, left, " ", right)

	var state string
	switch {
	case m.loading:
		state = activeBadgeStyle.Render("LOADING")
	case m.err != "":
		state = errorStyle.Render("ERROR")
	case m.crawling:
		state = activeBadgeStyle.Render("CRAWL")
	default:
		state = successStyle.Render("READY")
	}

	query := m.query
	if query == "" {
		query = "all guitarix"
	}
	meta := strings.Join([]string{
		badgeStyle.Render("page " + strconv.Itoa(m.page)),
		badgeStyle.Render("order " + m.order),
		badgeStyle.Render(strconv.Itoa(len(m.items)) + " files"),
	}, " ")
	if m.mode == modeSearch {
		meta = activeBadgeStyle.Render("search "+m.input+"_") + " " + meta
	}

	body := lipgloss.JoinVertical(
		lipgloss.Left,
		line,
		m.tabBarView(),
		fmt.Sprintf("%s  %s  %s", state, accentStyle.Render(query), meta),
		mutedStyle.Render(truncate(m.downloader.dest, width-4)),
	)
	return panel("", body, width, false)
}

func (m model) tabBarView() string {
	labels := []struct {
		tab   tab
		label string
	}{
		{tabLibrary, "Library"},
		{tabAudio, "Audio"},
		{tabGuitarix, "Guitarix"},
	}
	var parts []string
	for _, item := range labels {
		style := badgeStyle
		if m.activeTab == item.tab {
			style = activeBadgeStyle
		}
		parts = append(parts, style.Render(item.label))
	}
	return strings.Join(parts, " ")
}

func (m model) helpLine() string {
	switch m.activeTab {
	case tabAudio:
		return "tab view  h/← left  l/→ right  ↑/↓ select  enter target picker  space toggle  x disconnect  esc close  r refresh  q quit  ? hide"
	case tabGuitarix:
		return "tab view  h/← banks  l/→ presets  ↑/↓ select  enter/s switch preset  x delete bank  r refresh  q quit  ? hide"
	default:
		return "tab view  ↑/↓ select  enter/d download  a all visible  / search  n/p page  o order  c crawl  r refresh  q quit  ? hide"
	}
}

func (m model) contentView(width int, height int) string {
	switch m.activeTab {
	case tabAudio:
		return m.audioView(width, height)
	case tabGuitarix:
		return m.guitarixView(width)
	}

	if m.mode == modeSearch {
		body := lipgloss.NewStyle().Width(width - 6).Render(
			"Type a search and press Enter. Esc cancels.\n\n" +
				selectedStyle.Render(" "+m.input+"_ "),
		)
		return panel("Search", body, width, true)
	}

	if width >= 104 {
		gap := 1
		listW := int(float64(width-gap) * 0.58)
		detailW := width - listW - gap
		list := panel("Presets", m.listView(listW-4), listW, true)
		detail := panel("Details", m.detailView(detailW-4), detailW, false)
		return lipgloss.JoinHorizontal(lipgloss.Top, list, strings.Repeat(" ", gap), detail)
	}

	list := panel("Presets", m.listView(width-4), width, true)
	detail := panel("Details", m.detailView(width-4), width, false)
	return lipgloss.JoinVertical(lipgloss.Left, list, detail)
}

func (m model) footerView(width int) string {
	logs := m.logView(width - 4)
	status := m.statusView()
	deps := m.dependencyView(width - 4)
	help := mutedStyle.Render("? help")
	if m.help {
		help = mutedStyle.Render(m.helpLine())
	}
	if m.crawling {
		status += "  " + activeBadgeStyle.Render("fetching crawl pages")
	}
	if m.err != "" {
		status += "\n" + errorStyle.Render(m.err)
	}
	body := strings.Join(nonEmpty([]string{status, deps, logs, help}), "\n")
	return panel("Status", body, width, false)
}

func (m model) listView(width int) string {
	if len(m.items) == 0 {
		if m.loading {
			return activeBadgeStyle.Render(" loading ") + "\n\n" + mutedStyle.Render("Fetching presets from musical-artifacts.com")
		}
		return mutedStyle.Render("No downloadable .gx files found.")
	}

	maxRows := m.height - 17
	if maxRows < 6 {
		maxRows = 6
	}
	if maxRows > len(m.items) {
		maxRows = len(m.items)
	}
	top := 0
	if m.selected >= maxRows {
		top = m.selected - maxRows + 1
	}

	var b strings.Builder
	for i := top; i < top+maxRows && i < len(m.items); i++ {
		item := m.items[i]
		cursor := " "
		meta := strings.Join(nonEmpty([]string{item.Author, item.Size, downloadsLabel(item.Downloads)}), " | ")
		nameWidth := max(18, width-33)
		line := fmt.Sprintf("%s %2d. %-*s %s", cursor, i+1, nameWidth, truncate(item.Name, nameWidth), truncate(meta, 28))
		if i == m.selected {
			line = selectedStyle.Width(width - 2).Render(strings.TrimRight(line, " "))
		} else {
			line = itemStyle.Render(line)
		}
		b.WriteString(line)
		b.WriteString("\n")
	}
	if top+maxRows < len(m.items) {
		fmt.Fprintf(&b, "%s\n", mutedStyle.Render(fmt.Sprintf("  ... %d more", len(m.items)-(top+maxRows))))
	}
	return b.String()
}

func (m model) detailView(width int) string {
	if m.loading {
		return activeBadgeStyle.Render("loading") + "\n\n" + mutedStyle.Render("Network request in progress.")
	}
	if m.err != "" {
		return errorStyle.Render(m.err)
	}
	if len(m.items) == 0 {
		return mutedStyle.Render("No preset selected.")
	}

	item := m.items[m.selected]
	rows := []string{
		accentStyle.Bold(true).Render(wordWrapLine(item.Name, width)),
		"",
		labelValue("Author", item.Author, width),
		labelValue("File", item.Filename, width),
		labelValue("Size", item.Size, width),
		labelValue("Downloads", item.Downloads, width),
	}
	if item.PageURL != "" {
		rows = append(rows, labelValue("Page", item.PageURL, width))
	}
	rows = append(rows, "", mutedStyle.Render("Description"))
	desc := item.Description
	if desc == "" {
		desc = "(No description available)"
	}
	for _, line := range wordWrap(desc, width, 8) {
		rows = append(rows, line)
	}
	return strings.Join(rows, "\n")
}

func (m model) audioView(width int, height int) string {
	if height < 10 {
		height = 10
	}
	bodyWidth := max(20, width-4)
	connectionTotalH := min(max(10, height/2), 15)
	if height < 22 {
		connectionTotalH = max(7, height/2)
	}
	connectionBodyH := max(4, connectionTotalH-3)
	connections := panel(
		"Connections",
		fitHeight(m.audioConnectionsView(bodyWidth, connectionBodyH), connectionBodyH),
		width,
		m.audio.Focus == audioFocusConnections || m.audio.PickingTarget,
	)

	meterTotalH := height - lipgloss.Height(connections)
	if meterTotalH < 8 {
		meterTotalH = 8
	}
	meterBodyH := max(4, meterTotalH-3)
	meter := panel(
		"Meter",
		fitHeight(m.audioMeterView(bodyWidth, meterBodyH), meterBodyH),
		width,
		m.audio.Focus == audioFocusMeter,
	)
	return fitHeight(lipgloss.JoinVertical(lipgloss.Left, connections, meter), height)
}

func (m model) audioConnectionsView(width int, height int) string {
	listRows := max(3, min(6, height/2))
	if width >= 84 {
		gap := 1
		sourceW := (width - gap) / 2
		targetW := width - sourceW - gap
		sources := m.audioNodeList(m.audio.Outputs, m.audio.OutSelected, sourceW, listRows, m.audio.Focus == audioFocusConnections && !m.audio.PickingTarget, true)
		targets := m.audioRouteTargetView(targetW, listRows)
		top := lipgloss.JoinHorizontal(lipgloss.Top, sources, strings.Repeat(" ", gap), targets)
		linksH := max(2, height-lipgloss.Height(top)-2)
		return strings.Join(nonEmpty([]string{
			top,
			accentStyle.Render("Routes"),
			m.audioRoutesList(width, linksH),
		}), "\n")
	}

	parts := []string{
		m.audioNodeList(m.audio.Outputs, m.audio.OutSelected, width, listRows, m.audio.Focus == audioFocusConnections && !m.audio.PickingTarget, true),
		m.audioRouteTargetView(width, listRows),
		accentStyle.Render("Routes"),
		m.audioRoutesList(width, max(2, height-(listRows*2)-3)),
	}
	return strings.Join(nonEmpty(parts), "\n")
}

func (m model) audioRouteTargetView(width int, maxRows int) string {
	if m.audio.PickingTarget {
		rows := []string{accentStyle.Render("Choose target")}
		list := m.audioTargetList(width, maxRows)
		rows = append(rows, list)
		rows = append(rows, mutedStyle.Render("space toggle  enter/c connect  x disconnect  esc close"))
		return strings.Join(rows, "\n")
	}

	out := m.audio.selectedOutput()
	if out.Name == "" {
		return mutedStyle.Render("Select a source.")
	}
	rows := []string{
		accentStyle.Render("Selected source"),
		labelValue("Source", out.Name, width),
	}
	targets := m.audio.linkedTargets(out)
	if len(targets) == 0 {
		rows = append(rows, mutedStyle.Render("No target connected."))
	} else {
		rows = append(rows, mutedStyle.Render("Targets"))
		for _, target := range targets {
			rows = append(rows, "  -> "+truncate(target, max(8, width-5)))
		}
	}
	rows = append(rows, mutedStyle.Render("enter opens target picker"))
	return strings.Join(rows, "\n")
}

func (m model) audioRoutesList(width int, maxRows int) string {
	if maxRows < 1 {
		maxRows = 1
	}
	var rows []string
	for _, source := range m.audio.Outputs {
		targets := m.audio.linkedTargets(source)
		if len(targets) == 0 {
			continue
		}
		line := source.Name + " -> " + strings.Join(targets, ", ")
		rows = append(rows, truncate(line, width))
	}
	if len(rows) == 0 {
		return mutedStyle.Render("No audio routes connected.")
	}
	if len(rows) > maxRows {
		rows = rows[:maxRows]
		rows[len(rows)-1] = mutedStyle.Render("... more routes")
	}
	return strings.Join(rows, "\n")
}

func (m model) audioMeterView(width int, height int) string {
	if width >= 84 {
		gap := 1
		listW := min(42, max(26, width/3))
		graphW := width - listW - gap
		list := m.audioMeterSourceList(listW, max(3, height), m.audio.Focus == audioFocusMeter)
		graph := m.audioSpectrumView(graphW, height)
		return lipgloss.JoinHorizontal(lipgloss.Top, list, strings.Repeat(" ", gap), graph)
	}

	listH := min(5, max(2, height/3))
	parts := []string{
		m.audioMeterSourceList(width, listH, m.audio.Focus == audioFocusMeter),
		m.audioSpectrumView(width, max(4, height-listH-1)),
	}
	return strings.Join(nonEmpty(parts), "\n")
}

func (m model) audioMeterSourceList(width int, maxRows int, focused bool) string {
	rows := []string{accentStyle.Render("Listen source")}
	rows = append(rows, m.audioNodeList(m.audio.Outputs, m.audio.MeterSelected, width, maxRows-1, focused, true))
	return strings.Join(rows, "\n")
}

func (m model) audioSpectrumView(width int, height int) string {
	source := m.audio.MeterSource
	if source == "" {
		source = m.audio.selectedMeterSourceName()
	}
	header := labelValue("Meter", source, width)
	status := ""
	if m.audio.MeterErr != "" {
		status = mutedStyle.Render("meter: " + truncate(m.audio.MeterErr, width))
	}
	barHeight := height - lipgloss.Height(header)
	if status != "" {
		barHeight -= lipgloss.Height(status)
	}
	if barHeight < spectrumRangeCount {
		barHeight = spectrumRangeCount
	}
	parts := []string{
		header,
		spectrumProgressView(m.audio.SpectrumShown, width, barHeight),
		status,
	}
	return fitHeight(strings.Join(nonEmpty(parts), "\n"), height)
}

func (m model) audioNodeList(nodes []AudioNode, selected int, width int, maxRows int, focused bool, output bool) string {
	if m.audio.Loading && len(nodes) == 0 {
		return activeBadgeStyle.Render(" loading ") + "\n\n" + mutedStyle.Render("Reading PipeWire ports with pw-link.")
	}
	if m.audio.Err != "" && len(nodes) == 0 {
		return errorStyle.Render(m.audio.Err)
	}
	if len(nodes) == 0 {
		return mutedStyle.Render("No nodes found.")
	}
	if maxRows < 1 {
		maxRows = 1
	}
	if maxRows > len(nodes) {
		maxRows = len(nodes)
	}
	top := 0
	if selected >= maxRows {
		top = selected - maxRows + 1
	}
	var b strings.Builder
	for i := top; i < top+maxRows && i < len(nodes); i++ {
		node := nodes[i]
		name := node.Name
		tag := fmt.Sprintf("%d port", len(node.Ports))
		if len(node.Ports) != 1 {
			tag += "s"
		}
		if output && isMidiName(node.Name) {
			tag += " midi"
		}
		line := fmt.Sprintf("%2d. %-*s %s", i+1, max(12, width-18), truncate(name, max(12, width-18)), mutedStyle.Render(tag))
		if i == selected {
			line = selectedStyle.Width(width - 2).Render(strings.TrimRight(line, " "))
		} else if focused {
			line = itemStyle.Render(line)
		} else {
			line = mutedStyle.Render(line)
		}
		b.WriteString(line)
		b.WriteString("\n")
	}
	return b.String()
}

func (m model) audioTargetList(width int, maxRows int) string {
	if len(m.audio.Inputs) == 0 {
		if m.audio.Loading {
			return activeBadgeStyle.Render(" loading ") + "\n\n" + mutedStyle.Render("Reading PipeWire ports with pw-link.")
		}
		if m.audio.Err != "" {
			return errorStyle.Render(m.audio.Err)
		}
		return mutedStyle.Render("No targets found.")
	}
	if maxRows < 1 {
		maxRows = 1
	}
	if maxRows > len(m.audio.Inputs) {
		maxRows = len(m.audio.Inputs)
	}
	top := 0
	if m.audio.InSelected >= maxRows {
		top = m.audio.InSelected - maxRows + 1
	}
	out := m.audio.selectedOutput()
	var b strings.Builder
	for i := top; i < top+maxRows && i < len(m.audio.Inputs); i++ {
		node := m.audio.Inputs[i]
		mark := "[ ]"
		if out.Name != "" && m.audio.nodesConnected(out, node) {
			mark = "[x]"
		}
		tag := fmt.Sprintf("%d port", len(node.Ports))
		if len(node.Ports) != 1 {
			tag += "s"
		}
		nameWidth := max(10, width-24)
		line := fmt.Sprintf("%s %2d. %-*s %s", mark, i+1, nameWidth, truncate(node.Name, nameWidth), mutedStyle.Render(tag))
		if i == m.audio.InSelected {
			line = selectedStyle.Width(width - 2).Render(strings.TrimRight(line, " "))
		} else {
			line = itemStyle.Render(line)
		}
		b.WriteString(line)
		b.WriteString("\n")
	}
	return b.String()
}

func (m model) guitarixView(width int) string {
	if width >= 104 {
		gap := 1
		bankW := int(float64(width-gap) * 0.38)
		presetW := width - bankW - gap
		banks := panel("Banks", m.guitarixList(m.guitarx.Banks, m.guitarx.BankSelected, bankW-4, m.guitarx.Focus == 0), bankW, m.guitarx.Focus == 0)
		presets := panel("Presets", m.guitarixList(m.guitarx.Presets, m.guitarx.PresetSelected, presetW-4, m.guitarx.Focus == 1), presetW, m.guitarx.Focus == 1)
		main := lipgloss.JoinHorizontal(lipgloss.Top, banks, strings.Repeat(" ", gap), presets)
		return lipgloss.JoinVertical(lipgloss.Left, main, panel("Guitarix RPC", m.guitarixDetailView(width-4), width, false))
	}
	return lipgloss.JoinVertical(
		lipgloss.Left,
		panel("Banks", m.guitarixList(m.guitarx.Banks, m.guitarx.BankSelected, width-4, m.guitarx.Focus == 0), width, m.guitarx.Focus == 0),
		panel("Presets", m.guitarixList(m.guitarx.Presets, m.guitarx.PresetSelected, width-4, m.guitarx.Focus == 1), width, m.guitarx.Focus == 1),
		panel("Guitarix RPC", m.guitarixDetailView(width-4), width, false),
	)
}

func (m model) guitarixList(items []string, selected int, width int, focused bool) string {
	if m.guitarx.Loading && len(items) == 0 {
		return activeBadgeStyle.Render(" loading ") + "\n\n" + mutedStyle.Render("Querying 127.0.0.1:7000")
	}
	if m.guitarx.Err != "" && len(items) == 0 {
		return errorStyle.Render(m.guitarx.Err)
	}
	if len(items) == 0 {
		return mutedStyle.Render("No entries.")
	}
	maxRows := m.height - 20
	if maxRows < 6 {
		maxRows = 6
	}
	if maxRows > len(items) {
		maxRows = len(items)
	}
	top := 0
	if selected >= maxRows {
		top = selected - maxRows + 1
	}
	var b strings.Builder
	for i := top; i < top+maxRows && i < len(items); i++ {
		line := fmt.Sprintf("%2d. %s", i+1, truncate(items[i], width-6))
		if i == selected {
			line = selectedStyle.Width(width - 2).Render(line)
		} else if focused {
			line = itemStyle.Render(line)
		} else {
			line = mutedStyle.Render(line)
		}
		b.WriteString(line)
		b.WriteString("\n")
	}
	return b.String()
}

func (m model) guitarixDetailView(width int) string {
	if m.guitarx.Err != "" {
		return errorStyle.Render(m.guitarx.Err) + "\n\n" + mutedStyle.Render("Auto-start command: PIPEWIRE_LATENCY=128/48000 pw-jack guitarix -N -p 7000")
	}
	bank := m.guitarx.selectedBank()
	preset := m.guitarx.selectedPreset()
	if m.guitarx.ConfirmDelete {
		return strings.Join([]string{
			errorStyle.Render("Delete bank?"),
			"",
			labelValue("Bank", bank, width),
			labelValue("Dir", m.downloader.dest, width),
			"",
			mutedStyle.Render("Press y/enter to delete the .gx bank file, n/esc to cancel."),
		}, "\n")
	}
	rows := []string{
		labelValue("RPC", "127.0.0.1:7000", width),
		labelValue("Bank", bank, width),
		labelValue("Preset", preset, width),
	}
	if m.guitarx.CurrentPreset != "" {
		rows = append(rows, labelValue("Loaded", m.guitarx.CurrentBank+" / "+m.guitarx.CurrentPreset, width))
	}
	rows = append(rows, "", mutedStyle.Render("Guitarix auto-starts if RPC is not reachable. enter/s loads the selected preset."))
	return strings.Join(rows, "\n")
}

func (m model) statusView() string {
	stats := m.downloader.Stats()
	parts := []string{
		badgeStyle.Render(fmt.Sprintf("queued %d", stats.Queued)),
		activeBadgeStyle.Render(fmt.Sprintf("active %d", stats.Active)),
		badgeStyle.Render(fmt.Sprintf("pending %d", stats.Pending)),
		successStyle.Render(fmt.Sprintf("done %d", stats.Done)),
		mutedStyle.Render(fmt.Sprintf("skipped %d", stats.Skipped)),
	}
	if stats.Failed > 0 {
		parts = append(parts, errorStyle.Render(fmt.Sprintf("failed %d", stats.Failed)))
	} else {
		parts = append(parts, mutedStyle.Render("failed 0"))
	}
	return strings.Join(parts, " ")
}

func (m model) dependencyView(width int) string {
	if len(m.deps.Missing) == 0 {
		return ""
	}
	missing := strings.Join(m.deps.MissingCommands(), ", ")
	command := m.deps.InstallCommand()
	if command == "" {
		return errorStyle.Render("missing system deps: " + truncate(missing, width))
	}
	return strings.Join([]string{
		errorStyle.Render("missing system deps: " + truncate(missing, width)),
		accentStyle.Render("install: " + truncate(command, width)),
	}, "\n")
}

func (m model) logView(width int) string {
	lines := tail(m.log, 2)
	if len(lines) == 0 {
		return mutedStyle.Render("No download activity yet.")
	}
	var rendered []string
	for _, line := range lines {
		style := mutedStyle
		switch {
		case strings.HasPrefix(line, "saved:"):
			style = successStyle
		case strings.HasPrefix(line, "failed:"), strings.HasPrefix(line, "crawl failed:"):
			style = errorStyle
		case strings.HasPrefix(line, "queued:"), strings.HasPrefix(line, "crawl queued"):
			style = accentStyle
		}
		rendered = append(rendered, style.Render(truncate(line, width)))
	}
	return strings.Join(rendered, "\n")
}

func (m *model) addLog(line string) {
	m.log = append(m.log, line)
	if len(m.log) > 80 {
		m.log = m.log[len(m.log)-80:]
	}
}

func fetchCmd(client *http.Client, query, order string, page int) tea.Cmd {
	return func() tea.Msg {
		items, rawURL, err := FetchArtifacts(client, query, order, page)
		return fetchMsg{items: items, rawURL: rawURL, query: query, order: order, page: page, err: err}
	}
}

func listenDownloadEvents(events <-chan downloadEvent) tea.Cmd {
	return func() tea.Msg {
		event, ok := <-events
		if !ok {
			return nil
		}
		return event
	}
}

func waitDownloadsCmd(d *Downloader) tea.Cmd {
	return func() tea.Msg {
		d.Wait()
		return downloadsDoneMsg{}
	}
}

func requiredSystemDependencies() []SystemDependency {
	return []SystemDependency{
		{Command: "pw-link", Package: "pipewire-bin", Use: "PipeWire routing"},
		{Command: "pw-cat", Package: "pipewire-bin", Use: "audio meter capture"},
		{Command: "pw-jack", Package: "pipewire-jack", Use: "launch Guitarix through PipeWire JACK"},
		{Command: "wireplumber", Package: "pipewire-audio", Use: "PipeWire audio session manager"},
		{Command: "guitarix", Package: "guitarix", Use: "amp/effects engine and preset RPC"},
	}
}

func checkSystemDependencies() SystemDependencyStatus {
	var status SystemDependencyStatus
	for _, dep := range requiredSystemDependencies() {
		if _, err := commandPath(dep.Command); err != nil {
			status.Missing = append(status.Missing, dep)
		}
	}
	return status
}

func (s SystemDependencyStatus) MissingCommands() []string {
	out := make([]string, 0, len(s.Missing))
	for _, dep := range s.Missing {
		out = append(out, dep.Command)
	}
	return out
}

func (s SystemDependencyStatus) MissingPackages() []string {
	seen := make(map[string]bool)
	var packages []string
	for _, dep := range s.Missing {
		if dep.Package == "" || seen[dep.Package] {
			continue
		}
		seen[dep.Package] = true
		packages = append(packages, dep.Package)
	}
	return packages
}

func (s SystemDependencyStatus) InstallCommand() string {
	packages := s.MissingPackages()
	if len(packages) == 0 {
		return ""
	}
	return "sudo apt update && sudo apt install -y " + strings.Join(packages, " ")
}

func printSystemDependencies(status SystemDependencyStatus) {
	if len(status.Missing) == 0 {
		fmt.Println("All system dependencies are installed.")
		return
	}
	fmt.Println("Missing system dependencies:")
	for _, dep := range status.Missing {
		fmt.Printf("- %s (%s): %s\n", dep.Command, dep.Package, dep.Use)
	}
	fmt.Println()
	fmt.Println(status.InstallCommand())
}

func crawlCmd(client *http.Client, downloader *Downloader, query, order string, pages int) tea.Cmd {
	return func() tea.Msg {
		count := 0
		for page := 1; page <= pages; page++ {
			items, _, err := FetchArtifacts(client, query, order, page)
			if err != nil {
				return crawlDoneMsg{pages: page, count: count, err: err}
			}
			if len(items) == 0 {
				return crawlDoneMsg{pages: page - 1, count: count}
			}
			for _, item := range items {
				if downloader.Enqueue(item) {
					count++
				}
			}
		}
		return crawlDoneMsg{pages: pages, count: count}
	}
}

func audioRefreshCmd() tea.Cmd {
	return func() tea.Msg {
		outputPorts, err := pipewirePorts("-o")
		if err != nil {
			return audioMsg{err: err}
		}
		inputPorts, err := pipewirePorts("-i")
		if err != nil {
			return audioMsg{err: err}
		}
		links, _ := pipewireLinks()
		return audioMsg{
			outputs: groupPorts(outputPorts),
			inputs:  groupPorts(inputPorts),
			links:   links,
		}
	}
}

func audioConnectCmd(out AudioNode, in AudioNode) tea.Cmd {
	return func() tea.Msg {
		pairs := pairPorts(out.Ports, in.Ports)
		if len(pairs) == 0 {
			return audioActionMsg{action: "connect", err: errors.New("no compatible ports")}
		}
		var errs []string
		for _, pair := range pairs {
			if err := runPipewireLink(pair[0], pair[1], false); err != nil {
				errs = append(errs, err.Error())
			}
		}
		if len(errs) > 0 {
			return audioActionMsg{action: "connect", err: errors.New(strings.Join(errs, "; "))}
		}
		return audioActionMsg{action: "connect"}
	}
}

func audioDisconnectCmd(out AudioNode, in AudioNode) tea.Cmd {
	return func() tea.Msg {
		pairs := pairPorts(out.Ports, in.Ports)
		if len(pairs) == 0 {
			return audioActionMsg{action: "disconnect", err: errors.New("no compatible ports")}
		}
		var errs []string
		for _, pair := range pairs {
			if err := runPipewireLink(pair[0], pair[1], true); err != nil {
				errs = append(errs, err.Error())
			}
		}
		if len(errs) > 0 {
			return audioActionMsg{action: "disconnect", err: errors.New(strings.Join(errs, "; "))}
		}
		return audioActionMsg{action: "disconnect"}
	}
}

func startMeterStream(id int, source string) *MeterStream {
	ctx, cancel := context.WithCancel(context.Background())
	stream := &MeterStream{
		id:     id,
		source: source,
		events: make(chan meterMsg, 4),
		cancel: cancel,
	}
	go runMeterStream(ctx, stream)
	return stream
}

func listenMeterStream(stream *MeterStream) tea.Cmd {
	if stream == nil {
		return nil
	}
	id := stream.id
	source := stream.source
	events := stream.events
	return func() tea.Msg {
		msg, ok := <-events
		if !ok {
			return meterStoppedMsg{streamID: id, source: source}
		}
		return msg
	}
}

func runMeterStream(ctx context.Context, stream *MeterStream) {
	defer close(stream.events)

	path, err := commandPath("pw-cat")
	if err != nil {
		publishMeterMsg(ctx, stream.events, meterMsg{streamID: stream.id, source: stream.source, err: err})
		return
	}

	cmd := exec.CommandContext(ctx, path,
		"--record",
		"--raw",
		"--target", stream.source,
		"--rate", strconv.Itoa(meterSampleRate),
		"--channels", "1",
		"--format", "s16",
		"-",
	)
	cmd.Stderr = io.Discard

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		publishMeterMsg(ctx, stream.events, meterMsg{streamID: stream.id, source: stream.source, err: err})
		return
	}
	if err := cmd.Start(); err != nil {
		publishMeterMsg(ctx, stream.events, meterMsg{streamID: stream.id, source: stream.source, err: err})
		return
	}
	defer func() {
		if cmd.Process != nil {
			_ = cmd.Process.Kill()
		}
		_ = cmd.Wait()
	}()

	buf := make([]byte, meterFrameSamples*2)
	for {
		n, err := io.ReadFull(stdout, buf)
		if n > 0 {
			spectrum, calcErr := spectrumFromPCM(buf[:n], meterSpectrumBands, meterSampleRate)
			if calcErr != nil {
				if !publishMeterMsg(ctx, stream.events, meterMsg{streamID: stream.id, source: stream.source, err: calcErr}) {
					return
				}
			} else if !publishMeterMsg(ctx, stream.events, meterMsg{streamID: stream.id, source: stream.source, spectrum: spectrum}) {
				return
			}
		}
		if err != nil {
			if ctx.Err() != nil {
				return
			}
			if !errors.Is(err, io.ErrUnexpectedEOF) || n == 0 {
				publishMeterMsg(ctx, stream.events, meterMsg{streamID: stream.id, source: stream.source, err: err})
				return
			}
		}
	}
}

func publishMeterMsg(ctx context.Context, events chan meterMsg, msg meterMsg) bool {
	select {
	case events <- msg:
		return true
	case <-ctx.Done():
		return false
	default:
	}
	select {
	case <-events:
	default:
	}
	select {
	case events <- msg:
		return true
	case <-ctx.Done():
		return false
	default:
		return true
	}
}

func guitarixRefreshCmd(preferredBank string) tea.Cmd {
	return func() tea.Msg {
		banks, err := guitarixBanks()
		if err != nil {
			return guitarixMsg{err: err}
		}
		if len(banks) == 0 {
			return guitarixMsg{banks: banks, err: errors.New("no Guitarix banks returned")}
		}
		bank := preferredBank
		if bank == "" || !containsString(banks, bank) {
			bank = banks[0]
		}
		presets, err := guitarixPresets(bank)
		if err != nil {
			return guitarixMsg{banks: banks, err: err}
		}
		return guitarixMsg{banks: banks, presets: presets}
	}
}

func guitarixSetPresetCmd(bank, preset string) tea.Cmd {
	return func() tea.Msg {
		err := guitarixSetPreset(bank, preset)
		return guitarixPresetMsg{bank: bank, preset: preset, err: err}
	}
}

func guitarixDeleteBankCmd(bank, dir string) tea.Cmd {
	return func() tea.Msg {
		path, warn, err := deleteGuitarixBank(bank, dir)
		return guitarixBankDeleteMsg{bank: bank, path: path, warn: warn, err: err}
	}
}

func FetchArtifacts(client *http.Client, query, order string, page int) ([]Artifact, string, error) {
	rawURL := searchURL(query, order, page)
	body, err := fetchString(client, rawURL)
	if err != nil {
		return nil, rawURL, err
	}
	items := ParseArtifacts(body)
	return items, rawURL, nil
}

func searchURL(query, order string, page int) string {
	u, _ := url.Parse(baseURL + "/artifacts")
	q := u.Query()
	q.Set("apps", "guitarix")
	query = strings.TrimSpace(query)
	if query != "" {
		q.Set("q", query)
	}
	if order != "" {
		q.Set("order", order)
	}
	if page > 1 {
		q.Set("page", strconv.Itoa(page))
	}
	u.RawQuery = q.Encode()
	return u.String()
}

func ParseArtifacts(body string) []Artifact {
	var items []Artifact
	start := 0
	needle := "artifact-item col-sm-12"
	for {
		i := strings.Index(body[start:], needle)
		if i < 0 {
			break
		}
		i += start
		next := strings.Index(body[i+len(needle):], needle)
		end := len(body)
		if next >= 0 {
			end = i + len(needle) + next
		}
		block := body[i:end]
		if artifact, ok := parseArtifactBlock(block); ok {
			items = append(items, artifact)
		}
		start = end
	}
	return items
}

func parseArtifactBlock(block string) (Artifact, bool) {
	a := Artifact{}

	btn := strings.Index(block, "btn-download")
	if btn < 0 {
		return a, false
	}
	tagStart := strings.LastIndex(block[:btn], "<a ")
	if tagStart < 0 {
		return a, false
	}
	tagEndRel := strings.Index(block[btn:], ">")
	if tagEndRel < 0 {
		return a, false
	}
	tagEnd := btn + tagEndRel
	tag := block[tagStart : tagEnd+1]

	a.DownloadURL = absolutize(getAttr(tag, "href"))
	a.Filename = cleanFilename(firstNonEmpty(getAttr(tag, "download"), getAttr(tag, "title"), pathBaseFromURL(a.DownloadURL)))
	if a.DownloadURL == "" || a.Filename == "" || !strings.HasSuffix(strings.ToLower(a.Filename), ".gx") {
		return a, false
	}

	if m := nameRe.FindStringSubmatch(block); len(m) == 3 {
		a.ID = m[1]
		a.Name = cleanText(m[2])
	}
	if a.ID == "" {
		if m := idInURLRe.FindStringSubmatch(a.DownloadURL); len(m) == 2 {
			a.ID = m[1]
		}
	}
	if a.Name == "" {
		a.Name = strings.TrimSuffix(a.Filename, filepath.Ext(a.Filename))
	}
	if a.ID != "" {
		a.PageURL = baseURL + "/artifacts/" + a.ID
	}

	if m := authorRe.FindStringSubmatch(block); len(m) == 2 {
		a.Author = cleanText(m[1])
	}
	if m := descRe.FindStringSubmatch(block); len(m) == 2 {
		a.Description = cleanText(m[1])
	}

	anchorEnd := strings.Index(block[tagEnd+1:], "</a>")
	if anchorEnd >= 0 {
		inner := block[tagEnd+1 : tagEnd+1+anchorEnd]
		txt := cleanText(inner)
		if m := sizeRe.FindStringSubmatch(txt); len(m) == 2 {
			a.Size = cleanText(m[1])
		}
		if m := countRe.FindString(txt); m != "" {
			a.Downloads = strings.TrimSpace(m)
		}
	}
	return a, true
}

func fetchArtifactByIDOrURL(client *http.Client, idOrURL string) (Artifact, error) {
	raw := strings.TrimSpace(idOrURL)
	if raw == "" {
		return Artifact{}, errors.New("empty artifact id/url")
	}
	if strings.HasPrefix(raw, "http://") || strings.HasPrefix(raw, "https://") {
		if strings.HasSuffix(strings.ToLower(raw), ".gx") {
			return Artifact{
				Name:        strings.TrimSuffix(pathBaseFromURL(raw), ".gx"),
				Filename:    cleanFilename(pathBaseFromURL(raw)),
				DownloadURL: raw,
			}, nil
		}
	} else if _, err := strconv.Atoi(raw); err == nil {
		raw = baseURL + "/artifacts/" + raw
	} else {
		return Artifact{}, fmt.Errorf("not an artifact id or .gx URL: %s", idOrURL)
	}

	body, err := fetchString(client, raw)
	if err != nil {
		return Artifact{}, err
	}
	a, ok := parseDetailPage(body, raw)
	if !ok {
		return Artifact{}, fmt.Errorf("no downloadable .gx found on %s", raw)
	}
	return a, nil
}

func parseDetailPage(body, pageURL string) (Artifact, bool) {
	a, ok := parseArtifactBlock(body)
	if !ok {
		return Artifact{}, false
	}
	if m := h1NameRe.FindStringSubmatch(body); len(m) == 2 {
		a.Name = cleanText(m[1])
	}
	a.PageURL = pageURL
	if a.ID == "" {
		if m := idInURLRe.FindStringSubmatch(pageURL); len(m) == 2 {
			a.ID = m[1]
		}
	}
	return a, true
}

func fetchString(client *http.Client, rawURL string) (string, error) {
	req, err := http.NewRequest(http.MethodGet, rawURL, nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("User-Agent", "gxpreset/0.2 (+https://musical-artifacts.com)")
	req.Header.Set("Accept", "text/html,application/xhtml+xml")

	resp, err := client.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return "", fmt.Errorf("%s returned %s", rawURL, resp.Status)
	}
	data, err := io.ReadAll(resp.Body)
	if err != nil {
		return "", err
	}
	return string(data), nil
}

func NewDownloader(client *http.Client, dest string, workers int, force bool, events chan downloadEvent) *Downloader {
	d := &Downloader{
		client: client,
		dest:   dest,
		force:  force,
		queue:  make(chan Artifact, 256),
		events: events,
		seen:   make(map[string]bool),
	}
	for i := 0; i < workers; i++ {
		go d.worker()
	}
	return d
}

func (d *Downloader) Enqueue(a Artifact) bool {
	key := firstNonEmpty(a.DownloadURL, a.Filename)
	if key == "" {
		return false
	}
	d.mu.Lock()
	if d.seen[key] {
		d.mu.Unlock()
		d.emit(downloadEvent{kind: "duplicate", artifact: a})
		return false
	}
	d.seen[key] = true
	d.queued++
	d.wg.Add(1)
	d.mu.Unlock()

	d.queue <- a
	d.emit(downloadEvent{kind: "queued", artifact: a})
	return true
}

func (d *Downloader) Wait() {
	d.wg.Wait()
}

func (d *Downloader) Stop() {
	d.stop.Do(func() {
		close(d.queue)
	})
}

func (d *Downloader) HasWork() bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.active > 0 || d.queued > d.done+d.failed+d.skipped
}

func (d *Downloader) Status() string {
	stats := d.Stats()
	return fmt.Sprintf("downloads queued=%d active=%d pending=%d done=%d skipped=%d failed=%d",
		stats.Queued, stats.Active, stats.Pending, stats.Done, stats.Skipped, stats.Failed)
}

func (d *Downloader) Stats() DownloaderStats {
	d.mu.Lock()
	defer d.mu.Unlock()
	pending := d.queued - d.done - d.failed - d.skipped - d.active
	if pending < 0 {
		pending = 0
	}
	return DownloaderStats{
		Queued:  d.queued,
		Active:  d.active,
		Pending: pending,
		Done:    d.done,
		Failed:  d.failed,
		Skipped: d.skipped,
	}
}

func (d *Downloader) eventsChan() <-chan downloadEvent {
	return d.events
}

func (d *Downloader) worker() {
	for a := range d.queue {
		d.mu.Lock()
		d.active++
		d.mu.Unlock()

		result, skipped, err := d.download(a)

		d.mu.Lock()
		d.active--
		switch {
		case err != nil:
			d.failed++
		case skipped:
			d.skipped++
		default:
			d.done++
		}
		d.mu.Unlock()

		switch {
		case err != nil:
			d.emit(downloadEvent{kind: "failed", artifact: a, err: err})
		case skipped:
			d.emit(downloadEvent{kind: "exists", artifact: a, path: result})
		default:
			d.emit(downloadEvent{kind: "saved", artifact: a, path: result})
		}
		d.wg.Done()
	}
}

func (d *Downloader) download(a Artifact) (string, bool, error) {
	if a.DownloadURL == "" {
		return "", false, errors.New("missing download URL")
	}
	if a.Filename == "" {
		a.Filename = cleanFilename(pathBaseFromURL(a.DownloadURL))
	}
	if a.Filename == "" || !strings.HasSuffix(strings.ToLower(a.Filename), ".gx") {
		return "", false, fmt.Errorf("not a .gx filename: %s", a.Filename)
	}
	if err := os.MkdirAll(d.dest, 0755); err != nil {
		return "", false, err
	}
	out := filepath.Join(d.dest, a.Filename)
	if !d.force {
		if _, err := os.Stat(out); err == nil {
			return out, true, nil
		}
	}

	req, err := http.NewRequest(http.MethodGet, a.DownloadURL, nil)
	if err != nil {
		return "", false, err
	}
	req.Header.Set("User-Agent", "gxpreset/0.2 (+https://musical-artifacts.com)")
	resp, err := d.client.Do(req)
	if err != nil {
		return "", false, err
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return "", false, fmt.Errorf("download returned %s", resp.Status)
	}

	tmp := out + ".part"
	f, err := os.Create(tmp)
	if err != nil {
		return "", false, err
	}
	_, copyErr := io.Copy(f, resp.Body)
	closeErr := f.Close()
	if copyErr != nil {
		_ = os.Remove(tmp)
		return "", false, copyErr
	}
	if closeErr != nil {
		_ = os.Remove(tmp)
		return "", false, closeErr
	}
	if err := os.Rename(tmp, out); err != nil {
		_ = os.Remove(tmp)
		return "", false, err
	}
	return out, false, nil
}

func (d *Downloader) emit(event downloadEvent) {
	select {
	case d.events <- event:
	default:
	}
}

func installAllVisible(client *http.Client, dest string, workers int, force bool, query, order string, page int) error {
	items, rawURL, err := FetchArtifacts(client, query, order, page)
	if err != nil {
		return err
	}
	fmt.Printf("%s\n%d .gx file(s)\n", rawURL, len(items))
	events := make(chan downloadEvent, 512)
	downloader := NewDownloader(client, dest, workers, force, events)
	defer downloader.Stop()
	for _, item := range items {
		downloader.Enqueue(item)
	}
	done := make(chan struct{})
	go func() {
		downloader.Wait()
		close(done)
	}()
	for {
		select {
		case event := <-events:
			fmt.Println(formatDownloadEvent(event))
		case <-done:
			fmt.Println(downloader.Status())
			return nil
		}
	}
}

func formatDownloadEvent(event downloadEvent) string {
	name := firstNonEmpty(event.artifact.Filename, event.artifact.Name, event.artifact.DownloadURL)
	switch event.kind {
	case "queued":
		return "queued: " + name
	case "duplicate":
		return "already queued: " + name
	case "saved":
		return "saved: " + event.path
	case "exists":
		return "exists: " + event.path
	case "failed":
		return fmt.Sprintf("failed: %s: %v", name, event.err)
	default:
		return event.kind + ": " + name
	}
}

func pipewirePorts(direction string) ([]string, error) {
	out, err := commandOutput("pw-link", direction)
	if err != nil {
		return nil, err
	}
	lines := splitCleanLines(string(out))
	lines = filterAudioPorts(lines)
	sort.Strings(lines)
	return lines, nil
}

func pipewireLinks() (map[string]map[string]bool, error) {
	out, err := commandOutput("pw-link", "-l")
	if err != nil {
		return make(map[string]map[string]bool), err
	}
	links := make(map[string]map[string]bool)
	var current string
	for _, raw := range strings.Split(string(out), "\n") {
		line := strings.TrimSpace(raw)
		if line == "" {
			continue
		}
		if strings.Contains(line, "|->") {
			if current == "" {
				continue
			}
			target := strings.TrimSpace(strings.TrimPrefix(line[strings.Index(line, "|->"):], "|->"))
			if target == "" {
				continue
			}
			if links[current] == nil {
				links[current] = make(map[string]bool)
			}
			links[current][target] = true
			continue
		}
		current = line
	}
	return links, nil
}

func runPipewireLink(outPort, inPort string, disconnect bool) error {
	args := []string{outPort, inPort}
	if disconnect {
		args = []string{"-d", outPort, inPort}
	}
	_, err := commandOutput("pw-link", args...)
	return err
}

func groupPorts(ports []string) []AudioNode {
	grouped := make(map[string][]string)
	for _, port := range ports {
		node := nodeName(port)
		grouped[node] = append(grouped[node], port)
	}
	var names []string
	for name := range grouped {
		names = append(names, name)
	}
	sort.Strings(names)
	nodes := make([]AudioNode, 0, len(names))
	for _, name := range names {
		sort.Strings(grouped[name])
		nodes = append(nodes, AudioNode{Name: name, Ports: grouped[name]})
	}
	return nodes
}

func nodeName(port string) string {
	if i := strings.LastIndex(port, ":"); i > 0 {
		return port[:i]
	}
	return port
}

func pairPorts(outputs, inputs []string) [][2]string {
	if len(outputs) == 0 || len(inputs) == 0 {
		return nil
	}
	inByChan := make(map[string]string)
	for _, in := range inputs {
		if ch := channelKey(in); ch != "" {
			inByChan[ch] = in
		}
	}
	var pairs [][2]string
	usedIn := make(map[string]bool)
	for _, out := range outputs {
		ch := channelKey(out)
		if ch == "" {
			continue
		}
		if in, ok := inByChan[ch]; ok {
			pairs = append(pairs, [2]string{out, in})
			usedIn[in] = true
		}
	}
	if len(pairs) > 0 {
		return pairs
	}
	if len(outputs) == 1 {
		for _, in := range inputs {
			pairs = append(pairs, [2]string{outputs[0], in})
		}
		return pairs
	}
	if len(inputs) == 1 {
		for _, out := range outputs {
			pairs = append(pairs, [2]string{out, inputs[0]})
		}
		return pairs
	}
	limit := min(len(outputs), len(inputs))
	for i := 0; i < limit; i++ {
		if !usedIn[inputs[i]] {
			pairs = append(pairs, [2]string{outputs[i], inputs[i]})
		}
	}
	return pairs
}

func channelKey(port string) string {
	name := port
	if i := strings.LastIndex(name, ":"); i >= 0 {
		name = name[i+1:]
	}
	name = strings.ToUpper(name)
	for _, ch := range []string{"FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR", "MONO"} {
		if name == ch || strings.HasSuffix(name, "_"+ch) || strings.HasSuffix(name, "-"+ch) {
			return ch
		}
	}
	return ""
}

func spectrumFromPCM(data []byte, bands int, sampleRate int) ([]float64, error) {
	if bands < 8 {
		bands = 8
	}
	if len(data) < 2 {
		return nil, errors.New("no PCM data")
	}
	samples := make([]float64, 0, len(data)/2)
	var mean float64
	for i := 0; i+1 < len(data); i += 2 {
		v := int16(binary.LittleEndian.Uint16(data[i : i+2]))
		f := float64(v) / 32768.0
		samples = append(samples, f)
		mean += f
	}
	if len(samples) == 0 {
		return nil, errors.New("no PCM samples")
	}
	mean /= float64(len(samples))
	for i := range samples {
		window := 1.0
		if len(samples) > 1 {
			window = 0.5 - 0.5*math.Cos(2*math.Pi*float64(i)/float64(len(samples)-1))
		}
		samples[i] = (samples[i] - mean) * window
	}

	spectrum := make([]float64, bands)
	minFreq := 55.0
	maxFreq := 12000.0
	for band := 0; band < bands; band++ {
		t := float64(band) / float64(max(1, bands-1))
		freq := minFreq * math.Pow(maxFreq/minFreq, t)
		center := int(math.Round(freq * float64(len(samples)) / float64(sampleRate)))
		if center < 1 {
			center = 1
		}
		maxPower := 0.0
		for k := center - 1; k <= center+1; k++ {
			if k <= 0 || k >= len(samples)/2 {
				continue
			}
			if p := goertzelPower(samples, k); p > maxPower {
				maxPower = p
			}
		}
		amp := math.Sqrt(maxPower) * 2 / float64(len(samples))
		db := 20 * math.Log10(amp+1e-7)
		level := (db + 72) / 54
		if level < 0 {
			level = 0
		}
		if level > 1 {
			level = 1
		}
		spectrum[band] = math.Sqrt(level)
	}
	return smoothSpectrum(spectrum), nil
}

func goertzelPower(samples []float64, k int) float64 {
	w := 2 * math.Pi * float64(k) / float64(len(samples))
	coeff := 2 * math.Cos(w)
	var q0, q1, q2 float64
	for _, sample := range samples {
		q0 = coeff*q1 - q2 + sample
		q2 = q1
		q1 = q0
	}
	return q1*q1 + q2*q2 - coeff*q1*q2
}

func smoothSpectrum(values []float64) []float64 {
	if len(values) < 3 {
		return values
	}
	out := make([]float64, len(values))
	for i := range values {
		sum := values[i] * 0.5
		weight := 0.5
		if i > 0 {
			sum += values[i-1] * 0.25
			weight += 0.25
		}
		if i < len(values)-1 {
			sum += values[i+1] * 0.25
			weight += 0.25
		}
		out[i] = sum / weight
	}
	return out
}

func guitarixBanks() ([]string, error) {
	raw, err := guitarixCall("banks", []any{})
	if err != nil {
		return nil, err
	}
	names, err := extractNames(raw)
	if err != nil {
		return nil, err
	}
	return uniqueStrings(names), nil
}

func guitarixPresets(bank string) ([]string, error) {
	raw, err := guitarixCall("presets", []any{bank})
	if err != nil {
		return nil, err
	}
	var presets []string
	if err := json.Unmarshal(raw, &presets); err == nil {
		return presets, nil
	}
	names, err := extractNames(raw)
	if err != nil {
		return nil, err
	}
	return uniqueStrings(names), nil
}

func guitarixSetPreset(bank, preset string) error {
	return guitarixNotify("setpreset", []any{bank, preset})
}

func deleteGuitarixBank(bank, dir string) (string, string, error) {
	path, err := resolveGuitarixBankFile(bank, dir)
	if err != nil {
		return "", "", err
	}
	if err := os.Remove(path); err != nil {
		return path, "", err
	}
	if err := removeGuitarixBankListEntry(dir, bank, filepath.Base(path)); err != nil {
		return path, "banklist.js update failed: " + err.Error(), nil
	}
	return path, "", nil
}

func resolveGuitarixBankFile(bank, dir string) (string, error) {
	bank = strings.TrimSpace(bank)
	if bank == "" {
		return "", errors.New("empty bank name")
	}
	entries, _ := readGuitarixBankList(dir)
	for _, entry := range entries {
		if bankEntryMatches(entry, bank, "") {
			return safeBankFilePath(dir, entry.File)
		}
	}

	candidates := []string{
		bank + ".gx",
		cleanFilename(bank) + ".gx",
		strings.ReplaceAll(bank, " ", "_") + ".gx",
	}
	for _, candidate := range candidates {
		path, err := safeBankFilePath(dir, candidate)
		if err == nil {
			if st, statErr := os.Stat(path); statErr == nil && !st.IsDir() {
				return path, nil
			}
		}
	}

	files, err := filepath.Glob(filepath.Join(dir, "*.gx"))
	if err != nil {
		return "", err
	}
	key := normalizedBankKey(bank)
	for _, path := range files {
		stem := strings.TrimSuffix(filepath.Base(path), filepath.Ext(path))
		if normalizedBankKey(stem) == key {
			return path, nil
		}
	}
	return "", fmt.Errorf("bank file not found for %q in %s", bank, dir)
}

type guitarixBankListEntry struct {
	Name string
	File string
}

func readGuitarixBankList(dir string) ([]guitarixBankListEntry, error) {
	data, err := os.ReadFile(filepath.Join(dir, "banklist.js"))
	if err != nil {
		return nil, err
	}
	var rows [][]json.RawMessage
	if err := json.Unmarshal(data, &rows); err != nil {
		return nil, err
	}
	entries := make([]guitarixBankListEntry, 0, len(rows))
	for _, row := range rows {
		if len(row) < 2 {
			continue
		}
		var entry guitarixBankListEntry
		_ = json.Unmarshal(row[0], &entry.Name)
		_ = json.Unmarshal(row[1], &entry.File)
		if entry.Name != "" || entry.File != "" {
			entries = append(entries, entry)
		}
	}
	return entries, nil
}

func removeGuitarixBankListEntry(dir, bank, filename string) error {
	path := filepath.Join(dir, "banklist.js")
	data, err := os.ReadFile(path)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil
		}
		return err
	}
	var rows [][]json.RawMessage
	if err := json.Unmarshal(data, &rows); err != nil {
		return err
	}
	filtered := rows[:0]
	removed := false
	for _, row := range rows {
		entry := guitarixBankListEntry{}
		if len(row) > 0 {
			_ = json.Unmarshal(row[0], &entry.Name)
		}
		if len(row) > 1 {
			_ = json.Unmarshal(row[1], &entry.File)
		}
		if bankEntryMatches(entry, bank, filename) {
			removed = true
			continue
		}
		filtered = append(filtered, row)
	}
	if !removed {
		return nil
	}
	out, err := json.MarshalIndent(filtered, "", "  ")
	if err != nil {
		return err
	}
	out = append(out, '\n')
	return os.WriteFile(path, out, 0644)
}

func bankEntryMatches(entry guitarixBankListEntry, bank, filename string) bool {
	if bank != "" && (entry.Name == bank || entry.File == bank || strings.TrimSuffix(entry.File, filepath.Ext(entry.File)) == bank) {
		return true
	}
	if filename != "" && entry.File == filename {
		return true
	}
	key := normalizedBankKey(bank)
	if key == "" {
		return false
	}
	return normalizedBankKey(entry.Name) == key || normalizedBankKey(strings.TrimSuffix(entry.File, filepath.Ext(entry.File))) == key
}

func safeBankFilePath(dir, filename string) (string, error) {
	filename = cleanFilename(filename)
	if !strings.HasSuffix(strings.ToLower(filename), ".gx") {
		return "", fmt.Errorf("not a .gx bank file: %s", filename)
	}
	absDir, err := filepath.Abs(dir)
	if err != nil {
		return "", err
	}
	path := filepath.Join(absDir, filename)
	absPath, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	rel, err := filepath.Rel(absDir, absPath)
	if err != nil {
		return "", err
	}
	if rel == ".." || strings.HasPrefix(rel, ".."+string(os.PathSeparator)) || filepath.IsAbs(rel) {
		return "", fmt.Errorf("bank path escapes bank directory: %s", filename)
	}
	return absPath, nil
}

func normalizedBankKey(value string) string {
	value = strings.TrimSuffix(strings.ToLower(strings.TrimSpace(value)), ".gx")
	var b strings.Builder
	for _, r := range value {
		if r >= 'a' && r <= 'z' || r >= '0' && r <= '9' {
			b.WriteRune(r)
		}
	}
	return b.String()
}

func guitarixCall(method string, params any) (json.RawMessage, error) {
	data, err := guitarixRPC(map[string]any{
		"jsonrpc": "2.0",
		"method":  method,
		"params":  params,
		"id":      "gxpreset",
	}, true)
	if err != nil {
		return nil, err
	}
	var resp struct {
		Result json.RawMessage `json:"result"`
		Error  any             `json:"error"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return nil, err
	}
	if resp.Error != nil {
		return nil, fmt.Errorf("guitarix rpc error: %v", resp.Error)
	}
	return resp.Result, nil
}

func guitarixNotify(method string, params any) error {
	_, err := guitarixRPC(map[string]any{
		"jsonrpc": "2.0",
		"method":  method,
		"params":  params,
	}, false)
	return err
}

func guitarixRPC(payload map[string]any, waitResponse bool) ([]byte, error) {
	conn, err := net.DialTimeout("tcp", "127.0.0.1:7000", 900*time.Millisecond)
	if err != nil {
		if startErr := ensureGuitarixRunning(); startErr != nil {
			return nil, fmt.Errorf("connect Guitarix RPC 127.0.0.1:7000: %w; auto-start failed: %v", err, startErr)
		}
		conn, err = net.DialTimeout("tcp", "127.0.0.1:7000", 900*time.Millisecond)
		if err != nil {
			return nil, fmt.Errorf("connect Guitarix RPC 127.0.0.1:7000 after auto-start: %w", err)
		}
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(1200 * time.Millisecond))
	encoded, err := json.Marshal(payload)
	if err != nil {
		return nil, err
	}
	if _, err := conn.Write(append(encoded, '\n')); err != nil {
		return nil, err
	}
	if !waitResponse {
		return nil, nil
	}
	data, err := io.ReadAll(conn)
	if err != nil && len(data) == 0 {
		return nil, err
	}
	clean := extractJSONObject(strings.TrimSpace(string(data)))
	if clean == "" {
		return nil, errors.New("empty Guitarix RPC response")
	}
	return []byte(clean), nil
}

func ensureGuitarixRunning() error {
	guitarixLaunchMu.Lock()
	defer guitarixLaunchMu.Unlock()

	if guitarixRPCReady(120 * time.Millisecond) {
		return nil
	}

	path, err := commandPath("pw-jack")
	if err != nil {
		return err
	}
	cmd := exec.Command(path, "guitarix", "-N", "-p", "7000")
	cmd.Env = append(os.Environ(), "PIPEWIRE_LATENCY=128/48000")
	cmd.Stdout = io.Discard
	cmd.Stderr = io.Discard
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start pw-jack guitarix -N -p 7000: %w", err)
	}
	go func() {
		_ = cmd.Wait()
	}()

	deadline := time.Now().Add(6 * time.Second)
	for time.Now().Before(deadline) {
		if guitarixRPCReady(180 * time.Millisecond) {
			return nil
		}
		time.Sleep(180 * time.Millisecond)
	}
	return errors.New("Guitarix auto-started but RPC did not become ready on 127.0.0.1:7000")
}

func guitarixRPCReady(timeout time.Duration) bool {
	conn, err := net.DialTimeout("tcp", "127.0.0.1:7000", timeout)
	if err != nil {
		return false
	}
	_ = conn.Close()
	return true
}

func extractNames(raw json.RawMessage) ([]string, error) {
	var value any
	if err := json.Unmarshal(raw, &value); err != nil {
		return nil, err
	}
	var names []string
	collectJSONNames(value, &names)
	return names, nil
}

func collectJSONNames(value any, names *[]string) {
	switch v := value.(type) {
	case string:
		if strings.TrimSpace(v) != "" {
			*names = append(*names, v)
		}
	case []any:
		for _, item := range v {
			collectJSONNames(item, names)
		}
	case map[string]any:
		for _, key := range []string{"name", "bank", "title", "label"} {
			if s, ok := v[key].(string); ok && strings.TrimSpace(s) != "" {
				*names = append(*names, s)
				return
			}
		}
	}
}

func extractJSONObject(data string) string {
	start := strings.IndexByte(data, '{')
	end := strings.LastIndexByte(data, '}')
	if start < 0 || end < start {
		return ""
	}
	return data[start : end+1]
}

func printPage(query, order string, page int, rawURL string, items []Artifact) {
	fmt.Printf("\nGuitarix presets: page %d order=%s search=%q\n%s\n", page, order, query, rawURL)
	if len(items) == 0 {
		fmt.Println("No downloadable .gx files found on this page.")
		return
	}
	for i, a := range items {
		meta := strings.Join(nonEmpty([]string{a.Author, a.Size, downloadsLabel(a.Downloads)}), " | ")
		fmt.Printf("%2d. %-42s %s\n", i+1, truncate(a.Name, 42), meta)
	}
	fmt.Println()
}

func nextOrder(current string) string {
	orders := []string{"created_at", "most_downloaded", "top_rated", "name", "updated_at"}
	for i, order := range orders {
		if order == current {
			return orders[(i+1)%len(orders)]
		}
	}
	return orders[0]
}

func cleanText(s string) string {
	s = stripTagsRe.ReplaceAllString(s, " ")
	s = html.UnescapeString(s)
	return strings.Join(strings.Fields(s), " ")
}

func getAttr(tag, name string) string {
	for _, quote := range []byte{'"', '\''} {
		prefix := name + "=" + string(quote)
		i := strings.Index(tag, prefix)
		if i < 0 {
			continue
		}
		i += len(prefix)
		j := strings.IndexByte(tag[i:], quote)
		if j < 0 {
			continue
		}
		return html.UnescapeString(tag[i : i+j])
	}
	return ""
}

func cleanFilename(name string) string {
	name = filepath.Base(strings.TrimSpace(name))
	name = strings.ReplaceAll(name, string(os.PathSeparator), "_")
	name = strings.ReplaceAll(name, "/", "_")
	name = strings.ReplaceAll(name, "\\", "_")
	return name
}

func pathBaseFromURL(raw string) string {
	u, err := url.Parse(raw)
	if err != nil {
		return ""
	}
	return cleanFilename(filepath.Base(u.Path))
}

func absolutize(raw string) string {
	if raw == "" {
		return ""
	}
	u, err := url.Parse(raw)
	if err != nil {
		return raw
	}
	if u.IsAbs() {
		return u.String()
	}
	base, _ := url.Parse(baseURL)
	return base.ResolveReference(u).String()
}

func defaultBankDir() string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return "."
	}
	return filepath.Join(home, ".config", "guitarix", "banks")
}

func loadAppConfig() AppConfig {
	path, err := appConfigPath()
	if err != nil {
		return AppConfig{}
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return AppConfig{}
	}
	var cfg AppConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		return AppConfig{}
	}
	return cfg
}

func saveAppConfig(cfg AppConfig) error {
	path, err := appConfigPath()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return err
	}
	data = append(data, '\n')
	return os.WriteFile(path, data, 0644)
}

func appConfigPath() (string, error) {
	dir, err := os.UserConfigDir()
	if err != nil || dir == "" {
		home, homeErr := os.UserHomeDir()
		if homeErr != nil || home == "" {
			return "", firstNonNil(err, homeErr)
		}
		dir = filepath.Join(home, ".config")
	}
	return filepath.Join(dir, "gxpreset", "config.json"), nil
}

func truncate(s string, limit int) string {
	r := []rune(s)
	if len(r) <= limit {
		return s
	}
	if limit <= 1 {
		return string(r[:limit])
	}
	return string(r[:limit-1]) + "…"
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if strings.TrimSpace(v) != "" {
			return strings.TrimSpace(v)
		}
	}
	return ""
}

func nonEmpty(values []string) []string {
	out := values[:0]
	for _, v := range values {
		if strings.TrimSpace(v) != "" {
			out = append(out, strings.TrimSpace(v))
		}
	}
	return out
}

func downloadsLabel(s string) string {
	if s == "" {
		return ""
	}
	return s + " downloads"
}

func (a AudioState) selectedOutput() AudioNode {
	if a.OutSelected >= 0 && a.OutSelected < len(a.Outputs) {
		return a.Outputs[a.OutSelected]
	}
	return AudioNode{}
}

func (a AudioState) selectedInput() AudioNode {
	if a.InSelected >= 0 && a.InSelected < len(a.Inputs) {
		return a.Inputs[a.InSelected]
	}
	return AudioNode{}
}

func (a AudioState) selectedMeterSource() AudioNode {
	if a.MeterSelected >= 0 && a.MeterSelected < len(a.Outputs) {
		return a.Outputs[a.MeterSelected]
	}
	return AudioNode{}
}

func (a AudioState) selectedOutputName() string {
	return a.selectedOutput().Name
}

func (a AudioState) selectedMeterSourceName() string {
	return a.selectedMeterSource().Name
}

func audioNodeIndexByName(nodes []AudioNode, name string) int {
	if strings.TrimSpace(name) == "" {
		return -1
	}
	for i, node := range nodes {
		if node.Name == name {
			return i
		}
	}
	return -1
}

func (a AudioState) nodesConnected(out AudioNode, in AudioNode) bool {
	for _, outPort := range out.Ports {
		for _, inPort := range in.Ports {
			if a.Links[outPort][inPort] {
				return true
			}
		}
	}
	return false
}

func (a AudioState) linkedTargets(out AudioNode) []string {
	seen := make(map[string]bool)
	var targets []string
	for _, outPort := range out.Ports {
		for inPort := range a.Links[outPort] {
			name := nodeNameForPort(a.Inputs, inPort)
			if name == "" {
				name = inPort
			}
			if seen[name] {
				continue
			}
			seen[name] = true
			targets = append(targets, name)
		}
	}
	sort.Strings(targets)
	return targets
}

func nodeNameForPort(nodes []AudioNode, port string) string {
	for _, node := range nodes {
		for _, candidate := range node.Ports {
			if candidate == port {
				return node.Name
			}
		}
	}
	return ""
}

func (g GuitarixState) selectedBank() string {
	if g.BankSelected >= 0 && g.BankSelected < len(g.Banks) {
		return g.Banks[g.BankSelected]
	}
	return ""
}

func (g GuitarixState) selectedPreset() string {
	if g.PresetSelected >= 0 && g.PresetSelected < len(g.Presets) {
		return g.Presets[g.PresetSelected]
	}
	return ""
}

func commandOutput(name string, args ...string) ([]byte, error) {
	path, err := commandPath(name)
	if err != nil {
		return nil, err
	}
	cmd := exec.Command(path, args...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("%s %s: %w: %s", name, strings.Join(args, " "), err, strings.TrimSpace(string(out)))
	}
	return out, nil
}

func commandPath(name string) (string, error) {
	if path, err := exec.LookPath(name); err == nil {
		return path, nil
	}
	for _, candidate := range []string{
		filepath.Join("/usr/bin", name),
		filepath.Join("/usr/sbin", name),
		filepath.Join("/bin", name),
		filepath.Join("/sbin", name),
	} {
		if st, err := os.Stat(candidate); err == nil && !st.IsDir() {
			return candidate, nil
		}
	}
	return "", fmt.Errorf("%s not found in PATH", name)
}

func splitCleanLines(s string) []string {
	var lines []string
	for _, line := range strings.Split(s, "\n") {
		line = strings.TrimSpace(line)
		if line != "" {
			lines = append(lines, line)
		}
	}
	return lines
}

func filterAudioPorts(ports []string) []string {
	out := ports[:0]
	for _, port := range ports {
		if isMidiName(port) {
			continue
		}
		out = append(out, port)
	}
	return out
}

func containsString(values []string, needle string) bool {
	for _, value := range values {
		if value == needle {
			return true
		}
	}
	return false
}

func uniqueStrings(values []string) []string {
	seen := make(map[string]bool)
	out := values[:0]
	for _, value := range values {
		value = strings.TrimSpace(value)
		if value == "" || seen[value] {
			continue
		}
		seen[value] = true
		out = append(out, value)
	}
	return out
}

func clampIndex(index, length int) int {
	if length <= 0 {
		return 0
	}
	if index < 0 {
		return 0
	}
	if index >= length {
		return length - 1
	}
	return index
}

func isMidiName(name string) bool {
	return strings.Contains(strings.ToLower(name), "midi")
}

func spectrumProgressView(values []float64, width int, height int) string {
	if height < spectrumRangeCount {
		height = spectrumRangeCount
	}
	values = resampleSpectrum(values, height)
	barWidth := width - 5
	if barWidth < 1 {
		barWidth = 1
	}
	var out strings.Builder
	out.Grow(height * (barWidth + 6))
	lastLabel := -1
	for i := 0; i < height; i++ {
		value := 0.0
		if i < len(values) {
			value = values[i]
		}
		labelIndex := i * len(spectrumLabels) / height
		label := ""
		if labelIndex != lastLabel {
			label = spectrumLabels[labelIndex]
			lastLabel = labelIndex
		}
		if i > 0 {
			out.WriteByte('\n')
		}
		out.WriteString(mutedStyle.Render(fmt.Sprintf("%4s", label)))
		out.WriteByte(' ')
		out.WriteString(spectrumBarView(value, barWidth, i, height))
	}
	return out.String()
}

func spectrumBandColor(index, count int) string {
	if count <= 1 {
		return "#5EEAD4"
	}
	colors := []string{
		"#22C55E",
		"#84CC16",
		"#EAB308",
		"#F97316",
		"#EF4444",
	}
	pos := index * (len(colors) - 1) / (count - 1)
	return colors[pos]
}

func spectrumBandStyle(index, count int) lipgloss.Style {
	if count <= 1 {
		return spectrumFillStyles[0]
	}
	pos := index * (len(spectrumFillStyles) - 1) / (count - 1)
	return spectrumFillStyles[pos]
}

func spectrumBarView(value float64, width int, index int, count int) string {
	if width <= 0 {
		return ""
	}
	if value < 0 {
		value = 0
	}
	if value > 1 {
		value = 1
	}
	filled := int(math.Round(value * float64(width)))
	if filled < 0 {
		filled = 0
	}
	if filled > width {
		filled = width
	}
	filledPart := ""
	if filled > 0 {
		filledPart = spectrumBandStyle(index, count).Render(strings.Repeat("█", filled))
	}
	empty := width - filled
	if empty <= 0 {
		return filledPart
	}
	return filledPart + spectrumEmptyStyle.Render(strings.Repeat("░", empty))
}

func resampleSpectrum(values []float64, count int) []float64 {
	if count <= 0 {
		return nil
	}
	if len(values) == 0 {
		return nil
	}
	if len(values) == count {
		return values
	}
	out := make([]float64, count)
	for i := 0; i < count; i++ {
		pos := float64(i) * float64(len(values)-1) / float64(max(1, count-1))
		lo := int(math.Floor(pos))
		hi := int(math.Ceil(pos))
		if hi >= len(values) {
			hi = len(values) - 1
		}
		frac := pos - float64(lo)
		out[i] = values[lo]*(1-frac) + values[hi]*frac
	}
	return out
}

func smoothDisplaySpectrum(previous, next []float64) []float64 {
	if len(next) == 0 {
		return nil
	}
	if len(previous) != len(next) {
		out := make([]float64, len(next))
		copy(out, next)
		return out
	}
	out := make([]float64, len(next))
	for i, value := range next {
		if value > previous[i] {
			out[i] = previous[i]*0.10 + value*0.90
		} else {
			out[i] = previous[i]*0.45 + value*0.55
		}
		if out[i] < 0 {
			out[i] = 0
		}
		if out[i] > 1 {
			out[i] = 1
		}
	}
	return out
}

func panel(title, body string, width int, focused bool) string {
	if width < 20 {
		width = 20
	}
	innerWidth := max(1, width-2)
	style := panelStyle
	if focused {
		style = focusPanelStyle
	}
	if title != "" {
		title = " " + title + " "
		style = style.BorderTopForeground(lipgloss.Color("#5EEAD4"))
		body = accentStyle.Render(title) + "\n" + body
	}
	return style.Width(innerWidth).Render(body)
}

func labelValue(label, value string, width int) string {
	if strings.TrimSpace(value) == "" {
		value = "-"
	}
	prefix := mutedStyle.Render(label + ": ")
	available := width - lipgloss.Width(label) - 2
	if available < 12 {
		available = 12
	}
	lines := wordWrap(value, available, 3)
	if len(lines) == 0 {
		return prefix
	}
	out := prefix + lines[0]
	for _, line := range lines[1:] {
		out += "\n" + strings.Repeat(" ", len(label)+2) + line
	}
	return out
}

func wordWrapLine(s string, width int) string {
	lines := wordWrap(s, width, 1)
	if len(lines) == 0 {
		return ""
	}
	return lines[0]
}

func wordWrap(s string, width int, maxLines int) []string {
	if width < 12 {
		width = 12
	}
	words := strings.Fields(s)
	if len(words) == 0 {
		return nil
	}
	var lines []string
	var line string
	truncated := false
	for _, word := range words {
		if lipgloss.Width(word) > width {
			word = truncate(word, width)
		}
		if line == "" {
			line = word
			continue
		}
		if lipgloss.Width(line)+1+lipgloss.Width(word) <= width {
			line += " " + word
			continue
		}
		lines = append(lines, line)
		line = word
		if maxLines > 0 && len(lines) >= maxLines {
			truncated = true
			break
		}
	}
	if line != "" && (maxLines <= 0 || len(lines) < maxLines) {
		lines = append(lines, line)
	}
	if truncated && len(lines) > 0 {
		last := lines[len(lines)-1]
		if lipgloss.Width(last) >= width {
			lines[len(lines)-1] = truncate(last, width-1) + "…"
		} else {
			lines[len(lines)-1] = truncate(last, width-1) + "…"
		}
	}
	return lines
}

func safeWidth(width int) int {
	if width <= 0 {
		return 100
	}
	if width < 40 {
		return 40
	}
	return width - 2
}

func safeHeight(height int) int {
	if height <= 0 {
		return 30
	}
	if height < 10 {
		return 10
	}
	return height
}

func fitHeight(s string, height int) string {
	if height <= 0 {
		return ""
	}
	lines := strings.Split(s, "\n")
	if len(lines) <= height {
		return s
	}
	return strings.Join(lines[:height], "\n")
}

func tail(values []string, n int) []string {
	if len(values) <= n {
		return values
	}
	return values[len(values)-n:]
}

func firstNonNil(values ...error) error {
	for _, value := range values {
		if value != nil {
			return value
		}
	}
	return nil
}

func max(a, b int) int {
	if a > b {
		return a
	}
	return b
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}

func fatalf(format string, args ...interface{}) {
	fmt.Fprintf(os.Stderr, format, args...)
	os.Exit(1)
}
