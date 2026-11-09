use nng::options::{Options, RecvTimeout, SendTimeout};
use nng::{Error, Protocol, Socket};
use rfd::{MessageDialog, MessageLevel};
use serde::Deserialize;
use std::fs::{self, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Deserialize)]
struct Settings {
    cris_tools_path: PathBuf,
    /// Port the app listens on. Typed as a number so `tcp://{host}:{port}` is
    /// always a valid URL (a quoted TOML string used to render with quotes).
    port: u16,
    /// Host/address of the app's listener. Defaults to IPv4 loopback to match
    /// the app's own 127.0.0.1 bind; `localhost` can resolve to IPv6 first on
    /// Windows and prevent the NNG connection.
    #[serde(default = "default_host")]
    host: String,
    /// How long to wait for the app to accept a command and reply, in
    /// milliseconds. Automations can take a while, so do not be too eager.
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// Directory containing `app/current.txt` and the versioned build zips.
    /// Defaults to the directory containing the executable, not the working
    /// directory: Dragon may launch the proxy with an arbitrary CWD.
    #[serde(default)]
    app_root: Option<PathBuf>,
    /// Refuse to send a command when the app's IPC token cannot be found,
    /// instead of falling back to the pre-token `run/<command>` protocol.
    /// Enabled by default because current CRIS Tools builds reject untokened
    /// commands. Set `require_token = false` only during a mixed-version
    /// rollout.
    #[serde(default = "default_require_token")]
    require_token: bool,
    /// Override the location of the app's `ipc.token`. Defaults to
    /// `%LOCALAPPDATA%\cris-tools\ipc.token` (`$XDG_RUNTIME_DIR` on Linux).
    #[serde(default)]
    ipc_token_file: Option<PathBuf>,
    /// Default for launching CRIS Tools automatically when it is not running.
    /// Overridden by the per-user preferences file when that has a value.
    #[serde(default)]
    launch_without_prompt: bool,
    /// Override the location of the per-user preferences file. Defaults to
    /// `%LOCALAPPDATA%\cris-tools\prefs.toml`.
    #[serde(default)]
    preferences_file: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn default_require_token() -> bool {
    true
}

/// Per-user preferences saved by the proxy. Separate from the shared config so
/// one user's choice does not affect everyone using the same deployment.
#[derive(Debug, Default, Deserialize)]
struct Preferences {
    /// `None` when the file does not set it (fall back to the config).
    #[serde(default)]
    launch_without_prompt: Option<bool>,
}

/// Share subdirectory holding the published builds and the `current.txt` pointer.
const APP_SUBDIR: &str = "app";
const CURRENT_FILE: &str = "current.txt";
/// Layout inside the extracted zip: `<root>/cris_tools/cris_tools.exe`.
const EXE_SUBDIR: &str = "cris_tools";
const EXE_NAME: &str = "cris_tools.exe";
/// Settings file name, resolved next to the executable (see `settings_path`).
const SETTINGS_FILE: &str = "cris_tools_proxy.toml";
/// Persistent proxy log, written under the per-user profile so Dragon-launched
/// processes leave a diagnosable trace. Override with `CRIS_TOOLS_PROXY_LOG`.
const LOG_FILE_NAME: &str = "proxy.log";
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Default NNG send/receive timeout. Automations can take a while, but a wedged
/// listener must not block the REQ socket (and therefore Dragon) forever.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// How long to keep retrying before concluding CRIS Tools is not running. This
/// covers the short window where the user has just started it.
const CONNECT_RETRY_WINDOW: Duration = Duration::from_secs(2);
/// A launched app can take a while to unpack/bind, so wait longer for it.
const APP_START_TIMEOUT: Duration = Duration::from_secs(30);
/// Short grace period for the app's token file around connection time.
const TOKEN_WAIT: Duration = Duration::from_secs(2);
/// Poll interval for connection/token readiness while waiting.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Directory containing this executable, falling back to the working directory
/// when the path cannot be resolved (e.g. `current_exe` fails).
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Locate `cris_tools_proxy.toml`. Prefer the directory beside the executable
/// so the proxy works no matter what working directory Dragon launches it in;
/// fall back to the working directory for development.
fn settings_path() -> PathBuf {
    let beside_exe = exe_dir().join(SETTINGS_FILE);
    if beside_exe.is_file() {
        return beside_exe;
    }
    PathBuf::from(SETTINGS_FILE)
}

/// Default persistent log path. Python's app log lives in the shared log
/// directory, but the proxy is commonly launched by Dragon with no console, so
/// it writes to the current user's profile instead.
fn default_log_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CRIS_TOOLS_PROXY_LOG") {
        return PathBuf::from(path);
    }
    for variable in ["LOCALAPPDATA", "XDG_STATE_HOME"] {
        if let Some(base) = std::env::var_os(variable) {
            return PathBuf::from(base).join("cris-tools").join(LOG_FILE_NAME);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("cris-tools")
            .join(LOG_FILE_NAME);
    }
    std::env::temp_dir().join(LOG_FILE_NAME)
}

