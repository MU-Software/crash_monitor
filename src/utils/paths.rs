//! Shared path utilities for crash reporter data directories.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{FchmodatFlags, Mode, fchmodat, fstatat, mkdirat};
use nix::unistd::{UnlinkatFlags, unlinkat};

/// Exact mode for every crash-monitor data and report directory.
pub(crate) const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

/// Exact mode for report artifacts and same-directory temporary files.
pub(crate) const PRIVATE_FILE_MODE: u32 = 0o600;

/// Environment variable that a host project sets to choose the base data
/// directory (where crash reports, sessions, and locks live).
///
/// This is the project-configuration point: a project embedding the crash
/// monitor points it at its own namespace (e.g. `~/.myapp`) by exporting
/// `CRASH_MONITOR_DATA_DIR` before launching the monitor — the same value is
/// inherited by the monitored child, so the C reporter and the Rust monitor
/// agree on one location. It is also set by `tools/crash_monitor/.cargo/config.toml`
/// during `cargo test`/`cargo run` to a sandbox under `target/` so tests never
/// touch the real data directory.
///
/// When unset, both sides fall back to the tool default `~/.crash_monitor/`.
const DATA_DIR_OVERRIDE_ENV: &str = "CRASH_MONITOR_DATA_DIR";

/// Base directory name under `$HOME` when the override env is unset.
///
/// A host project bakes its own namespace here at build time by setting the
/// `CRASH_MONITOR_DATA_DIR_NAME` env when compiling (see `build.rs`, which marks
/// it as a rebuild trigger). When unbaked — the generic standalone tool build —
/// this is `.crash_monitor`. Only the dir *name* is baked; it resolves against
/// `$HOME` at runtime, so the binary carries no build-machine path and stays
/// safe to distribute.
const DEFAULT_DATA_DIR_NAME: &str = match option_env!("CRASH_MONITOR_DATA_DIR_NAME") {
    Some(name) => name,
    None => ".crash_monitor",
};

/// Base directory for crash reporter data: `$CRASH_MONITOR_DATA_DIR` if set,
/// else `~/.crash_monitor/`.
pub fn data_dir_path() -> Result<PathBuf, String> {
    let dir = if let Ok(override_path) = std::env::var(DATA_DIR_OVERRIDE_ENV) {
        if override_path.is_empty() {
            return Err(format!("{DATA_DIR_OVERRIDE_ENV} is set but empty"));
        }
        PathBuf::from(override_path)
    } else {
        let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
        PathBuf::from(home).join(DEFAULT_DATA_DIR_NAME)
    };
    Ok(dir)
}

pub fn data_dir() -> Result<PathBuf, String> {
    let dir = data_dir_path()?;
    ensure_private_directory(&dir)?;
    Ok(dir)
}

/// Resolve the pending report root without touching the filesystem. Capture
/// paths use this pure helper so directory I/O cannot extend task suspension
/// or the Mach exception reply deadline.
pub fn pending_dir_path() -> Result<PathBuf, String> {
    Ok(data_dir_path()?.join("crashes").join("pending"))
}

/// Working directory for in-flight reports: `<data_dir>/crashes/pending/`.
/// The pipeline writes Stage 1 raw dumps, Stage 2 JSON, and intermediate
/// files here. The `MoveToSent` post-processor relocates finished reports
/// to `sent_dir()`.
pub fn pending_dir() -> Result<PathBuf, String> {
    let data = data_dir()?;
    let crashes = data.join("crashes");
    ensure_private_directory(&crashes)?;
    let dir = crashes.join("pending");
    ensure_private_directory(&dir)?;
    Ok(dir)
}

/// Archive directory for completed reports: `<data_dir>/crashes/sent/`.
/// `MoveToSent` populates it after the post-processor chain finishes, and
/// `RetentionManager` prunes it by count/size/age.
pub fn sent_dir() -> Result<PathBuf, String> {
    let data = data_dir()?;
    let crashes = data.join("crashes");
    ensure_private_directory(&crashes)?;
    let dir = crashes.join("sent");
    ensure_private_directory(&dir)?;
    Ok(dir)
}

