use nng::{Protocol, Socket, Error};
use rfd::MessageDialog;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

#[derive(Debug, Deserialize)]
struct Settings {
    cris_tools_path: PathBuf,
    port: toml::Value,
    /// Directory containing `app/current.txt` and the versioned build zips.
    /// Defaults to the working directory, which is the deployment share.
    #[serde(default)]
    app_root: Option<PathBuf>,
}

/// Share subdirectory holding the published builds and the `current.txt` pointer.
const APP_SUBDIR: &str = "app";
const CURRENT_FILE: &str = "current.txt";
/// Layout inside the extracted zip: `<root>/cris_tools/cris_tools.exe`.
const EXE_SUBDIR: &str = "cris_tools";
const EXE_NAME: &str = "cris_tools.exe";

fn load_settings() -> Result<Settings, toml::de::Error> {
    // Read the entire contents of the file
    let contents = fs::read_to_string("cris_tools_proxy.toml")
        .expect("Failed to read settings file");

    // Parse the file contents and deserialize into the Settings struct
    toml::from_str(&contents)
}

fn app_root(settings: &Settings) -> PathBuf {
    settings
        .app_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."))
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

fn current_version(app_root: &Path) -> Option<String> {
    let contents = fs::read_to_string(app_root.join(APP_SUBDIR).join(CURRENT_FILE)).ok()?;
    let version = contents.trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// Windows-native progress dialog (`IProgressDialog`), shown while a build is
/// being downloaded and unpacked into the local cache. On non-Windows platforms
/// this is a no-op so the crate still checks/builds elsewhere.
#[cfg(windows)]
mod progress {
    use windows::core::{HSTRING, IUnknown};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        IProgressDialog, CLSID_ProgressDialog, PROGDLG_AUTOTIME, PROGDLG_NOMINIMIZE,
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
/// determinate progress and honour the dialog's Cancel button.
fn extract_zip(
    zip_path: &Path,
    dest: &Path,
    progress: Option<&progress::ProgressDialog>,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

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
                )
                .into());
            }
        }

        let mut entry = archive.by_index(i)?;
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
/// Returns `None` when there is nothing to cache (legacy layout, no cache dir,
/// missing zip, ...); the caller then falls back to launching `cris_tools_path`
/// directly, preserving the old behaviour.
///
/// Fetching and unpacking from the share on first use (rather than running from
/// the share) is what keeps startup fast and lets aggressive endpoint AV leave
/// the app alone: the process image and its DLLs are already local and no
/// per-launch extraction into `%TEMP%` happens.
fn ensure_cached_build(app_root: &Path) -> Option<PathBuf> {
    let version = current_version(app_root)?;
    let cache_root = local_cache_root()?;
    let version_dir = cache_root.join(&version);
    let exe = version_dir.join(EXE_SUBDIR).join(EXE_NAME);
    if exe.is_file() {
        prune_old_versions(&cache_root, &version_dir);
        return Some(exe);
    }

    let zip_path = app_root
        .join(APP_SUBDIR)
        .join(format!("cris_tools_{version}.zip"));
    if !zip_path.is_file() {
        return None;
    }

    println!("Caching CRIS Tools {version} to {}", version_dir.display());
    fs::create_dir_all(&cache_root).ok()?;

    // Extract into a staging directory first, then rename, so a half-finished
    // extraction is never mistaken for a valid cache. The progress dialog is
    // shown only for this one-time cost.
    let staging = cache_root.join(format!(".staging-{version}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).ok()?;

    // The progress dialog is scoped to the extraction so it closes before we
    // rename the build into place and launch it.
    let extract_result = {
        let dialog =
            progress::ProgressDialog::new("Preparing CRIS Tools", &format!("Version {version}"));
        extract_zip(&zip_path, &staging, dialog.as_ref())
    };

    if let Err(e) = extract_result {
        eprintln!("Failed to extract {zip_path:?}: {e:?}");
        let _ = fs::remove_dir_all(&staging);
        return None;
    }

    if fs::rename(&staging, &version_dir).is_err() {
        // Another instance may have populated the cache first; fall through and
        // check whether the exe is now present.
        let _ = fs::remove_dir_all(&staging);
    }

    if exe.is_file() {
        prune_old_versions(&cache_root, &version_dir);
        Some(exe)
    } else {
        None
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
fn launch_cris_tools(settings: &Settings) -> std::io::Result<Child> {
    let root = app_root(settings);
    let root = fs::canonicalize(&root).unwrap_or(root);

    match ensure_cached_build(&root) {
        Some(exe) => {
            println!("Launching cached build: {}", exe.display());
            spawn(&exe, Some(&root), Some(&root))
        }
        None => {
            println!("Launching: {}", settings.cris_tools_path.display());
            spawn(&settings.cris_tools_path, None, None)
        }
    }
}

fn main() {
    let settings = match load_settings() {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("Failed to load settings: {:?}", e);
            std::process::exit(1);
        }
    };

    // Create a socket of type REQ (request)
    let socket = Socket::new(Protocol::Req0).expect("Failed to create socket");

    // Connect to the NNG server
    match socket.dial(format!("tcp://localhost:{}", settings.port).as_str()) {
        Ok(_) => {
            println!("Connected to server");
        }
        Err(e) => {
            println!("Failed to connect to server: {:?}", e);
            let choice = MessageDialog::new()
                .set_title("CRIS Tools not running")
                .set_description("For advanced voice commands to function CRIS Tools needs to be running.\nDo you want to launch it?")
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();

            match choice {
                rfd::MessageDialogResult::Yes => {
                    println!("User chose Yes");
                    match launch_cris_tools(&settings) {
                        Ok(_) => {
                            println!("CRIS Tools launched successfully");
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            // Handle the NotFound error here
                            println!("CRIS Tools not found");
                            eprintln!("Failed to launch CRIS Tools: {:?}", e);
                        }
                        Err(e) => {
                            eprintln!("Failed to launch CRIS Tools: {:?}", e);
                        }
                    }
                },
                rfd::MessageDialogResult::No => println!("User chose No"),
                _ => println!("User closed or cancelled the dialog box"),
            }

            std::process::exit(1);
        }
    }

    // Get the command line argument
    //let arg = std::env::args().nth(1).expect("Missing argument");
    let arg = match std::env::args().nth(1) {
        Some(arg) => arg,
        None => {
            eprintln!("Missing argument");
            std::process::exit(1);
        }
    };

    // Send the argument to the server
    let message = "run/".to_string() + &arg;
    socket.send(message.as_bytes()).expect("Failed to send message");

    // Receive the response from the server
    match socket.recv() {
        Ok(response) => {
            let response = String::from_utf8(response.to_vec()).expect("Failed to receive response");
            println!("Response: {}", response);
        }
        Err(Error::TimedOut) => {
            println!("Failed to receive response: Timeout");
        }
        Err(e) => {
            println!("Failed to receive response: {:?}", e);
        }
    }
}
