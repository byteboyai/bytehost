//! Real CEF runtime slice (feature `cef-runtime`).
//!
//! Owns in-process CEF initialization, native child surfaces, per-scope request
//! contexts, the external message pump, target lifecycle and event/devtools
//! queues. It stays on the GUI thread: `CefRuntime` is `Rc`-based and must not be
//! moved into worker threads. Product code talks to it through a GUI-thread
//! facade, not by sharing CEF handles.
//!
//! Scope: manual browsing (T4) plus the in-process DevTools transport that T5 will
//! build observation/input on. It intentionally does not implement the whole
//! `BrowserHost` trait yet: typed observation/locator/input live in T5.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytehost_browser_core::*;
use cef::*;

use crate::profile::{instance_lock_path, profile_paths};
use crate::runtime_guard::{require_closed, require_single_target, validate_identity};

pub type PumpScheduler = Arc<dyn Fn(i64) + Send + Sync>;
pub type NavigationPolicy = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct CefRuntimeConfig {
    /// Product data root (derives `browser/cef/...`).
    pub data_root: PathBuf,
    pub framework_dir: Option<PathBuf>,
    pub helper_path: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub no_sandbox: bool,
    pub persist_session_cookies: bool,
    pub user_agent: Option<String>,
    /// Wake the host event loop; must only schedule, never pump from another thread.
    pub scheduler: PumpScheduler,
    /// Navigation gate for API calls and all frame navigations/redirects.
    /// Defaults to http/https/file/about:blank/data HTML and denies
    /// javascript:/dozer:/chrome:.
    pub navigation_policy: Option<NavigationPolicy>,
}

impl std::fmt::Debug for CefRuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CefRuntimeConfig")
            .field("data_root", &self.data_root)
            .field("no_sandbox", &self.no_sandbox)
            .field("persist_session_cookies", &self.persist_session_cookies)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevToolsResult {
    pub id: i32,
    pub method: Option<String>,
    /// Raw JSON body (`{"id":..,"result":..}` or `{"id":..,"error":..}`).
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevToolsEvent {
    pub method: String,
    pub params: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    pub remaining_targets: usize,
    pub drain_ms: u128,
    pub shutdown_ms: u128,
}

struct TargetEntry {
    identity: TargetIdentity,
    tab: TabId,
    browser: Browser,
    browser_id: i32,
    hidden: bool,
    url: String,
    title: String,
    loading: bool,
    active: bool,
    document_serial: u64,
}

struct PendingCreate {
    tab: TabId,
    identity: TargetIdentity,
}

struct RuntimeState {
    provider: ProviderVersion,
    session: BrowserSessionId,
    context_initialized: bool,
    next_target_serial: u64,
    next_devtools_id: i32,
    next_sequence: u64,
    targets: HashMap<TargetId, TargetEntry>,
    browser_targets: HashMap<i32, TargetId>,
    profiles: HashMap<String, RequestContext>,
    pending_create: Option<PendingCreate>,
    clients: Vec<Client>,
    observers: Vec<DevToolsMessageObserver>,
    registrations: Vec<Registration>,
    pending_devtools: HashMap<i32, String>,
    devtools_results: VecDeque<DevToolsResult>,
    devtools_events: VecDeque<DevToolsEvent>,
    events: VecDeque<EventEnvelope>,
    closing: HashSet<TargetId>,
    allow: NavigationPolicy,
}

impl RuntimeState {
    fn emit(&mut self, identity: Option<TargetIdentity>, event: BrowserEvent) {
        self.next_sequence += 1;
        let sequence = self.next_sequence;
        let envelope = EventEnvelope {
            sequence,
            at_ms: now_ms(),
            identity,
            event,
        };
        if self.events.len() >= 4096 {
            self.events.pop_front();
        }
        self.events.push_back(envelope);
    }

    fn target(&self, target: &TargetId) -> Option<&TargetEntry> {
        self.targets.get(target)
    }

    fn browser_target(&self, browser: Option<&Browser>) -> Option<TargetId> {
        let browser_id = browser?.identifier();
        self.targets
            .iter()
            .find(|(_, entry)| entry.browser.identifier() == browser_id)
            .map(|(target, _)| target.clone())
    }

    fn scope_key(scope: &ProfileScope) -> String {
        scope.storage_segment()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn default_navigation_allowed(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("file://")
        || lower == "about:blank"
        || lower.starts_with("data:text/html")
}

// ---------------------------------------------------------------------------
// CEF handlers
// ---------------------------------------------------------------------------

wrap_app! {
    struct RuntimeApp {
        shared: Rc<RefCell<RuntimeState>>,
        scheduler: PumpScheduler,
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(RuntimeBrowserProcessHandler::new(
                self.shared.clone(),
                self.scheduler.clone(),
            ))
        }
    }
}

wrap_browser_process_handler! {
    struct RuntimeBrowserProcessHandler {
        shared: Rc<RefCell<RuntimeState>>,
        scheduler: PumpScheduler,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            self.shared.borrow_mut().context_initialized = true;
        }

        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            // Contract: schedule, do not pump from CEF's thread.
            (self.scheduler)(delay_ms);
        }
    }
}