/// Create or validate one private directory.
///
/// Missing path components are created one at a time with `0700` and then
/// corrected with `fchmod`, so both permissive and restrictive umasks produce
/// the same final mode. Existing final directories must be owned by the
/// effective user, must not be symlinks, and must not carry an extended ACL.
/// An owned directory with a different POSIX mode is safely corrected.
/// Existing ancestors are opened relative to their parent with `O_NOFOLLOW`,
/// checked for safe ownership/write access, and never chmod'ed. The only path
/// aliases accepted are Darwin's exact root-owned `/var` and `/tmp` aliases,
/// which are rewritten to `/private/var` and `/private/tmp` before traversal.
/// The requested final directory, plus every component newly created below
/// the first missing ancestor, is managed as private storage.
pub(crate) fn ensure_private_directory(path: &Path) -> Result<(), String> {
    let (mut parent, components, mut resolved) = path_walk(path)?;
    let mut creating_private_tree = false;
    for (index, component) in components.iter().enumerate() {
        resolved.push(component);
        let is_final = index + 1 == components.len();
        let child = match open_directory_at_with_retry(&parent, component, &resolved) {
            Ok(child) => {
                if creating_private_tree || is_final {
                    validate_private_handle(&child, &resolved, PrivateNodeKind::Directory, true)?;
                } else {
                    validate_trusted_ancestor(&child, &resolved)?;
                }
                child
            }
            Err(nix::errno::Errno::EACCES) if creating_private_tree || is_final => {
                prepare_directory_entry(&parent, component, &resolved)?;
                let child = open_directory_at(&parent, component, &resolved).map_err(|error| {
                    format!(
                        "cannot open corrected private directory '{}': {error}",
                        resolved.display()
                    )
                })?;
                validate_private_handle(&child, &resolved, PrivateNodeKind::Directory, true)?;
                child
            }
            Err(nix::errno::Errno::ENOENT) => {
                creating_private_tree = true;
                create_or_join_private_directory(&parent, component, &resolved)?
            }
            Err(error) => {
                return Err(format!(
                    "cannot safely open private directory '{}': {error}",
                    resolved.display()
                ));
            }
        };
        parent = child;
    }

    Ok(())
}

/// Exclusively create one private directory below an already validated
/// parent. Unlike [`ensure_private_directory`], an existing path is an error.
pub(crate) fn create_private_directory(path: &Path) -> Result<File, String> {
    let (parent, component, resolved) = secure_parent(path)?;
    mkdirat(&parent, component.as_os_str(), Mode::S_IRWXU).map_err(|error| {
        format!(
            "cannot exclusively create private directory '{}': {error}",
            path.display()
        )
    })?;
    if let Err(error) = prepare_directory_entry(&parent, &component, &resolved) {
        let _ = unlinkat(&parent, component.as_os_str(), UnlinkatFlags::RemoveDir);
        return Err(error);
    }
    let directory = match open_directory_at(&parent, &component, &resolved) {
        Ok(directory) => directory,
        Err(error) => {
            let _ = unlinkat(&parent, component.as_os_str(), UnlinkatFlags::RemoveDir);
            return Err(format!(
                "cannot open new private directory '{}': {error}",
                path.display()
            ));
        }
    };
    if let Err(error) = validate_private_handle(&directory, path, PrivateNodeKind::Directory, true)
    {
        drop(directory);
        let _ = unlinkat(&parent, component.as_os_str(), UnlinkatFlags::RemoveDir);
        return Err(error);
    }
    directory.sync_all().map_err(|error| {
        format!(
            "cannot sync private directory '{}': {error}",
            path.display()
        )
    })?;
    parent.sync_all().map_err(|error| {
        format!(
            "cannot sync private directory parent for '{}': {error}",
            path.display()
        )
    })?;
    Ok(directory)
}

/// Exclusively create a private regular file without following a final
/// symlink. `create_new` supplies `O_CREAT|O_EXCL`; `O_NOFOLLOW` is specified
/// explicitly as defense in depth. The descriptor is `fchmod`'ed to `0600`
/// and revalidated before it is returned.
pub(crate) fn create_private_file(path: &Path) -> Result<File, String> {
    let (parent, component, _) = secure_parent(path)?;
    let descriptor = openat(
        &parent,
        component.as_os_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .map_err(|error| format!("cannot create private file '{}': {error}", path.display()))?;
    let file = File::from(descriptor);

    if let Err(error) = validate_private_handle(&file, path, PrivateNodeKind::File, true) {
        drop(file);
        let _ = unlinkat(&parent, component.as_os_str(), UnlinkatFlags::NoRemoveDir);
        return Err(error);
    }
    Ok(file)
}

/// Open an existing private regular file relative to securely opened parent
/// directory descriptors. Every ancestor and the final component use
/// `O_NOFOLLOW`.
pub(crate) fn open_private_file(path: &Path) -> Result<File, String> {
    let (parent, component, _) = secure_parent(path)?;
    let descriptor = openat(
        &parent,
        component.as_os_str(),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        format!(
            "cannot safely open private file '{}': {error}",
            path.display()
        )
    })?;
    let file = File::from(descriptor);
    validate_private_handle(&file, path, PrivateNodeKind::File, true)?;
    Ok(file)
}