/// Append one diagnostic line without letting a logging failure break the
/// command. The log rotates once at 2 MB, keeping one previous file.
fn log_line(level: &str, message: &str) {
    let path = default_log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(metadata) = fs::metadata(&path) {
        if metadata.len() > LOG_MAX_BYTES {
            let previous = path.with_extension("log.1");
            let _ = fs::remove_file(&previous);
            let _ = fs::rename(&path, previous);
        }
    }
    let timestamp = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown-time".to_string());
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "[{timestamp}] {level:<5} {message}");
    }
}

fn load_settings(path: &Path) -> Result<Settings, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read settings file {}: {e}", path.display()))?;

    // Parse the file contents and deserialize into the Settings struct
    Ok(toml::from_str(&contents)?)
}

/// Default location of the per-user preferences file. Kept next to the run
/// cache/token rather than beside the shared config so preferences do not leak
/// between users on the same workstation.
fn default_preferences_path() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(base).join("cris-tools").join("prefs.toml"));
    }
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(base).join("cris-tools").join("prefs.toml"));
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("cris-tools")
            .join("prefs.toml")
    })
}

/// Resolve the per-user preferences file, honouring an optional config
/// override. Falls back next to the config only when no home directory can be
/// found at all.
fn preferences_path(settings: &Settings, settings_dir: &Path) -> PathBuf {
    match &settings.preferences_file {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => settings_dir.join(path),
        None => default_preferences_path()
            .unwrap_or_else(|| settings_dir.join("cris_tools_proxy.prefs.toml")),
    }
}

/// Read the user's preferences, treating a missing or malformed file as empty.
fn read_preferences(path: &Path) -> Preferences {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
        Err(_) => Preferences::default(),
    }
}

/// Whether to launch without prompting. The per-user preference wins over the
/// shared config default when it is present.
fn launch_without_prompt(settings: &Settings, settings_dir: &Path) -> bool {
    let path = preferences_path(settings, settings_dir);
    read_preferences(&path)
        .launch_without_prompt
        .unwrap_or(settings.launch_without_prompt)
}

/// Persist `launch_without_prompt = true` in the per-user preferences file.
/// Editing the document (rather than re-serializing `Preferences`) keeps any
/// comments or other settings the user added.
fn save_launch_preference(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let is_new = contents.trim().is_empty();
    let mut document = contents
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    document["launch_without_prompt"] = toml_edit::value(true);

    let contents = if is_new {
        format!(
            "# Per-user preferences for cris_tools_proxy.\n# Delete this file to be prompted again.\n{document}"
        )
    } else {
        document.to_string()
    };
    fs::write(path, contents)
}

/// Base directory for the deployment share, relative `app_root` values in the
/// config are resolved against the settings file's directory.
fn app_root(settings: &Settings, settings_dir: &Path) -> PathBuf {
    match &settings.app_root {
        Some(root) if root.is_absolute() => root.clone(),
        // A relative `app_root` is relative to the config, not the CWD.
        Some(root) => settings_dir.join(root),
        // No `app_root`: the share is wherever the config lives.
        None => settings_dir.to_path_buf(),
    }
}

/// Best-effort removal of everything in the cache except the version we keep.
/// A build still in use by a running instance is locked on Windows, so the
/// delete simply fails and is retried on the next launch.
fn prune_old_versions(cache_root: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(cache_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let _ = fs::remove_dir_all(&path);
    }
}

/// Root of the per-user run cache, e.g. `%LOCALAPPDATA%\cris-tools\run`.
fn local_cache_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("cris-tools").join("run"))
}

/// Per-user lock file used to serialise launches between proxy processes.
fn launch_lock_path() -> PathBuf {
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(base).join("cris-tools").join("launch.lock");
    }
    if let Some(base) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(base).join("cris-tools").join("launch.lock");
    }
    std::env::temp_dir().join("cris-tools-launch.lock")
}

/// Held for as long as this process owns the launch. Dropping it releases the
/// OS file lock, including if the process exits unexpectedly.
struct LaunchLock {
    _file: fs::File,
}