wrap_client! {
    struct RuntimeClient {
        shared: Rc<RefCell<RuntimeState>>,
        allow: NavigationPolicy,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(RuntimeLifeSpanHandler::new(self.shared.clone()))
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(RuntimeLoadHandler::new(self.shared.clone()))
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(RuntimeDisplayHandler::new(self.shared.clone()))
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(RuntimeRequestHandler::new(self.allow.clone()))
        }
    }
}

// Only the policy is shared here: these handlers do not touch GUI state.
wrap_request_handler! {
    struct RuntimeRequestHandler {
        allow: NavigationPolicy,
    }
    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: i32,
            _is_redirect: i32,
        ) -> i32 {
            let url = request.map(|request| CefString::from(&request.url()).to_string());
            match url {
                Some(url) if !url.is_empty() => (!(self.allow)(&url)) as i32,
                _ => 0,
            }
        }

        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
        ) -> i32 {
            match target_url {
                Some(url) => {
                    let url = url.to_string();
                    if url.is_empty() {
                        0
                    } else {
                        (!(self.allow)(&url)) as i32
                    }
                }
                None => 0,
            }
        }
    }
}

fn frame_url(frame: Option<&mut Frame>) -> String {
    frame
        .map(|frame| CefString::from(&frame.url()).to_string())
        .unwrap_or_default()
}

fn browser_identity(state: &RuntimeState, browser: Option<&mut Browser>) -> Option<TargetIdentity> {
    let mut browser = browser?.clone();
    let id = browser.identifier();
    state
        .browser_targets
        .get(&id)
        .and_then(|target| state.targets.get(target))
        .map(|entry| entry.identity.clone())
}

wrap_life_span_handler! {
    struct RuntimeLifeSpanHandler {
        shared: Rc<RefCell<RuntimeState>>,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            _target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            // T4 single-surface slice: never create an untracked popup.
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser.cloned() else { return };
            let mut state = self.shared.borrow_mut();
            let Some(pending) = state.pending_create.take() else {
                return;
            };
            let browser_id = browser.identifier();
            let host = browser.host();
            if let Some(host) = host {
                let mut observer = RuntimeDevToolsObserver::new(self.shared.clone());
                let registration = host.add_dev_tools_message_observer(Some(&mut observer));
                state.observers.push(observer);
                if let Some(registration) = registration {
                    state.registrations.push(registration);
                }
            }
            state.emit(
                Some(pending.identity.clone()),
                BrowserEvent::TargetCreated {
                    tab: pending.tab.clone(),
                },
            );
            state.browser_targets.insert(
                browser_id,
                pending.identity.target.clone(),
            );
            state.targets.insert(
                pending.identity.target.clone(),
                TargetEntry {
                    identity: pending.identity,
                    tab: pending.tab,
                    browser,
                    browser_id,
                    hidden: false,
                    url: String::new(),
                    title: String::new(),
                    loading: true,
                    active: true,
                    document_serial: 0,
                },
            );
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser.cloned() else { return };
            let mut state = self.shared.borrow_mut();
            let id = browser.identifier();
            let Some(target) = state.browser_targets.remove(&id) else {
                return;
            };
            if let Some(entry) = state.targets.remove(&target) {
                state.closing.remove(&target);
                state.emit(
                    Some(entry.identity),
                    BrowserEvent::TargetClosed { tab: entry.tab },
                );
            }
        }

        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            0
        }
    }
}

