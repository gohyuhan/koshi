//! `cleanup` domain — terminal restoration that survives panics.
//!
//! Koshi puts the terminal into raw mode and the alternate screen while it runs.
//! If the process exits without undoing that — including an unwinding panic — the
//! user is left with a corrupted shell. [`cleanup::TerminalCleanupGuard`] guarantees the
//! undo: callers register cleanup hooks, and the hooks run exactly once on
//! whichever comes first — the guard being dropped, or a panic, if
//! [`cleanup::install_panic_hook`] armed one.
//!
//! An armed panic hook also writes a crash report to the directory the caller
//! names, after the cleanup hooks run.
//!
//! This module ships only the mechanism. The concrete hooks — disabling raw mode
//! and leaving the alternate screen via `crossterm` — are registered by the
//! runtime when it actually enters those modes, so this crate takes no terminal
//! dependency. Hooks are plain [`FnOnce`] closures here.

use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use koshi_core::recent_event::RecentEvent;

/// A one-shot terminal-cleanup action. Boxed and `Send` so it can be held in the
/// shared registry and run from either the dropping thread or the panic hook.
pub type CleanupHook = Box<dyn FnOnce() + Send>;

/// The hook registry, shared between the guard and any installed panic hook.
type Registry = Arc<Mutex<Vec<CleanupHook>>>;

#[cfg(test)]
pub(crate) fn panic_hook_test_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A shareable panic hook, so the installed chained hook and the
/// [`PanicHookGuard`] that restores it can both hold the prior hook.
type SharedPanicHook = Arc<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// One panic as the crash file records it: the time, message, place, recent
/// events, and stack. [`CrashReport::capture`] reads it on the panicking thread,
/// where the payload and the stack are still reachable; another thread writes
/// it.
struct CrashReport {
    /// Whole seconds since the Unix epoch, written in the report and filename.
    timestamp: u64,
    /// The panic message.
    message: String,
    /// `file:line:column` of the panic.
    location: String,
    /// The panicking thread's stack.
    backtrace: String,
    /// The content-free recent-event snapshot, or `None` when its ring was
    /// locked during the panic.
    recent_events: Option<Vec<RecentEvent>>,
}

impl CrashReport {
    /// Read one panic into an owned report.
    fn capture(info: &PanicHookInfo<'_>) -> CrashReport {
        let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).ok();
        CrashReport {
            timestamp: since_epoch.map_or(0, |since| since.as_secs()),
            message: info
                .payload_as_str()
                .unwrap_or("a panic with no message")
                .to_string(),
            location: info
                .location()
                .map_or_else(|| "unknown".to_string(), ToString::to_string),
            backtrace: std::backtrace::Backtrace::force_capture().to_string(),
            recent_events: crate::logging::recent_events::try_recent(),
        }
    }

    /// The file's text: one `field: value` line per fact, then the stack.
    fn text(&self) -> String {
        let recent_events = self
            .recent_events
            .as_deref()
            .map(recent_event_lines)
            .unwrap_or_default();
        format!(
            "version: {}\nplatform: {} {}\ntimestamp: {}\nmessage: {}\nlocation: {}\n{}backtrace:\n{}\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.timestamp,
            self.message,
            self.location,
            recent_events,
            self.backtrace,
        )
    }

    /// Write the report to a unique `crash-<timestamp>.txt` file. The directory
    /// and file are private to this user, and old reports are removed after the
    /// complete file becomes visible.
    fn write(&self, dir: &Path) {
        if koshi_paths::ensure_private_dir(dir).is_err() {
            return;
        }
        #[cfg(windows)]
        if ensure_windows_private(dir).is_err() {
            return;
        }

        use std::io::Write as _;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let Ok((temporary_path, mut file)) = open_temporary_report(dir) else {
            return;
        };
        #[cfg(unix)]
        if file
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .is_err()
        {
            let _ = std::fs::remove_file(&temporary_path);
            return;
        }
        if file.write_all(self.text().as_bytes()).is_err() {
            let _ = std::fs::remove_file(&temporary_path);
            return;
        }
        drop(file);

        let Some(_path) = publish_report(&temporary_path, dir, self.timestamp) else {
            let _ = std::fs::remove_file(&temporary_path);
            return;
        };
        let _ = std::fs::remove_file(&temporary_path);

        #[cfg(windows)]
        if ensure_windows_private(&_path).is_err() {
            let _ = std::fs::remove_file(&_path);
            return;
        }
        retain_crash_reports(dir);
    }
}