/// Atomically publish a new private file or directory without replacing an
/// existing destination.
///
/// On macOS this uses `renameatx_np(RENAME_EXCL)` with both parents opened
/// through the same fd-relative, no-follow traversal used for creation. A
/// plain preflight existence check is deliberately insufficient because it
/// leaves a clobber race before `rename`.
pub(crate) fn publish_private_path(source: &Path, destination: &Path) -> Result<(), String> {
    let (source_parent, source_name, source_resolved) = secure_parent(source)?;
    let (destination_parent, destination_name, destination_resolved) = secure_parent(destination)?;
    let source_parent_path = source_resolved
        .parent()
        .ok_or_else(|| format!("private source has no parent: '{}'", source.display()))?;
    let destination_parent_path = destination_resolved.parent().ok_or_else(|| {
        format!(
            "private destination has no parent: '{}'",
            destination.display()
        )
    })?;
    validate_private_handle(
        &source_parent,
        source_parent_path,
        PrivateNodeKind::Directory,
        true,
    )?;
    validate_private_handle(
        &destination_parent,
        destination_parent_path,
        PrivateNodeKind::Directory,
        true,
    )?;

    let _source_handle =
        open_validated_publish_source(&source_parent, &source_name, &source_resolved)?;

    publish_private_path_at(
        &source_parent,
        &source_name,
        &destination_parent,
        &destination_name,
    )
    .map_err(|error| {
        format!(
            "cannot exclusively publish private path '{}' as '{}': {error}",
            source.display(),
            destination.display()
        )
    })
}