wrap_load_handler! {
    struct RuntimeLoadHandler {
        shared: Rc<RefCell<RuntimeState>>,
    }

    impl LoadHandler {
        fn on_load_start(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            let is_main = frame.as_ref().map(|f| f.is_main() != 0).unwrap_or(false);
            let url = frame_url(frame);
            let mut state = self.shared.borrow_mut();
            let Some(target) = state.browser_target(browser.as_deref()) else { return };
            let identity = state
                .targets
                .get_mut(&target)
                .map(|entry| {
                    if is_main {
                        entry.url = url.clone();
                        entry.loading = true;
                    }
                    entry.identity.clone()
                });
            state.emit(
                identity,
                BrowserEvent::NavigationStarted { url, is_main },
            );
        }

        fn on_load_end(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            http_status_code: i32,
        ) {
            let is_main = frame.as_ref().map(|f| f.is_main() != 0).unwrap_or(false);
            let url = frame_url(frame);
            let mut state = self.shared.borrow_mut();
            let Some(target) = state.browser_target(browser.as_deref()) else { return };
            let mut emitted = None;
            if let Some(entry) = state.targets.get_mut(&target) {
                if is_main {
                    entry.loading = false;
                    entry.url = url.clone();
                    entry.document_serial += 1;
                }
                let document = DocumentId::new(format!(
                    "doc-{}-{}",
                    entry.identity.target, entry.document_serial.max(1)
                ))
                .ok();
                if let Some(document) = document {
                    emitted = Some((entry.identity.clone(), document));
                }
            }
            if let Some((identity, document)) = emitted {
                state.emit(
                    Some(identity),
                    BrowserEvent::NavigationReady {
                        url,
                        is_main,
                        document,
                        http_status: http_status_code,
                    },
                );
            }
        }

        fn on_load_error(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let code: sys::cef_errorcode_t = error_code.into();
            let is_main = frame.as_ref().map(|f| f.is_main() != 0).unwrap_or(false);
            let url = failed_url.map(|u| u.to_string()).unwrap_or_default();
            let mut state = self.shared.borrow_mut();
            let Some(target) = state.browser_target(browser.as_deref()) else { return };
            let identity = state.targets.get_mut(&target).map(|entry| {
                if is_main { entry.loading = false; }
                entry.identity.clone()
            });
            state.emit(
                identity,
                BrowserEvent::NavigationFailed {
                    url,
                    is_main,
                    code: code as i32,
                    message: error_text.map(|t| t.to_string()).unwrap_or_default(),
                },
            );
        }
    }
}

wrap_display_handler! {
    struct RuntimeDisplayHandler {
        shared: Rc<RefCell<RuntimeState>>,
    }

    impl DisplayHandler {
        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            let title = title.map(|t| t.to_string()).unwrap_or_default();
            let mut state = self.shared.borrow_mut();
            let Some(target) = state.browser_target(browser.as_deref()) else { return };
            let identity = state
                .targets
                .get_mut(&target)
                .map(|entry| {
                    entry.title = title.clone();
                    entry.identity.clone()
                });
            if let Some(identity) = identity {
                state.emit(Some(identity), BrowserEvent::TitleChanged { title });
            }
        }

        fn on_address_change(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            let is_main = frame.as_ref().map(|f| f.is_main() != 0).unwrap_or(false);
            let url = url.map(|u| u.to_string()).unwrap_or_default();
            let mut state = self.shared.borrow_mut();
            let Some(target) = state.browser_target(browser.as_deref()) else { return };
            let identity = state
                .targets
                .get_mut(&target)
                .map(|entry| {
                    if is_main {
                        entry.url = url.clone();
                    }
                    entry.identity.clone()
                });
            state.emit(identity, BrowserEvent::UrlChanged { url, is_main });
        }
    }
}