/// Try to become the process responsible for launching CRIS Tools. Returns
/// `Ok(None)` when another proxy process is already launching it.
fn try_acquire_launch_lock(path: &Path) -> std::io::Result<Option<LaunchLock>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(LaunchLock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Versions become directory and file names in the cache, so only allow the
/// characters used by normal version strings.
fn is_safe_version(version: &str) -> bool {
    !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

fn current_version(app_root: &Path) -> Option<String> {
    let path = app_root.join(APP_SUBDIR).join(CURRENT_FILE);
    let contents = fs::read_to_string(&path).ok()?;
    // Tolerate a UTF-8 BOM from editors on the share.
    let version = contents
        .trim()
        .trim_start_matches('\u{feff}')
        .trim()
        .to_string();
    if !is_safe_version(&version) {
        eprintln!(
            "Ignoring invalid version in {}: {:?}",
            path.display(),
            version
        );
        return None;
    }
    Some(version)
}

/// Build the listener URL. Bare IPv6 addresses need brackets in a URL.
fn socket_url(host: &str, port: u16) -> String {
    if host.starts_with('[') || !host.contains(':') {
        format!("tcp://{host}:{port}")
    } else {
        format!("tcp://[{host}]:{port}")
    }
}

fn connect(socket: &Socket, settings: &Settings) -> Result<(), Error> {
    socket.dial(&socket_url(&settings.host, settings.port))
}

/// Try to connect for a short window before treating CRIS Tools as not running.
/// Without this, a just-started app would produce a spurious launch prompt.
fn connect_with_retry(socket: &Socket, settings: &Settings) -> Result<(), Error> {
    let deadline = Instant::now() + CONNECT_RETRY_WINDOW;
    loop {
        match connect(socket, settings) {
            Ok(()) => return Ok(()),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => thread::sleep(READY_POLL_INTERVAL),
        }
    }
}

/// After launching, wait for the listener (and, when required, the IPC token)
/// to become ready so the original command can still be delivered. When
/// `previous_token` is provided, a stale token left by a crashed run is ignored
/// until the newly launched app publishes a different one.
fn wait_for_app(
    socket: &Socket,
    settings: &Settings,
    settings_dir: &Path,
    previous_token: Option<&str>,
) -> Result<(), String> {
    let deadline = Instant::now() + APP_START_TIMEOUT;
    loop {
        match connect(socket, settings) {
            Ok(()) => break,
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "CRIS Tools did not start within {} seconds ({e:?}).",
                        APP_START_TIMEOUT.as_secs()
                    ));
                }
                thread::sleep(READY_POLL_INTERVAL);
            }
        }
    }

    // The token is written after the listener starts, so it can lag slightly.
    if settings.require_token {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if wait_for_token(settings, settings_dir, remaining, previous_token).is_none() {
            return Err(format!(
                "CRIS Tools started, but its IPC token did not appear within {} seconds.",
                APP_START_TIMEOUT.as_secs()
            ));
        }
    }

    Ok(())
}

/// Default path of the per-run IPC token written by CRIS Tools. Mirrors
/// `cris_tools.ipc_token_path()`: prefer the per-user profile so other users on
/// the same workstation cannot read it.
fn default_ipc_token_path() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(base).join("cris-tools").join("ipc.token"));
    }
    if let Some(base) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(base).join("cris-tools").join("ipc.token"));
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".cache")
            .join("cris-tools")
            .join("ipc.token")
    })
}

/// Resolve the IPC token path, honouring an optional config override.
fn ipc_token_path(settings: &Settings, settings_dir: &Path) -> Option<PathBuf> {
    match &settings.ipc_token_file {
        Some(path) if path.is_absolute() => Some(path.clone()),
        Some(path) => Some(settings_dir.join(path)),
        None => default_ipc_token_path(),
    }
}

/// Read the token CRIS Tools published for this run, if any.
fn read_ipc_token(settings: &Settings, settings_dir: &Path) -> Option<String> {
    let path = ipc_token_path(settings, settings_dir)?;
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim().to_string();
    (!token.is_empty()).then_some(token)
}

/// Poll for the IPC token for a short window. The app writes it around the time
/// the listener starts, so it can lag a connection attempt slightly. A token
/// equal to `previous_token` is treated as stale so a crashed run's file cannot
/// be mistaken for the newly launched app's token.
fn wait_for_token(
    settings: &Settings,
    settings_dir: &Path,
    timeout: Duration,
    previous_token: Option<&str>,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(token) = read_ipc_token(settings, settings_dir) {
            if previous_token != Some(token.as_str()) {
                return Some(token);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(READY_POLL_INTERVAL);
    }
}

/// Build the wire message. The tokened form is `run/<token>/<command>`; the
/// legacy form (`run/<command>`) is only used while no token is available.
fn command_message(token: Option<&str>, command: &str) -> String {
    match token {
        Some(token) => format!("run/{token}/{command}"),
        None => format!("run/{command}"),
    }
}

/// Windows-native progress dialog (`IProgressDialog`), shown while a build is
/// being downloaded and unpacked into the local cache. On non-Windows platforms
/// this is a no-op so the crate still checks/builds elsewhere.
#[cfg(windows)]
mod progress {
    use windows::core::{IUnknown, HSTRING};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        CLSID_ProgressDialog, IProgressDialog, PROGDLG_AUTOTIME, PROGDLG_NOMINIMIZE,
    };

    pub struct ProgressDialog {
        dialog: IProgressDialog,
    }

    impl ProgressDialog {
        /// Create and show the dialog, returning `None` if COM/dialog creation
        /// fails (the caller then simply proceeds without a progress UI).
        pub fn new(title: &str, line: &str) -> Option<Self> {
            unsafe {
                // Ignore the result: COM may already be initialised on this
                // thread (for example by the message dialog). We deliberately
                // never call CoUninitialize because the process is short-lived.
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

                let dialog: IProgressDialog = CoCreateInstance(
                    &CLSID_ProgressDialog,
                    None::<&IUnknown>,
                    CLSCTX_INPROC_SERVER,
                )
                .ok()?;

                // Start first: the remaining methods are documented to be called
                // on a running dialog.
                let flags = PROGDLG_AUTOTIME | PROGDLG_NOMINIMIZE;
                let _ = dialog.StartProgressDialog(None, None::<&IUnknown>, flags, None);

                let _ = dialog.SetTitle(&HSTRING::from(title));
                let _ = dialog.SetLine(2, &HSTRING::from(line), false, None);

                Some(Self { dialog })
            }
        }

        pub fn set_status(&self, text: &str) {
            unsafe {
                let _ = self.dialog.SetLine(1, &HSTRING::from(text), false, None);
            }
        }

        pub fn set_progress(&self, completed: u64, total: u64) {
            unsafe {
                let _ = self.dialog.SetProgress64(completed, total);
            }
        }

        pub fn cancelled(&self) -> bool {
            unsafe { self.dialog.HasUserCancelled().as_bool() }
        }
    }

    impl Drop for ProgressDialog {
        fn drop(&mut self) {
            unsafe {
                let _ = self.dialog.StopProgressDialog();
            }
        }
    }
}