fn open_validated_publish_source(
    parent: &File,
    name: &OsString,
    path: &Path,
) -> Result<File, String> {
    let initial = fstatat(parent, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
        .map_err(|error| format!("cannot inspect private publish source: {error}"))?;
    let file_type = initial.st_mode & nix::libc::S_IFMT;
    let (handle, kind) = if file_type == nix::libc::S_IFDIR {
        let directory = open_directory_at(parent, name, path)
            .map_err(|error| format!("cannot safely open private publish source: {error}"))?;
        (directory, PrivateNodeKind::Directory)
    } else if file_type == nix::libc::S_IFREG {
        let descriptor = openat(
            parent,
            name.as_os_str(),
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| format!("cannot safely open private publish source: {error}"))?;
        (File::from(descriptor), PrivateNodeKind::File)
    } else {
        return Err(format!(
            "private publish source is not a regular file or directory: '{}'",
            path.display()
        ));
    };
    validate_private_handle(&handle, path, kind, false)?;

    let named = fstatat(parent, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
        .map_err(|error| format!("cannot re-inspect private publish source: {error}"))?;
    let opened = handle
        .metadata()
        .map_err(|error| format!("cannot inspect opened private publish source: {error}"))?;
    let named_device = u64::try_from(named.st_dev)
        .map_err(|_| "private publish source device is negative".to_string())?;
    if opened.dev() != named_device || opened.ino() != named.st_ino {
        return Err(format!(
            "private publish source changed during validation: '{}'",
            path.display()
        ));
    }
    Ok(handle)
}

#[cfg(target_os = "macos")]
fn publish_private_path_at(
    source_parent: &File,
    source_name: &OsString,
    destination_parent: &File,
    destination_name: &OsString,
) -> Result<(), std::io::Error> {
    const RENAME_EXCL: u32 = 0x0000_0004;

    unsafe extern "C" {
        fn renameatx_np(
            from_fd: nix::libc::c_int,
            from: *const nix::libc::c_char,
            to_fd: nix::libc::c_int,
            to: *const nix::libc::c_char,
            flags: u32,
        ) -> nix::libc::c_int;
    }

    let source = std::ffi::CString::new(source_name.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let destination = std::ffi::CString::new(destination_name.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both descriptors are live directory handles, both C strings are
    // NUL-terminated and borrow for the duration of the call, and the flag is
    // Darwin's documented `RENAME_EXCL` value.
    let status = unsafe {
        renameatx_np(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
            RENAME_EXCL,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn publish_private_path_at(
    _source_parent: &File,
    _source_name: &OsString,
    _destination_parent: &File,
    _destination_name: &OsString,
) -> Result<(), std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive private publication is only implemented for macOS",
    ))
}

/// Validate and, when safe, correct a private regular file that is already
/// open through a no-follow descriptor.
pub(crate) fn validate_private_file(file: &File, path: &Path) -> Result<(), String> {
    validate_private_handle(file, path, PrivateNodeKind::File, true)
}

/// Open and validate a private directory without following its final path
/// component. Owned mode drift is corrected to `0700`.
pub(crate) fn open_private_directory(path: &Path) -> Result<File, String> {
    let (mut parent, mut components, mut resolved) = path_walk(path)?;
    let final_component = components
        .pop()
        .ok_or_else(|| format!("private directory has no name: '{}'", path.display()))?;
    for component in components {
        resolved.push(&component);
        parent = open_directory_at_with_retry(&parent, &component, &resolved).map_err(|error| {
            format!(
                "cannot safely open private path ancestor '{}': {error}",
                resolved.display()
            )
        })?;
        validate_trusted_ancestor(&parent, &resolved)?;
    }
    resolved.push(&final_component);
    let directory = match open_directory_at_with_retry(&parent, &final_component, &resolved) {
        Ok(directory) => directory,
        Err(nix::errno::Errno::EACCES) => {
            prepare_directory_entry(&parent, &final_component, &resolved)?;
            open_directory_at(&parent, &final_component, &resolved).map_err(|error| {
                format!(
                    "cannot open corrected private directory '{}': {error}",
                    resolved.display()
                )
            })?
        }
        Err(error) => {
            return Err(format!(
                "cannot safely open private directory '{}': {error}",
                resolved.display()
            ));
        }
    };
    validate_private_handle(&directory, &resolved, PrivateNodeKind::Directory, true)?;
    Ok(directory)
}

fn path_walk(path: &Path) -> Result<(File, Vec<OsString>, PathBuf), String> {
    let mut absolute = false;
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::Normal(name) => components.push(name.to_os_string()),
            Component::ParentDir => {
                return Err(format!(
                    "private path must not contain '..': '{}'",
                    path.display()
                ));
            }
            Component::Prefix(_) => {
                return Err(format!(
                    "unsupported private path prefix: '{}'",
                    path.display()
                ));
            }
        }
    }
    if components.is_empty() {
        return Err(format!(
            "private path must name a child directory: '{}'",
            path.display()
        ));
    }
    if absolute {
        rewrite_darwin_root_alias(&mut components)?;
        let root = PathBuf::from("/");
        let handle = open_directory_no_follow(&root)?;
        validate_trusted_ancestor(&handle, &root)?;
        Ok((handle, components, root))
    } else {
        let current = std::env::current_dir()
            .map_err(|error| format!("cannot resolve current directory: {error}"))?;
        let handle = open_directory_no_follow(&current)?;
        validate_trusted_ancestor(&handle, &current)?;
        Ok((handle, components, current))
    }
}

fn secure_parent(path: &Path) -> Result<(File, OsString, PathBuf), String> {
    let (mut parent, mut components, mut resolved) = path_walk(path)?;
    let component = components
        .pop()
        .ok_or_else(|| format!("private path has no filename: '{}'", path.display()))?;
    for ancestor in components {
        resolved.push(&ancestor);
        parent = open_directory_at_with_retry(&parent, &ancestor, &resolved).map_err(|error| {
            format!(
                "cannot safely open private path ancestor '{}': {error}",
                resolved.display()
            )
        })?;
        validate_trusted_ancestor(&parent, &resolved)?;
    }
    validate_private_handle(&parent, &resolved, PrivateNodeKind::Directory, false)?;
    resolved.push(&component);
    Ok((parent, component, resolved))
}

fn open_directory_at(
    parent: &File,
    component: &OsString,
    _display_path: &Path,
) -> Result<File, nix::errno::Errno> {
    openat(
        parent,
        component.as_os_str(),
        OFlag::O_RDONLY
            | OFlag::O_DIRECTORY
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC
            | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
}

fn open_directory_at_with_retry(
    parent: &File,
    component: &OsString,
    path: &Path,
) -> Result<File, nix::errno::Errno> {
    const PERMISSION_RACE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(10);
    let started = std::time::Instant::now();
    let deadline = started
        .checked_add(PERMISSION_RACE_DEADLINE)
        .unwrap_or(started);
    loop {
        match open_directory_at(parent, component, path) {
            Err(nix::errno::Errno::EACCES) if std::time::Instant::now() < deadline => {
                std::thread::yield_now();
            }
            result => return result,
        }
    }
}

fn trusted_ancestor_metadata(metadata: &fs::Metadata) -> bool {
    if !metadata.file_type().is_dir() {
        return false;
    }
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { nix::libc::geteuid() };
    let mode = metadata.mode() & 0o7777;
    let owner_is_trusted = metadata.uid() == effective_uid || metadata.uid() == 0;
    let owner_can_traverse = mode & 0o500 == 0o500;
    let untrusted_write = mode & 0o022 != 0;
    let root_sticky_directory = metadata.uid() == 0 && mode & 0o1000 != 0;
    owner_is_trusted && owner_can_traverse && (!untrusted_write || root_sticky_directory)
}

fn validate_trusted_ancestor(handle: &File, path: &Path) -> Result<(), String> {
    let metadata = handle.metadata().map_err(|error| {
        format!(
            "cannot inspect trusted private path ancestor '{}': {error}",
            path.display()
        )
    })?;
    if !trusted_ancestor_metadata(&metadata) {
        return Err(format!(
            "private path ancestor is not owned and protected against untrusted writes: '{}'",
            path.display()
        ));
    }
    if has_allowing_extended_acl(handle)? {
        return Err(format!(
            "private path ancestor '{}' has an extended ACL that grants access",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn rewrite_darwin_root_alias(components: &mut Vec<OsString>) -> Result<(), String> {
    let Some(first) = components.first().and_then(|component| component.to_str()) else {
        return Ok(());
    };
    if !matches!(first, "var" | "tmp") {
        return Ok(());
    }
    let alias = Path::new("/").join(first);
    let metadata = fs::symlink_metadata(&alias).map_err(|error| {
        format!(
            "cannot inspect Darwin root alias '{}': {error}",
            alias.display()
        )
    })?;
    if metadata.uid() != 0 || !metadata.file_type().is_symlink() {
        return Err(format!(
            "Darwin root alias is not a root-owned symlink: '{}'",
            alias.display()
        ));
    }
    let expected = Path::new("/private").join(first);
    let target = fs::canonicalize(&alias).map_err(|error| {
        format!(
            "cannot resolve Darwin root alias '{}': {error}",
            alias.display()
        )
    })?;
    if target != expected {
        return Err(format!(
            "Darwin root alias '{}' resolves outside '{}': '{}'",
            alias.display(),
            expected.display(),
            target.display()
        ));
    }
    components.insert(0, OsString::from("private"));
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn rewrite_darwin_root_alias(_components: &mut Vec<OsString>) -> Result<(), String> {
    Ok(())
}

fn create_or_join_private_directory(
    parent: &File,
    component: &OsString,
    path: &Path,
) -> Result<File, String> {
    match mkdirat(parent, component.as_os_str(), Mode::S_IRWXU) {
        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
        Err(error) => {
            return Err(format!(
                "cannot create private directory '{}': {error}",
                path.display()
            ));
        }
    }
    // Shared ensure callers may already hold this inode open. On any later
    // error, leave the directory in its umask-restricted/private state instead
    // of unlinking it from under a concurrent caller.
    prepare_directory_entry(parent, component, path)?;
    let child = match open_directory_at(parent, component, path) {
        Ok(child) => child,
        Err(error) => {
            return Err(format!(
                "cannot open newly private directory '{}': {error}",
                path.display()
            ));
        }
    };
    validate_private_handle(&child, path, PrivateNodeKind::Directory, true)?;
    child.sync_all().map_err(|error| {
        format!(
            "cannot sync private directory '{}': {error}",
            path.display()
        )
    })?;
    parent.sync_all().map_err(|error| {
        format!(
            "cannot sync private directory parent for '{}': {error}",
            path.display()
        )
    })?;
    Ok(child)
}

fn prepare_directory_entry(parent: &File, component: &OsString, path: &Path) -> Result<(), String> {
    let metadata =
        fstatat(parent, component.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|error| {
            format!(
                "cannot inspect private directory entry '{}': {error}",
                path.display()
            )
        })?;
    if metadata.st_mode & nix::libc::S_IFMT != nix::libc::S_IFDIR {
        return Err(format!(
            "private directory entry is not a directory: '{}'",
            path.display()
        ));
    }
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { nix::libc::geteuid() };
    if metadata.st_uid != effective_uid {
        return Err(format!(
            "private directory '{}' is owned by uid {}, expected effective uid {effective_uid}",
            path.display(),
            metadata.st_uid
        ));
    }
    fchmodat(
        parent,
        component.as_os_str(),
        Mode::S_IRWXU,
        FchmodatFlags::NoFollowSymlink,
    )
    .map_err(|error| {
        format!(
            "cannot set private directory mode for '{}': {error}",
            path.display()
        )
    })
}

fn open_directory_no_follow(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(
            nix::libc::O_DIRECTORY
                | nix::libc::O_NOFOLLOW
                | nix::libc::O_CLOEXEC
                | nix::libc::O_NONBLOCK,
        )
        .open(path)
        .map_err(|error| {
            format!(
                "cannot safely open private directory '{}': {error}",
                path.display()
            )
        })
}

#[derive(Clone, Copy)]
enum PrivateNodeKind {
    Directory,
    File,
}

impl PrivateNodeKind {
    const fn mode(self) -> u32 {
        match self {
            Self::Directory => PRIVATE_DIRECTORY_MODE,
            Self::File => PRIVATE_FILE_MODE,
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
        }
    }
}

fn validate_private_handle(
    handle: &File,
    path: &Path,
    kind: PrivateNodeKind,
    correct_mode: bool,
) -> Result<(), String> {
    let metadata = handle.metadata().map_err(|error| {
        format!(
            "cannot inspect private {} '{}': {error}",
            kind.description(),
            path.display()
        )
    })?;
    let expected_type = match kind {
        PrivateNodeKind::Directory => metadata.file_type().is_dir(),
        PrivateNodeKind::File => metadata.file_type().is_file(),
    };
    if !expected_type {
        return Err(format!(
            "private path is not a regular {}: '{}'",
            kind.description(),
            path.display()
        ));
    }

    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { nix::libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(format!(
            "private {} '{}' is owned by uid {}, expected effective uid {effective_uid}",
            kind.description(),
            path.display(),
            metadata.uid()
        ));
    }
    if has_extended_acl(handle)? {
        return Err(format!(
            "private {} '{}' has an extended ACL",
            kind.description(),
            path.display()
        ));
    }

    let expected_mode = kind.mode();
    let actual_mode = metadata.mode() & 0o7777;
    if actual_mode != expected_mode {
        if !correct_mode {
            return Err(format!(
                "private {} '{}' has mode {actual_mode:04o}, expected {expected_mode:04o}",
                kind.description(),
                path.display()
            ));
        }
        handle
            .set_permissions(fs::Permissions::from_mode(expected_mode))
            .map_err(|error| {
                format!(
                    "cannot correct private {} mode for '{}': {error}",
                    kind.description(),
                    path.display()
                )
            })?;
        let corrected = handle.metadata().map_err(|error| {
            format!(
                "cannot re-inspect private {} '{}': {error}",
                kind.description(),
                path.display()
            )
        })?;
        let corrected_mode = corrected.mode() & 0o7777;
        if corrected_mode != expected_mode {
            return Err(format!(
                "private {} '{}' remains mode {corrected_mode:04o}, expected {expected_mode:04o}",
                kind.description(),
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn has_extended_acl(handle: &File) -> Result<bool, String> {
    Ok(!matches!(
        extended_acl_access(handle)?,
        ExtendedAclAccess::None
    ))
}

#[cfg(target_os = "macos")]
fn has_allowing_extended_acl(handle: &File) -> Result<bool, String> {
    Ok(matches!(
        extended_acl_access(handle)?,
        ExtendedAclAccess::Allows
    ))
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExtendedAclAccess {
    None,
    DenyOnly,
    Allows,
}

#[cfg(target_os = "macos")]
fn extended_acl_access(handle: &File) -> Result<ExtendedAclAccess, String> {
    use std::ffi::c_void;

    type Acl = *mut c_void;
    type AclEntry = *mut c_void;

    const ACL_TYPE_EXTENDED: nix::libc::c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: nix::libc::c_int = 0;
    const ACL_NEXT_ENTRY: nix::libc::c_int = -1;
    const ACL_EXTENDED_ALLOW: nix::libc::c_int = 1;
    const ACL_EXTENDED_DENY: nix::libc::c_int = 2;

    unsafe extern "C" {
        fn acl_get_fd_np(fd: nix::libc::c_int, acl_type: nix::libc::c_int) -> Acl;
        fn acl_get_entry(
            acl: Acl,
            entry_id: nix::libc::c_int,
            entry: *mut AclEntry,
        ) -> nix::libc::c_int;
        fn acl_get_tag_type(entry: AclEntry, tag_type: *mut nix::libc::c_int) -> nix::libc::c_int;
        fn acl_free(object: *mut c_void) -> nix::libc::c_int;
    }

    // SAFETY: `handle` owns a valid descriptor and the constant is the Darwin
    // `ACL_TYPE_EXTENDED` value. The returned allocation is released below.
    let acl = unsafe { acl_get_fd_np(handle.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        // Darwin reports ENOENT when the inode has no extended ACL object.
        // This is the empty-ACL success case, not a missing filesystem path.
        if error.raw_os_error() == Some(nix::libc::ENOENT) {
            return Ok(ExtendedAclAccess::None);
        }
        return Err(format!("cannot inspect private path ACL: {error}"));
    }
    let access_result = (|| {
        let mut access = ExtendedAclAccess::None;
        let mut entry_id = ACL_FIRST_ENTRY;
        loop {
            let mut entry: AclEntry = std::ptr::null_mut();
            // SAFETY: `acl` is live and `entry` points to writable storage for
            // the borrowed entry handle.
            let status = unsafe { acl_get_entry(acl, entry_id, &raw mut entry) };
            if status == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(nix::libc::EINVAL) {
                    break;
                }
                return Err(format!("cannot enumerate private path ACL: {error}"));
            }
            if status != 0 {
                return Err(format!("unexpected acl_get_entry status: {status}"));
            }
            let mut tag_type = 0;
            // SAFETY: `entry` was returned by acl_get_entry for this live ACL;
            // `tag_type` points to initialized writable integer storage.
            if unsafe { acl_get_tag_type(entry, &raw mut tag_type) } != 0 {
                return Err(format!(
                    "cannot inspect private path ACL tag: {}",
                    std::io::Error::last_os_error()
                ));
            }
            match tag_type {
                ACL_EXTENDED_ALLOW => access = ExtendedAclAccess::Allows,
                ACL_EXTENDED_DENY if access == ExtendedAclAccess::None => {
                    access = ExtendedAclAccess::DenyOnly;
                }
                ACL_EXTENDED_DENY => {}
                other => return Err(format!("unsupported private path ACL tag: {other}")),
            }
            entry_id = ACL_NEXT_ENTRY;
        }
        Ok(access)
    })();
    // SAFETY: `acl` was allocated by the ACL API and is freed exactly once.
    let free_status = unsafe { acl_free(acl.cast()) };
    if free_status != 0 {
        return Err(format!(
            "cannot release private path ACL: {}",
            std::io::Error::last_os_error()
        ));
    }
    access_result
}

#[cfg(not(target_os = "macos"))]
fn has_extended_acl(_handle: &File) -> Result<bool, String> {
    // The product target is macOS. Other targets retain owner/type/mode and
    // no-follow enforcement, but have no portable std API for POSIX ACLs.
    Ok(false)
}

#[cfg(not(target_os = "macos"))]
fn has_allowing_extended_acl(_handle: &File) -> Result<bool, String> {
    Ok(false)
}

/// Given a pending directory path, return the sibling sent directory:
/// `<parent>/sent/`. Used by `Pipeline.output_dir` overrides so tests can
/// substitute a tempdir-rooted layout without touching `data_dir()`.
#[must_use]
pub fn sent_dir_for(pending: &std::path::Path) -> PathBuf {
    pending
        .parent()
        .map_or_else(|| pending.join("sent"), |parent| parent.join("sent"))
}

#[cfg(test)]
#[path = "../../tests/unit/utils/paths_tests.rs"]
mod tests;