wrap_dev_tools_message_observer! {
    struct RuntimeDevToolsObserver {
        shared: Rc<RefCell<RuntimeState>>,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_message(
            &self,
            _browser: Option<&mut Browser>,
            message: Option<&[u8]>,
        ) -> i32 {
            let Some(body) = message.map(|m| String::from_utf8_lossy(m).into_owned()) else {
                return 1;
            };
            let mut state = self.shared.borrow_mut();
            let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
            if let Some(id) = parsed
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_i64())
            {
                let id = id as i32;
                let method = state.pending_devtools.remove(&id);
                if state.devtools_results.len() >= 1024 {
                    state.devtools_results.pop_front();
                }
                state.devtools_results.push_back(DevToolsResult {
                    id,
                    method,
                    body,
                });
            } else if let Some(method) = parsed
                .as_ref()
                .and_then(|v| v.get("method"))
                .and_then(|v| v.as_str())
            {
                let params = parsed
                    .as_ref()
                    .and_then(|v| v.get("params"))
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                if state.devtools_events.len() >= 4096 {
                    state.devtools_events.pop_front();
                }
                state.devtools_events.push_back(DevToolsEvent {
                    method: method.to_owned(),
                    params,
                });
            }
            1
        }
    }
}

// ---------------------------------------------------------------------------
// Public runtime
// ---------------------------------------------------------------------------

pub struct CefRuntime {
    shared: Rc<RefCell<RuntimeState>>,
    library: library_loader::LibraryLoader,
    _args: args::Args,
    config: CefRuntimeConfig,
}

impl CefRuntime {
    pub fn initialize(config: CefRuntimeConfig) -> Result<Self, BrowserError> {
        let mut config = config;
        if let Ok(root) = std::fs::canonicalize(&config.data_root) {
            config.data_root = root;
        }
        let exe = std::env::current_exe().map_err(|error| {
            BrowserError::new(ErrorClass::ProviderUnavailable, error.to_string())
        })?;
        let library = library_loader::LibraryLoader::new(&exe, false);
        if !library.load() {
            return Err(BrowserError::new(
                ErrorClass::ProviderUnavailable,
                format!("cannot load CEF framework next to {exe:?}"),
            ));
        }
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

        let args = args::Args::new();
        let shared = Rc::new(RefCell::new(RuntimeState {
            provider: ProviderVersion::new("cef-156.0.30").map_err(schema_error)?,
            session: BrowserSessionId::new(format!("cef-session-{}", std::process::id()))
                .map_err(schema_error)?,
            context_initialized: false,
            next_target_serial: 0,
            next_devtools_id: 0,
            next_sequence: 0,
            targets: HashMap::new(),
            browser_targets: HashMap::new(),
            profiles: HashMap::new(),
            pending_create: None,
            clients: Vec::new(),
            observers: Vec::new(),
            registrations: Vec::new(),
            pending_devtools: HashMap::new(),
            devtools_results: VecDeque::new(),
            devtools_events: VecDeque::new(),
            events: VecDeque::new(),
            closing: HashSet::new(),
            allow: config
                .navigation_policy
                .clone()
                .unwrap_or_else(|| Arc::new(default_navigation_allowed)),
        }));

        let profile_root = config.data_root.join("browser/cef");
        // The browser-process default context gets its own `system` directory;
        // `profiles/global` belongs to an explicit Global request context.
        // All scope request contexts must be descendants of this one root.
        let profiles_root = profile_root.join("profiles");
        let system_root = profiles_root.join("system");
        let system_cache = system_root.join("cache");
        std::fs::create_dir_all(&system_cache).map_err(io_error)?;
        std::fs::create_dir_all(profile_root.join("artifacts")).map_err(io_error)?;

        let mut settings = Settings {
            external_message_pump: 1,
            no_sandbox: config.no_sandbox as _,
            persist_session_cookies: config.persist_session_cookies as _,
            cache_path: CefString::from(system_cache.to_string_lossy().as_ref()),
            root_cache_path: CefString::from(profiles_root.to_string_lossy().as_ref()),
            log_severity: sys::cef_log_severity_t::LOGSEVERITY_INFO.into(),
            ..Default::default()
        };
        if let Some(log) = &config.log_file {
            settings.log_file = CefString::from(log.to_string_lossy().as_ref());
        }
        if let Some(framework) = &config.framework_dir {
            settings.framework_dir_path = CefString::from(framework.to_string_lossy().as_ref());
        }
        if let Some(helper) = &config.helper_path {
            settings.browser_subprocess_path = CefString::from(helper.to_string_lossy().as_ref());
        }
        if let Some(agent) = &config.user_agent {
            settings.user_agent = CefString::from(agent.as_str());
        }

        let mut app = RuntimeApp::new(shared.clone(), config.scheduler.clone());
        let initialized = initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        );
        if initialized != 1 {
            return Err(BrowserError::new(
                ErrorClass::ProviderUnavailable,
                "cef_initialize failed",
            ));
        }