#[cfg(not(windows))]
mod progress {
    /// No-op fallback for non-Windows builds.
    pub struct ProgressDialog;

    impl ProgressDialog {
        pub fn new(_title: &str, _line: &str) -> Option<Self> {
            None
        }
        pub fn set_status(&self, _text: &str) {}
        pub fn set_progress(&self, _completed: u64, _total: u64) {}
        pub fn cancelled(&self) -> bool {
            false
        }
    }
}

/// Extract `zip_path` into `dest`, reporting progress to `progress` if present.
///
/// This replaces `ZipArchive::extract` so the one-time cache population can show
/// determinate progress and honour the dialog's Cancel button. Errors are
/// `io::Error`s so the cancellation case is `ErrorKind::Interrupted`.
fn extract_zip(
    zip_path: &Path,
    dest: &Path,
    progress: Option<&progress::ProgressDialog>,
) -> std::io::Result<()> {
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(std::io::Error::other)?;

    // Sum of uncompressed sizes, used as the progress bar's total.
    let total: u64 = (0..archive.len())
        .map(|i| archive.by_index(i).map(|entry| entry.size()).unwrap_or(0))
        .sum();

    if let Some(dialog) = progress {
        dialog.set_progress(0, total);
        dialog.set_status("Preparing CRIS Tools...");
    }

    let mut done: u64 = 0;
    for i in 0..archive.len() {
        if let Some(dialog) = progress {
            if dialog.cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "cancelled by user",
                ));
            }
        }

        let mut entry = archive.by_index(i).map_err(std::io::Error::other)?;
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        let out_path = dest.join(relative);

        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut out = fs::File::create(&out_path)?;
        let copied = std::io::copy(&mut entry, &mut out)?;
        done += copied;

        if let Some(dialog) = progress {
            dialog.set_progress(done, total);
        }
    }

    Ok(())
}