const MAX_CRASH_REPORTS: usize = 10;
static NEXT_REPORT_TEMPORARY_SUFFIX: AtomicU64 = AtomicU64::new(0);
static NEXT_REPORT_COLLISION_SUFFIX: AtomicU64 = AtomicU64::new(0);

/// Reserve a hidden temporary file without replacing another report.
fn open_temporary_report(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    for _ in 0..100 {
        let suffix = NEXT_REPORT_TEMPORARY_SUFFIX.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".crash-{}-{suffix}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a crash report file",
    ))
}

/// Publish a complete temporary report under a new crash-report name.
fn publish_report(temporary_path: &Path, dir: &Path, timestamp: u64) -> Option<PathBuf> {
    for attempt in 0..100 {
        let path = if attempt == 0 {
            dir.join(format!("crash-{timestamp}.txt"))
        } else {
            let suffix = NEXT_REPORT_COLLISION_SUFFIX.fetch_add(1, Ordering::Relaxed);
            dir.join(format!(
                "crash-{timestamp}-{}-{suffix}.txt",
                std::process::id()
            ))
        };
        match std::fs::hard_link(temporary_path, &path) {
            Ok(()) => return Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Ok(metadata) = std::fs::symlink_metadata(&path) {
                    if !metadata.file_type().is_file() {
                        return None;
                    }
                }
            }
            Err(_) => return None,
        }
    }
    None
}

/// Render the recent-event section without reading any event payload.
fn recent_event_lines(events: &[RecentEvent]) -> String {
    let mut rendered = String::from("recent_events:\n");
    rendered.extend(events.iter().map(|event| {
        format!(
            "at: {} event: {} ids: {}\n",
            event_timestamp(event.at),
            event.name,
            event_id_cells(event),
        )
    }));
    rendered
}

/// Render the named ids in a stable order.
fn event_id_cells(event: &RecentEvent) -> String {
    let ids = [
        event.session.map(|id| id.to_string()),
        event.client.map(|id| id.to_string()),
        event.tab.map(|id| id.to_string()),
        event.pane.map(|id| id.to_string()),
        event.plugin.map(|id| id.to_string()),
        event.command.map(|id| id.to_string()),
        event.subscriber.map(|id| id.to_string()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if ids.is_empty() {
        return "-".to_string();
    }
    ids.join(" ")
}

/// Render an event timestamp as whole Unix seconds.
fn event_timestamp(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Keep only the newest [`MAX_CRASH_REPORTS`] crash files.
fn retain_crash_reports(dir: &Path) {
    loop {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut reports = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let timestamp = crash_report_timestamp(name)?;
                if !entry.file_type().ok()?.is_file() {
                    return None;
                }
                #[cfg(windows)]
                if ensure_windows_private(&entry.path()).is_err() {
                    let _ = std::fs::remove_file(entry.path());
                    return None;
                }
                Some((timestamp, entry.path()))
            })
            .collect::<Vec<_>>();
        reports.sort();

        if reports.len() <= MAX_CRASH_REPORTS {
            return;
        }
        let (_, path) = reports.remove(0);
        if let Err(error) = std::fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return;
            }
        }
    }
}

/// Read the timestamp prefix from a normal or collision-suffixed report name.
fn crash_report_timestamp(name: &str) -> Option<u64> {
    let stem = name.strip_prefix("crash-")?.strip_suffix(".txt")?;
    let mut pieces = stem.split('-');
    let timestamp = pieces.next()?.parse().ok()?;
    match (pieces.next(), pieces.next(), pieces.next()) {
        (None, None, None) => Some(timestamp),
        (Some(process_id), Some(suffix), None)
            if process_id.parse::<u32>().is_ok() && suffix.parse::<u64>().is_ok() =>
        {
            Some(timestamp)
        }
        _ => None,
    }
}