        let runtime = Self {
            shared,
            library,
            _args: args,
            config,
        };
        // CEF fires OnContextInitialized during initialize in practice; pump a
        // bounded amount so callers can create targets immediately.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !runtime.shared.borrow().context_initialized && Instant::now() < deadline {
            runtime.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        if !runtime.shared.borrow().context_initialized {
            runtime.shutdown()?;
            return Err(BrowserError::new(
                ErrorClass::ProviderUnavailable,
                "CEF context did not initialize",
            ));
        }
        Ok(runtime)
    }

    /// Load only the sandbox dylib. Used by the packaging preflight to isolate
    /// dyld/Library-Validation failures without starting CEF or a GUI.
    pub fn load_sandbox_library() -> bool {
        let library = cef::sandbox::Sandbox::new();
        drop(library);
        true
    }

    /// Subprocess entry point (renderer/GPU/utility). Call this **first** in the
    /// helper binary; it must not initialize GUI/winit/DB/Tokio.
    pub fn run_subprocess() -> Result<i32, BrowserError> {
        let args = args::Args::new();
        let is_helper = std::env::args().any(|arg| arg.starts_with("--type="));
        if !is_helper {
            return Err(BrowserError::new(
                ErrorClass::InvalidParams,
                "helper entry requires --type=<subprocess-role>",
            ));
        }
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        let exe = std::env::current_exe().map_err(|error| {
            BrowserError::new(ErrorClass::ProviderUnavailable, error.to_string())
        })?;
        let loader = library_loader::LibraryLoader::new(&exe, true);
        if !loader.load() {
            return Err(BrowserError::new(
                ErrorClass::ProviderUnavailable,
                "helper cannot load bundled CEF framework",
            ));
        }
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let result = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
        drop(loader);
        drop(sandbox);
        Ok(result)
    }

    pub fn provider_version(&self) -> ProviderVersion {
        self.shared.borrow().provider.clone()
    }

    pub fn session(&self) -> BrowserSessionId {
        self.shared.borrow().session.clone()
    }

    /// Single-instance marker path for this data root.
    pub fn instance_lock_path(&self) -> PathBuf {
        instance_lock_path(&self.config.data_root)
    }

    pub fn pump(&self) {
        do_message_loop_work();
    }

    pub fn create_target(
        &self,
        parent_view: usize,
        scope: ProfileScope,
        tab: TabId,
        url: &str,
    ) -> Result<TargetInfo, BrowserError> {
        self.check_navigation(url)?;
        let (identity, context) = {
            let mut state = self.shared.borrow_mut();
            require_single_target(state.targets.len(), state.pending_create.is_some())?;
            state.next_target_serial += 1;
            let target = TargetId::new(format!("target-{}", state.next_target_serial))
                .map_err(schema_error)?;
            let generation = TargetGeneration::new(state.next_target_serial);
            let identity = TargetIdentity::new(
                state.provider.clone(),
                scope.clone(),
                state.session.clone(),
                target,
                generation,
            );
            let context = match state
                .profiles
                .get(&RuntimeState::scope_key(&scope))
                .cloned()
            {
                Some(context) => context,
                None => {
                    drop(state);
                    let context = self.create_request_context(&scope)?;
                    self.shared
                        .borrow_mut()
                        .profiles
                        .insert(RuntimeState::scope_key(&scope), context.clone());
                    return self.finish_create_target(parent_view, identity, tab, url, context);
                }
            };
            (identity, context)
        };
        self.finish_create_target(parent_view, identity, tab, url, context)
    }

    fn create_request_context(&self, scope: &ProfileScope) -> Result<RequestContext, BrowserError> {
        let paths = profile_paths(&self.config.data_root, scope);
        std::fs::create_dir_all(&paths.cache).map_err(io_error)?;
        let settings = RequestContextSettings {
            cache_path: CefString::from(paths.root.to_string_lossy().as_ref()),
            persist_session_cookies: self.config.persist_session_cookies as _,
            ..Default::default()
        };
        request_context_create_context(Some(&settings), None).ok_or_else(|| {
            BrowserError::new(
                ErrorClass::ProviderUnavailable,
                "cannot create CEF request context",
            )
        })
    }

    fn finish_create_target(
        &self,
        parent_view: usize,
        identity: TargetIdentity,
        tab: TabId,
        url: &str,
        mut context: RequestContext,
    ) -> Result<TargetInfo, BrowserError> {
        let rect = cef::Rect {
            x: 0,
            y: 0,
            width: 1100,
            height: 800,
        };
        let window_info = WindowInfo::default().set_as_child(parent_view as *mut _, &rect);
        let allow = self.shared.borrow().allow.clone();
        let mut client = RuntimeClient::new(self.shared.clone(), allow);
        {
            let mut state = self.shared.borrow_mut();
            state.clients.push(client.clone());
            state.pending_create = Some(PendingCreate {
                tab: tab.clone(),
                identity: identity.clone(),
            });
        }
        let browser_settings = BrowserSettings::default();
        let created = browser_host_create_browser(
            Some(&window_info),
            Some(&mut client),
            Some(&CefString::from(url)),
            Some(&browser_settings),
            None,
            Some(&mut context),
        );
        if created == 0 {
            self.shared.borrow_mut().pending_create = None;
            return Err(BrowserError::new(
                ErrorClass::ProviderUnavailable,
                "CreateBrowser was rejected",
            ));
        }
        Ok(TargetInfo {
            identity,
            tab,
            url: url.to_owned(),
            title: String::new(),
            loading: true,
            active: true,
            retained: true,
        })
    }

    pub fn pending_creation(&self) -> bool {
        self.shared.borrow().pending_create.is_some()
    }

    pub fn targets(&self) -> Vec<TargetInfo> {
        let state = self.shared.borrow();
        let mut targets: Vec<TargetInfo> = state
            .targets
            .values()
            .map(|entry| TargetInfo {
                identity: entry.identity.clone(),
                tab: entry.tab.clone(),
                url: entry.url.clone(),
                title: entry.title.clone(),
                loading: entry.loading,
                active: entry.active,
                retained: true,
            })
            .collect();
        targets.sort_by(|a, b| a.tab.as_str().cmp(b.tab.as_str()));
        targets
    }

    pub fn navigate(&self, target: &TargetIdentity, url: &str) -> Result<(), BrowserError> {
        self.check_navigation(url)?;
        let state = self.shared.borrow();
        let entry = state
            .target(&target.target)
            .ok_or_else(target_unavailable)?;
        validate_identity(&entry.identity, target)?;
        let browser = entry.browser.clone();
        drop(state);
        if let Some(frame) = browser.main_frame() {
            frame.load_url(Some(&CefString::from(url)));
            Ok(())
        } else {
            Err(target_unavailable())
        }
    }

    pub fn go_back(&self, target: &TargetIdentity) -> Result<(), BrowserError> {
        self.with_browser(target, |browser| browser.go_back())
    }

    pub fn go_forward(&self, target: &TargetIdentity) -> Result<(), BrowserError> {
        self.with_browser(target, |browser| browser.go_forward())
    }

    pub fn reload(&self, target: &TargetIdentity) -> Result<(), BrowserError> {
        self.with_browser(target, |browser| browser.reload())
    }

    pub fn stop(&self, target: &TargetIdentity) -> Result<(), BrowserError> {
        self.with_browser(target, |browser| browser.stop_load())
    }

    pub fn resize(&self, target: &TargetIdentity) -> Result<(), BrowserError> {
        self.with_browser(target, |browser| {
            if let Some(host) = browser.host() {
                host.was_resized();
            }
        })
    }

    pub fn close_target(&self, target: &TargetId) -> Result<(), BrowserError> {
        let mut state = self.shared.borrow_mut();
        let Some(entry) = state.targets.get(target) else {
            return Err(target_unavailable());
        };
        let browser = entry.browser.clone();
        state.closing.insert(target.clone());
        drop(state);
        if let Some(host) = browser.host() {
            host.close_browser(1);
            Ok(())
        } else {
            Err(target_unavailable())
        }
    }

    pub fn set_hidden(&self, target: &TargetIdentity, hidden: bool) -> Result<(), BrowserError> {
        let mut state = self.shared.borrow_mut();
        let entry = state
            .targets
            .get_mut(&target.target)
            .ok_or_else(target_unavailable)?;
        if let Some(host) = entry.browser.host() {
            host.was_hidden(hidden as _);
        }
        entry.hidden = hidden;
        entry.active = !hidden;
        Ok(())
    }

    pub fn set_focus(&self, target: &TargetIdentity, focus: bool) -> Result<(), BrowserError> {
        self.with_browser(target, |browser| {
            if let Some(host) = browser.host() {
                host.set_focus(focus as _);
            }
        })
    }

    pub fn view_handle(&self, target: &TargetId) -> Result<usize, BrowserError> {
        let state = self.shared.borrow();
        let entry = state.targets.get(target).ok_or_else(target_unavailable)?;
        entry
            .browser
            .host()
            .map(|host| host.window_handle() as usize)
            .ok_or_else(target_unavailable)
    }

    pub fn can_go_back(&self, target: &TargetIdentity) -> Result<bool, BrowserError> {
        let state = self.shared.borrow();
        let entry = state
            .target(&target.target)
            .ok_or_else(target_unavailable)?;
        Ok(entry.browser.can_go_back() != 0)
    }

    pub fn can_go_forward(&self, target: &TargetIdentity) -> Result<bool, BrowserError> {
        let state = self.shared.borrow();
        let entry = state
            .target(&target.target)
            .ok_or_else(target_unavailable)?;
        Ok(entry.browser.can_go_forward() != 0)
    }

    pub fn find(
        &self,
        target: &TargetIdentity,
        text: &str,
        forward: bool,
        match_case: bool,
        find_next: bool,
    ) -> Result<(), BrowserError> {
        self.with_browser(target, |browser| {
            if let Some(host) = browser.host() {
                host.find(
                    Some(&CefString::from(text)),
                    forward as _,
                    match_case as _,
                    find_next as _,
                );
            }
        })
    }

    pub fn stop_finding(
        &self,
        target: &TargetIdentity,
        clear_selection: bool,
    ) -> Result<(), BrowserError> {
        self.with_browser(target, |browser| {
            if let Some(host) = browser.host() {
                host.stop_finding(clear_selection as _);
            }
        })
    }

    pub fn set_zoom(&self, target: &TargetIdentity, level: f64) -> Result<(), BrowserError> {
        self.with_browser(target, |browser| {
            if let Some(host) = browser.host() {
                host.set_zoom_level(level);
            }
        })
    }

    /// Send a bounded in-process DevTools command. Results/events are delivered
    /// through [`Self::drain_devtools_results`]/[`Self::drain_devtools_events`].
    pub fn send_devtools(
        &self,
        target: &TargetId,
        method: &str,
        params_json: Option<&serde_json::Value>,
    ) -> Result<i32, BrowserError> {
        let mut state = self.shared.borrow_mut();
        let Some(entry) = state.targets.get(target) else {
            return Err(target_unavailable());
        };
        let browser = entry.browser.clone();
        let host = browser.host().ok_or_else(target_unavailable)?;
        state.next_devtools_id += 1;
        let id = state.next_devtools_id;
        let mut message = serde_json::Map::new();
        message.insert("id".into(), serde_json::json!(id));
        message.insert("method".into(), serde_json::json!(method));
        if let Some(params) = params_json {
            message.insert("params".into(), params.clone());
        }
        let body = serde_json::Value::Object(message).to_string();
        state.pending_devtools.insert(id, method.to_owned());
        drop(state);
        let sent = host.send_dev_tools_message(Some(body.as_bytes()));
        if sent == 0 {
            self.shared.borrow_mut().pending_devtools.remove(&id);
            return Err(BrowserError::new(
                ErrorClass::ProviderUnavailable,
                "DevTools message was rejected",
            ));
        }
        Ok(id)
    }

    pub fn drain_events(&self) -> Vec<EventEnvelope> {
        self.shared.borrow_mut().events.drain(..).collect()
    }

    pub fn drain_devtools_results(&self) -> Vec<DevToolsResult> {
        self.shared
            .borrow_mut()
            .devtools_results
            .drain(..)
            .collect()
    }

    pub fn drain_devtools_events(&self) -> Vec<DevToolsEvent> {
        self.shared.borrow_mut().devtools_events.drain(..).collect()
    }

    /// Close targets, pump to completion, then call `CefShutdown`.
    ///
    /// For windowed child surfaces the host must tear down the owning native
    /// view/window hierarchy before this call (after requesting browser close).
    /// On error CEF remains loaded intentionally; the host MUST terminate the
    /// process. Do not attempt another runtime or continue using CEF.
    pub fn shutdown(self) -> Result<ShutdownReport, BrowserError> {
        let drain_start = Instant::now();
        let browsers: Vec<Browser> = self
            .shared
            .borrow()
            .targets
            .values()
            .map(|entry| entry.browser.clone())
            .collect();
        for browser in browsers {
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
        }
        let drain_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = self.shared.borrow();
            let done = state.targets.is_empty() && state.pending_create.is_none();
            drop(state);
            if done || Instant::now() >= drain_deadline {
                break;
            }
            self.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let remaining_targets = self.shared.borrow().targets.len();
        let pending_create = self.shared.borrow().pending_create.is_some();
        let drain_ms = drain_start.elapsed().as_millis();
        if let Err(error) = require_closed(remaining_targets, pending_create) {
            // Unloading live CEF code / dropping its objects is unsafe. Preserve
            // ownership until the host's fatal exit instead of claiming success.
            std::mem::forget(self);
            return Err(error.with_detail(serde_json::json!({
                "remaining_targets": remaining_targets,
                "pending_create": pending_create,
                "drain_ms": drain_ms,
                "requires_process_exit": true,
            })));
        }
        {
            let mut state = self.shared.borrow_mut();
            state.registrations.clear();
            state.observers.clear();
            state.clients.clear();
            state.profiles.clear();
            state.pending_devtools.clear();
        }
        let shutdown_start = Instant::now();
        shutdown();
        let shutdown_ms = shutdown_start.elapsed().as_millis();
        drop(self._args);
        drop(self.library);
        Ok(ShutdownReport {
            remaining_targets: 0,
            drain_ms,
            shutdown_ms,
        })
    }

    fn check_navigation(&self, url: &str) -> Result<(), BrowserError> {
        let allowed = (self.shared.borrow().allow)(url);
        if allowed {
            Ok(())
        } else {
            Err(BrowserError::new(
                ErrorClass::Forbidden,
                "URL is not allowed by the browser navigation policy",
            ))
        }
    }

    fn with_browser(
        &self,
        target: &TargetIdentity,
        action: impl FnOnce(&Browser),
    ) -> Result<(), BrowserError> {
        let state = self.shared.borrow();
        let entry = state
            .target(&target.target)
            .ok_or_else(target_unavailable)?;
        validate_identity(&entry.identity, target)?;
        let browser = entry.browser.clone();
        drop(state); // CEF can re-enter callbacks; never hold RefCell across it.
        action(&browser);
        Ok(())
    }
}

fn target_unavailable() -> BrowserError {
    BrowserError::new(ErrorClass::TargetUnavailable, "target is not available")
}

fn schema_error(error: impl std::fmt::Display) -> BrowserError {
    BrowserError::new(ErrorClass::Internal, error.to_string())
}

fn io_error(error: std::io::Error) -> BrowserError {
    BrowserError::new(ErrorClass::ProviderUnavailable, error.to_string())
}

/// Bounded backoff for hosts that only have `OnScheduleMessagePumpWork`.
pub fn schedule_deadline(delay_ms: i64) -> Instant {
    Instant::now() + Duration::from_millis(delay_ms.max(0) as u64)
}

pub fn profile_dir_for(data_root: &Path, scope: &ProfileScope) -> PathBuf {
    profile_paths(data_root, scope).root
}