/// Ensure the currently published build is present in the local cache and
/// return the path to its executable.
///
/// `Ok(None)` means there is nothing to cache (legacy layout, no cache dir,
/// missing zip, ...); the caller then falls back to launching `cris_tools_path`
/// directly, preserving the old behaviour. `Err` means a published build was
/// found but could not be prepared, so silently falling back would hide the
/// problem. `Err(Interrupted)` means the user cancelled the progress dialog and
/// the caller must not launch anything.
///
/// Fetching and unpacking from the share on first use (rather than running from
/// the share) is what keeps startup fast and lets aggressive endpoint AV leave
/// the app alone: the process image and its DLLs are already local and no
/// per-launch extraction into `%TEMP%` happens.
fn ensure_cached_build(app_root: &Path) -> Result<Option<PathBuf>, std::io::Error> {
    let Some(version) = current_version(app_root) else {
        return Ok(None);
    };
    let Some(cache_root) = local_cache_root() else {
        return Ok(None);
    };
    let version_dir = cache_root.join(&version);
    let exe = version_dir.join(EXE_SUBDIR).join(EXE_NAME);
    if exe.is_file() {
        prune_old_versions(&cache_root, &version_dir);
        return Ok(Some(exe));
    }

    let zip_path = app_root
        .join(APP_SUBDIR)
        .join(format!("cris_tools_{version}.zip"));
    if !zip_path.is_file() {
        return Ok(None);
    }

    println!("Caching CRIS Tools {version} to {}", version_dir.display());
    fs::create_dir_all(&cache_root)?;

    // Extract into a staging directory first, then rename, so a half-finished
    // extraction is never mistaken for a valid cache. The progress dialog is
    // shown only for this one-time cost.
    let staging = cache_root.join(format!(".staging-{version}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    // The progress dialog is scoped to the extraction so it closes before we
    // rename the build into place and launch it.
    let extract_result = {
        let dialog =
            progress::ProgressDialog::new("Preparing CRIS Tools", &format!("Version {version}"));
        extract_zip(&zip_path, &staging, dialog.as_ref())
    };

    if let Err(e) = extract_result {
        let _ = fs::remove_dir_all(&staging);
        // Cancellation is not a partial success: surface it so the caller does
        // not launch a build the user just declined to prepare.
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Err(e);
        }
        return Err(std::io::Error::new(
            e.kind(),
            format!("Failed to extract {}: {e}", zip_path.display()),
        ));
    }

    if let Err(rename_error) = fs::rename(&staging, &version_dir) {
        // Another instance may have populated the cache first; fall through and
        // check whether the exe is now present.
        let _ = fs::remove_dir_all(&staging);
        if !exe.is_file() {
            return Err(std::io::Error::new(
                rename_error.kind(),
                format!(
                    "Failed to install CRIS Tools {version} in {}: {rename_error}",
                    version_dir.display()
                ),
            ));
        }
    }

    if exe.is_file() {
        prune_old_versions(&cache_root, &version_dir);
        Ok(Some(exe))
    } else {
        Err(std::io::Error::other(format!(
            "Extracted {} but {} is missing.",
            zip_path.display(),
            exe.display()
        )))
    }
}

fn spawn(exe: &Path, cwd: Option<&Path>, data_dir: Option<&Path>) -> std::io::Result<Child> {
    let mut command = Command::new(exe);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    if let Some(dir) = data_dir {
        // Keep config/, plugins/ and logs on the share even though the code
        // runs from the local cache.
        command.env("CRIS_TOOLS_DATA_DIR", dir);
    }
    command.spawn()
}

/// Launch CRIS Tools, preferring a locally cached build.
fn launch_cris_tools(settings: &Settings, settings_dir: &Path) -> std::io::Result<Child> {
    let root = app_root(settings, settings_dir);
    let root = fs::canonicalize(&root).unwrap_or(root);

    match ensure_cached_build(&root)? {
        Some(exe) => {
            println!("Launching cached build: {}", exe.display());
            log_line(
                "INFO",
                &format!("Launching cached build: {}", exe.display()),
            );
            spawn(&exe, Some(&root), Some(&root))
        }
        None => {
            println!("Launching: {}", settings.cris_tools_path.display());
            log_line(
                "INFO",
                &format!(
                    "Launching configured executable: {}",
                    settings.cris_tools_path.display()
                ),
            );
            spawn(&settings.cris_tools_path, None, None)
        }
    }
}

/// Show an error to the user. The proxy is normally started by Dragon with no
/// visible console, so logging alone is not enough.
fn show_error(title: &str, description: &str) {
    eprintln!("{title}: {description}");
    log_line("ERROR", &format!("{title}: {description}"));
    MessageDialog::new()
        .set_level(MessageLevel::Error)
        .set_title(title)
        .set_description(description)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// Decide whether to launch CRIS Tools, prompting unless the user previously
/// chose to always launch. Returns true when the caller should launch.
fn confirm_launch(settings: &Settings, settings_dir: &Path) -> bool {
    if launch_without_prompt(settings, settings_dir) {
        log_line("INFO", "Launching without prompting (user preference)");
        return true;
    }

    let choice = MessageDialog::new()
        .set_title("CRIS Tools not running")
        .set_description("For advanced voice commands to function CRIS Tools needs to be running.\nDo you want to launch it?")
        .set_buttons(rfd::MessageButtons::YesNo)
        .show();

    if choice != rfd::MessageDialogResult::Yes {
        log_line("INFO", "User declined to launch CRIS Tools");
        return false;
    }

    // A second confirmation rather than a third button: rfd's custom-button
    // dialogs need a Windows common-controls v6 manifest, which the release
    // build does not embed.
    let remember = MessageDialog::new()
        .set_title("Launch CRIS Tools automatically?")
        .set_description("Do you want to always launch CRIS Tools without asking?\n\nThis preference is saved for your user account only.")
        .set_buttons(rfd::MessageButtons::YesNo)
        .show();

    if remember == rfd::MessageDialogResult::Yes {
        let path = preferences_path(settings, settings_dir);
        if let Err(e) = save_launch_preference(&path) {
            show_error(
                "Could not save preference",
                &format!(
                    "Failed to save the launch preference to {}:\n{e}",
                    path.display()
                ),
            );
        } else {
            log_line(
                "INFO",
                &format!("Saved launch preference to {}", path.display()),
            );
        }
    }

    true
}

/// Launch and report failures in a dialog, since the console is hidden when the
/// proxy is run by Dragon. Returns true when the process was started.
fn launch_and_report(settings: &Settings, settings_dir: &Path, config_path: &Path) -> bool {
    match launch_cris_tools(settings, settings_dir) {
        Ok(_) => {
            println!("CRIS Tools launched successfully");
            log_line("INFO", "CRIS Tools process started");
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            println!("CRIS Tools launch cancelled");
            log_line("INFO", "CRIS Tools launch cancelled");
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            show_error(
                "CRIS Tools not found",
                &format!(
                    "CRIS Tools could not be found. Check that the published build exists under app/ or that cris_tools_path in {} points to the CRIS Tools executable.",
                    config_path.display()
                ),
            );
            false
        }
        Err(e) => {
            show_error(
                "Failed to launch CRIS Tools",
                &format!("CRIS Tools could not be launched:\n{e}"),
            );
            false
        }
    }
}

/// Launch CRIS Tools, serialising launches so simultaneous commands cannot
/// start several instances. Returns true when the app is ready.
fn launch_once(
    socket: &Socket,
    settings: &Settings,
    settings_dir: &Path,
    config_path: &Path,
    allow_prompt: bool,
) -> bool {
    let lock_path = launch_lock_path();
    let _lock = match try_acquire_launch_lock(&lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            // Another proxy process is already launching it. Wait for that
            // launch instead of starting a second instance. We cannot know the
            // previous token safely here, so accept the current one.
            println!("Another CRIS Tools launch is in progress; waiting...");
            return match wait_for_app(socket, settings, settings_dir, None) {
                Ok(()) => true,
                Err(message) => {
                    show_error("CRIS Tools did not start", &message);
                    false
                }
            };
        }
        Err(e) => {
            show_error(
                "Failed to check launch status",
                &format!(
                    "Could not create the launch lock {}:\n{e}",
                    lock_path.display()
                ),
            );
            return false;
        }
    };

    if allow_prompt && !confirm_launch(settings, settings_dir) {
        println!("User chose not to launch CRIS Tools");
        return false;
    }

    // A crashed run can leave its token behind. Remember it so the wait below
    // does not accept the stale file before the new process writes its token.
    let previous_token = read_ipc_token(settings, settings_dir);

    if !launch_and_report(settings, settings_dir, config_path) {
        return false;
    }

    match wait_for_app(socket, settings, settings_dir, previous_token.as_deref()) {
        Ok(()) => true,
        Err(message) => {
            show_error("CRIS Tools did not start", &message);
            false
        }
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        log_line("PANIC", &format!("{info}"));
    }));
    log_line(
        "INFO",
        &format!(
            "cris_tools_proxy {} starting (log: {})",
            env!("CARGO_PKG_VERSION"),
            default_log_path().display()
        ),
    );

    // With no command the proxy's only useful action is to make sure CRIS
    // Tools is running, so launch it without prompting in that case.
    let command = std::env::args().nth(1).filter(|arg| !arg.is_empty());
    match &command {
        Some(command) => log_line("INFO", &format!("Requested command: {command}")),
        None => log_line(
            "INFO",
            "No command supplied; ensuring CRIS Tools is running",
        ),
    }

    let config_path = settings_path();
    log_line("INFO", &format!("Settings path: {}", config_path.display()));
    let settings_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let settings = match load_settings(&config_path) {
        Ok(settings) => settings,
        Err(e) => {
            show_error(
                "Failed to load settings",
                &format!("{e}\n\nExpected settings in {}", config_path.display()),
            );
            std::process::exit(1);
        }
    };
    let token_path = ipc_token_path(&settings, &settings_dir);
    log_line(
        "INFO",
        &format!(
            "Settings: host={}, port={}, require_token={}, timeout_ms={}, app_root={:?}",
            settings.host,
            settings.port,
            settings.require_token,
            settings.timeout_ms,
            settings.app_root
        ),
    );
    log_line(
        "INFO",
        &format!(
            "IPC token path: {}",
            token_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unavailable>".to_string())
        ),
    );

    // Create a socket of type REQ (request)
    let socket = match Socket::new(Protocol::Req0) {
        Ok(socket) => socket,
        Err(e) => {
            show_error(
                "Failed to create socket",
                &format!("Could not create the command socket:\n{e:?}"),
            );
            std::process::exit(1);
        }
    };

    // Bound how long we wait on the app. Without this a wedged listener would
    // block the REQ socket - and therefore Dragon - indefinitely.
    let timeout = Duration::from_millis(settings.timeout_ms);
    let _ = socket.set_opt::<SendTimeout>(Some(timeout));
    let _ = socket.set_opt::<RecvTimeout>(Some(timeout));

    // Connect to the NNG server, allowing a short window for an app that is
    // already starting up before deciding it is not running.
    let launch_only = command.is_none();
    log_line(
        "INFO",
        &format!(
            "Connecting to {}",
            socket_url(&settings.host, settings.port)
        ),
    );
    match connect_with_retry(&socket, &settings) {
        Ok(()) => {
            println!("Connected to server");
            log_line("INFO", "Connected to CRIS Tools listener");
            if launch_only {
                println!("CRIS Tools is already running");
                log_line("INFO", "CRIS Tools is already running");
                return;
            }
        }
        Err(e) => {
            println!("Failed to connect to server: {:?}", e);
            log_line(
                "WARN",
                &format!("Could not connect to CRIS Tools listener: {e:?}"),
            );

            // Serialise launches so simultaneous commands cannot start
            // several CRIS Tools instances.
            if !launch_once(
                &socket,
                &settings,
                &settings_dir,
                &config_path,
                !launch_only,
            ) {
                log_line("ERROR", "Launch/wait failed; exiting");
                std::process::exit(1);
            }
            println!("CRIS Tools is ready");
            log_line("INFO", "CRIS Tools is ready");

            if launch_only {
                return;
            }
        }
    }

    let Some(arg) = command else { return };

    // Authenticate with the token CRIS Tools wrote for this run. If it is
    // missing and the config requires one, never fall back to the legacy form.
    let token = if settings.require_token {
        wait_for_token(&settings, &settings_dir, TOKEN_WAIT, None)
    } else {
        read_ipc_token(&settings, &settings_dir)
    };
    if let Some(token) = &token {
        log_line(
            "INFO",
            &format!("IPC token read successfully ({} characters)", token.len()),
        );
    } else if settings.require_token {
        log_line("ERROR", "IPC token not found; command not sent");
        show_error(
            "CRIS Tools IPC token not found",
            "CRIS Tools is running but has not published its IPC token. Is it fully started?",
        );
        std::process::exit(1);
    } else {
        log_line(
            "WARN",
            "IPC token not found; sending legacy untokened command",
        );
    }

    // Send the argument to the server
    let message = command_message(token.as_deref(), &arg);
    log_line(
        "INFO",
        &format!(
            "Sending {} command: {arg}",
            if token.is_some() { "tokened" } else { "legacy" }
        ),
    );
    if let Err(e) = socket.send(message.as_bytes()) {
        show_error(
            "Failed to send command",
            &format!("Could not send the command to CRIS Tools:\n{e:?}"),
        );
        std::process::exit(1);
    }

    // Receive the response from the server
    match socket.recv() {
        Ok(response) => match String::from_utf8(response.to_vec()) {
            Ok(response) => {
                log_line(
                    "INFO",
                    &format!(
                        "Response: {}",
                        response.chars().take(200).collect::<String>()
                    ),
                );
                println!("Response: {}", response)
            }
            Err(_) => {
                show_error(
                    "Invalid response",
                    "CRIS Tools returned a response that was not valid UTF-8.",
                );
                std::process::exit(1);
            }
        },
        Err(Error::TimedOut) => {
            show_error(
                "CRIS Tools did not respond",
                &format!(
                    "No response from CRIS Tools within {} ms.",
                    settings.timeout_ms
                ),
            );
            std::process::exit(1);
        }
        Err(e) => {
            show_error(
                "Failed to receive response",
                &format!("Could not read the response from CRIS Tools:\n{e:?}"),
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokened_message_includes_token() {
        assert_eq!(
            command_message(Some("abc123"), "load_event"),
            "run/abc123/load_event"
        );
    }

    #[test]
    fn legacy_message_has_no_token() {
        assert_eq!(command_message(None, "load_event"), "run/load_event");
    }

    #[test]
    fn token_file_prefers_localappdata() {
        std::env::set_var("LOCALAPPDATA", "/tmp/localappdata");
        std::env::remove_var("XDG_RUNTIME_DIR");
        assert_eq!(
            default_ipc_token_path(),
            Some(PathBuf::from("/tmp/localappdata/cris-tools/ipc.token"))
        );
        std::env::remove_var("LOCALAPPDATA");
    }

    #[test]
    fn missing_token_file_reads_none() {
        let settings = Settings {
            cris_tools_path: PathBuf::from("missing"),
            port: 5555,
            host: default_host(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            app_root: None,
            require_token: false,
            ipc_token_file: Some(PathBuf::from("definitely-not-here.ipc")),
            launch_without_prompt: false,
            preferences_file: None,
        };
        assert!(read_ipc_token(&settings, Path::new(".")).is_none());
    }

    #[test]
    fn token_file_is_read_and_trimmed() {
        let dir = std::env::temp_dir().join(format!("proxy-token-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ipc.token");
        fs::write(&path, "  deadbeef\n").unwrap();

        let settings = Settings {
            cris_tools_path: PathBuf::from("missing"),
            port: 5555,
            host: default_host(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            app_root: None,
            require_token: false,
            ipc_token_file: Some(path.clone()),
            launch_without_prompt: false,
            preferences_file: None,
        };
        assert_eq!(
            read_ipc_token(&settings, Path::new(".")).as_deref(),
            Some("deadbeef")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_preference_is_saved_without_losing_comments() {
        let dir = std::env::temp_dir().join(format!("proxy-prefs-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.toml");
        fs::write(
            &path,
            "# keep this comment\nlaunch_without_prompt = false\n",
        )
        .unwrap();

        save_launch_preference(&path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("# keep this comment"));
        let prefs: Preferences = toml::from_str(&contents).unwrap();
        assert_eq!(prefs.launch_without_prompt, Some(true));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_preference_file_gets_header_comment() {
        let dir = std::env::temp_dir().join(format!("proxy-prefs-new-test-{}", std::process::id()));
        let path = dir.join("prefs.toml");

        save_launch_preference(&path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("# Per-user preferences for cris_tools_proxy."));
        let prefs: Preferences = toml::from_str(&contents).unwrap();
        assert_eq!(prefs.launch_without_prompt, Some(true));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_preference_overrides_config_default() {
        let dir =
            std::env::temp_dir().join(format!("proxy-prefs-override-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let prefs_path = dir.join("prefs.toml");

        let mut settings = Settings {
            cris_tools_path: PathBuf::from("missing"),
            port: 5555,
            host: default_host(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            app_root: None,
            require_token: false,
            ipc_token_file: None,
            launch_without_prompt: true,
            preferences_file: Some(prefs_path.clone()),
        };

        // No per-user value: the config default applies.
        assert!(launch_without_prompt(&settings, &dir));

        // An explicit per-user false overrides a config true.
        fs::write(&prefs_path, "launch_without_prompt = false\n").unwrap();
        assert!(!launch_without_prompt(&settings, &dir));

        // An explicit per-user true overrides a config false.
        settings.launch_without_prompt = false;
        fs::write(&prefs_path, "launch_without_prompt = true\n").unwrap();
        assert!(launch_without_prompt(&settings, &dir));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn socket_url_uses_ipv4_loopback_by_default() {
        assert_eq!(socket_url(&default_host(), 5555), "tcp://127.0.0.1:5555");
    }

    #[test]
    fn socket_url_brackets_ipv6_hosts() {
        assert_eq!(socket_url("::1", 5555), "tcp://[::1]:5555");
        assert_eq!(socket_url("[::1]", 5555), "tcp://[::1]:5555");
    }

    #[test]
    fn version_validation_rejects_path_characters() {
        assert!(is_safe_version("0.1.3"));
        assert!(is_safe_version("1.2.3-beta.1+build_5"));
        assert!(!is_safe_version(""));
        assert!(!is_safe_version("../../evil"));
        assert!(!is_safe_version("1.2.3/other"));
        assert!(!is_safe_version("1.2.3 "));
    }

    #[test]
    fn current_version_rejects_unsafe_values_and_strips_bom() {
        let dir = std::env::temp_dir().join(format!("proxy-version-test-{}", std::process::id()));
        let app_dir = dir.join(APP_SUBDIR);
        fs::create_dir_all(&app_dir).unwrap();
        let path = app_dir.join(CURRENT_FILE);

        fs::write(&path, "../../evil").unwrap();
        assert_eq!(current_version(&dir), None);

        fs::write(&path, "\u{feff}0.1.3\n").unwrap();
        assert_eq!(current_version(&dir), Some("0.1.3".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_for_token_ignores_previous_value() {
        let dir =
            std::env::temp_dir().join(format!("proxy-token-wait-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ipc.token");
        fs::write(&path, "old-token\n").unwrap();

        let settings = Settings {
            cris_tools_path: PathBuf::from("missing"),
            port: 5555,
            host: default_host(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            app_root: None,
            require_token: true,
            ipc_token_file: Some(path.clone()),
            launch_without_prompt: false,
            preferences_file: None,
        };

        assert_eq!(
            wait_for_token(
                &settings,
                Path::new("."),
                Duration::from_millis(20),
                Some("old-token"),
            ),
            None
        );
        assert_eq!(
            wait_for_token(&settings, Path::new("."), Duration::from_millis(20), None,).as_deref(),
            Some("old-token")
        );

        fs::write(&path, "new-token\n").unwrap();
        assert_eq!(
            wait_for_token(
                &settings,
                Path::new("."),
                Duration::from_millis(20),
                Some("old-token"),
            )
            .as_deref(),
            Some("new-token")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_default_to_ipv4_and_thirty_seconds() {
        let settings: Settings =
            toml::from_str("cris_tools_path = \"cris_tools.exe\"\nport = 5555\n").unwrap();
        assert_eq!(settings.host, "127.0.0.1");
        assert_eq!(settings.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(settings.require_token);
    }

    #[test]
    fn launch_lock_allows_only_one_holder() {
        let dir = std::env::temp_dir().join(format!("proxy-lock-test-{}", std::process::id()));
        let path = dir.join("launch.lock");

        let first = try_acquire_launch_lock(&path).unwrap();
        assert!(first.is_some());
        assert!(try_acquire_launch_lock(&path).unwrap().is_none());

        drop(first);
        let third = try_acquire_launch_lock(&path).unwrap();
        assert!(third.is_some());

        drop(third);
        let _ = fs::remove_dir_all(&dir);
    }
}