#[cfg(windows)]
/// Set and verify a protected DACL with full access for the owner and the owner's child entries.
fn ensure_windows_private(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{
        BuildTrusteeWithSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
        EXPLICIT_ACCESS_W, SET_ACCESS, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let mut object_name = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if object_name.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "crash report path contains a NUL",
        ));
    }
    object_name.push(0);
    let object_name = object_name.as_ptr();

    let mut owner: PSID = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: `object_name` is NUL-terminated and all output pointers are valid.
    let status = unsafe {
        GetNamedSecurityInfoW(
            object_name,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    if owner.is_null() {
        // SAFETY: The descriptor was allocated by `GetNamedSecurityInfoW`.
        unsafe {
            LocalFree(descriptor);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the crash report has no owner",
        ));
    }

    let mut trustee = windows_sys::Win32::Security::Authorization::TRUSTEE_W::default();
    // SAFETY: `trustee` is writable and `owner` remains valid until the
    // descriptor is freed below.
    unsafe {
        BuildTrusteeWithSidW(&mut trustee, owner);
    }
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        Trustee: trustee,
    };
    let mut dacl: *mut ACL = null_mut();
    // SAFETY: `access` is valid, and the API writes the new ACL pointer.
    let status = unsafe { SetEntriesInAclW(1, &access, null_mut(), &mut dacl) };
    if status != ERROR_SUCCESS {
        // SAFETY: The descriptor was allocated by `GetNamedSecurityInfoW`.
        unsafe {
            LocalFree(descriptor);
        }
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }

    // SAFETY: `dacl` is the ACL returned by `SetEntriesInAclW`; the owner
    // pointer and descriptor remain valid for this call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            object_name,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    // SAFETY: Both allocations came from the Windows security APIs.
    unsafe {
        LocalFree(dacl.cast());
        LocalFree(descriptor);
    }
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }

    verify_windows_private(object_name)
}

#[cfg(windows)]
/// Verify that `path` has one allow ACE for its owner and no other ACE.
fn verify_windows_private(object_name: *const u16) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::{addr_of_mut, null_mut};

    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        AclSizeInformation, EqualSid, GetAce, GetAclInformation, ACL, ACL_SIZE_INFORMATION,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSID,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: `object_name` is NUL-terminated and all output pointers are valid.
    let status = unsafe {
        GetNamedSecurityInfoW(
            object_name,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }

    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` came from `GetNamedSecurityInfoW`; `size` is writable.
    let valid = unsafe {
        !owner.is_null()
            && !dacl.is_null()
            && GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            ) != 0
            && size.AceCount == 1
    };
    let mut ace = null_mut();
    let valid = valid
        // SAFETY: The ACL has one entry when this call is reached.
        && unsafe { GetAce(dacl, 0, &mut ace) != 0 && !ace.is_null() };
    let valid = valid
        // SAFETY: `ace` points into the ACL returned by Windows and remains
        // valid until `descriptor` is freed below.
        && unsafe {
            let access = ace.cast::<windows_sys::Win32::Security::ACCESS_ALLOWED_ACE>();
            let ace_sid: PSID = addr_of_mut!((*access).SidStart).cast();
            (*access).Header.AceType == ACCESS_ALLOWED_ACE_TYPE as u8
                && (*access).Mask == FILE_ALL_ACCESS
                && EqualSid(owner, ace_sid) != 0
        };
    // SAFETY: The descriptor was allocated by `GetNamedSecurityInfoW`.
    unsafe {
        LocalFree(descriptor);
    }
    if valid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the crash report ACL is not owner-only",
        ))
    }
}

/// Runs its registered [cleanup hooks](CleanupHook) exactly once — on drop, or on
/// panic if [`install_panic_hook`] was called with this guard. Hooks run in the
/// order they were registered.
///
/// The guard owns the registry; [`install_panic_hook`] shares it with the process
/// panic hook. Whichever path fires first drains and runs the hooks, so the other
/// finds an empty registry and does nothing — a hook never runs twice.
pub struct TerminalCleanupGuard {
    hooks: Registry,
}

impl TerminalCleanupGuard {
    /// Create a guard with no hooks registered yet.
    pub fn new() -> Self {
        Self {
            hooks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a hook to run at cleanup. Hooks run in registration order.
    pub fn register_cleanup(&self, hook: CleanupHook) {
        self.hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(hook);
    }
}

impl Default for TerminalCleanupGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        run_hooks(&self.hooks);
    }
}

/// Restores the panic hook that was installed before [`install_panic_hook`], on
/// drop. Holding it for the terminal session's lifetime keeps the chained hook
/// active; dropping it unchains cleanup so a later session does not stack inert
/// wrappers on the process-global hook.
///
/// The process panic hook is a single global slot. Keep one guard active at a
/// time. Drop the guards in reverse install order (LIFO) — the natural lifetime
/// of a nested scope. `Drop` restores the captured hook whenever the dropping
/// thread is not itself panicking.
#[must_use = "dropping the returned guard immediately restores the previous panic hook"]
pub struct PanicHookGuard {
    previous: Option<SharedPanicHook>,
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        // `set_hook` itself panics when called from a panicking thread, which
        // would turn the in-flight panic into a destructor abort. A drop while
        // unwinding returns here instead, leaving the chained hook installed.
        //
        // If that unwind is later caught and the process runs on, the chained
        // hook stays installed until a later, non-panicking drop restores the
        // previous one. Until then it runs no cleanup hook — the registry is
        // already drained — and still writes a crash report for each further
        // panic when a crash directory was named.
        if std::thread::panicking() {
            return;
        }
        if let Some(previous) = self.previous.take() {
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

/// Chain a panic hook that runs `guard`'s cleanup hooks and writes a crash
/// report before the previously installed hook. Terminal restoration happens
/// first, so by the time the panic message prints, the terminal is already
/// back on its normal screen with raw mode disabled.
///
/// The crash report goes to `crash_dir` under a unique `crash-<timestamp>.txt`
/// name, after the cleanup hooks. `None` reads no panic and writes no file.
///
/// The panic hook shares the guard's registry, so a panic and a later drop draw
/// from the same set: whichever runs first drains it, and the other is a no-op.
///
/// Returns a [`PanicHookGuard`] that restores the previous hook when dropped.
pub fn install_panic_hook(
    guard: &TerminalCleanupGuard,
    crash_dir: Option<PathBuf>,
) -> PanicHookGuard {
    let hooks = Arc::clone(&guard.hooks);
    let previous: SharedPanicHook = Arc::from(panic::take_hook());
    let chained = Arc::clone(&previous);
    panic::set_hook(Box::new(move |info| {
        let report = crash_dir
            .as_ref()
            .map(|dir| (dir.clone(), CrashReport::capture(info)));
        restore_then_report(&hooks, report);
        chained(info);
    }));
    PanicHookGuard {
        previous: Some(previous),
    }
}

/// Run the cleanup hooks, then write the crash report, both on one fresh
/// thread ([`on_fresh_thread`]). The thread starts every time, with or
/// without a registered hook. The report is written after the hooks, so the
/// terminal is back on its normal screen first, and inside
/// [`catch_unwind`](panic::catch_unwind), the same as a hook.
///
/// `report` pairs the directory that takes the file with the report itself.
/// It is absent when no crash directory was named.
///
/// A hook that panics on the spawned thread re-enters this function through
/// the chained hook. It finds the registry empty, runs no hook, and writes its
/// own report. A collision suffix keeps reports from replacing each other.
fn restore_then_report(hooks: &Registry, report: Option<(PathBuf, CrashReport)>) {
    let drained = drain_hooks(hooks);
    on_fresh_thread(move || {
        run_each(drained);
        if let Some((dir, report)) = report {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| report.write(&dir)));
        }
    });
}

/// Run `work` on a fresh thread and wait for it to finish. A panic raised
/// while Rust is executing a panic hook aborts the process before any
/// `catch_unwind` landing pad, so hook work stays off the panicking thread.
///
/// Spawning may fail under resource exhaustion mid-panic; then `work` is
/// dropped unrun, and the terminal may be left dirty.
fn on_fresh_thread(work: impl FnOnce() + Send + 'static) {
    if let Ok(handle) = std::thread::Builder::new().spawn(work) {
        let _ = handle.join();
    }
}

/// Take every registered hook out of the registry, leaving it empty. The lock
/// is held only for the swap, so a hook may register another hook while it
/// runs and a slow hook never holds the registry. A poisoned lock is
/// recovered: cleanup must still run when another thread died.
fn drain_hooks(hooks: &Registry) -> Vec<CleanupHook> {
    let mut guard = hooks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *guard)
}

/// Run the cleanup hooks for a dropped guard: drain the registry and run every
/// hook in registration order. Each hook runs inside
/// [`catch_unwind`](panic::catch_unwind), so a hook that panics does not stop
/// the hooks after it.
///
/// A drop that happens while the thread is already unwinding runs the hooks on
/// a fresh thread ([`on_fresh_thread`]).
///
/// The installed panic hook uses [`restore_then_report`].
fn run_hooks(hooks: &Registry) {
    let drained = drain_hooks(hooks);
    if drained.is_empty() {
        return;
    }

    if std::thread::panicking() {
        on_fresh_thread(move || run_each(drained));
    } else {
        run_each(drained);
    }
}

/// Run each hook in order, isolating a panicking hook so the rest still run.
fn run_each(hooks: Vec<CleanupHook>) {
    for hook in hooks {
        let _ = panic::catch_unwind(AssertUnwindSafe(hook));
    }
}

#[cfg(test)]
mod tests;
