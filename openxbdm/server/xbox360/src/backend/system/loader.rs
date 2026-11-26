//! Title launches through XAM's loader, for patched retail consoles whose
//! system software registers no title launch routine.
//!
//! The loader takes drive paths only: given an object path such as
//! `\Device\Harddisk0\Partition1\Games\x.xex` it goes to the dashboard
//! instead. The image's directory is linked to [`LINK_DRIVE`] in both the
//! system and title namespaces, and the loader is given
//! `openxbdm-launch:\<image name>`, so the launched title's `game:` is the
//! image's own directory.
//!
//! A title calling the loader never returns from it, so the call runs on a
//! thread of its own and the command replies at once.

use core::cell::UnsafeCell;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;

use openxbdm_server::commands::system::LaunchTitle;
use openxbdm_server::commands::system::LoaderLaunch;
use xbox360::module::Ordinal;
use xbox360::module::SystemModule;
use xbox360::module::resolve_export;
use xbox360_sys::xboxkrnl;
use xbox360_sys::xboxkrnl::Handle;
use xbox360_sys::xboxkrnl::ObCreateSymbolicLink;
use xbox360_sys::xboxkrnl::ThreadCreationFlags;

use crate::kernel;

/// `XamLoaderLaunchTitle(path, flags)`.
const XAM_LOADER_LAUNCH_TITLE: Ordinal = Ordinal::new(0x1a4).unwrap();
/// `ObDeleteSymbolicLink(link)`.
const OB_DELETE_SYMBOLIC_LINK: Ordinal = Ordinal::new(0x104).unwrap();

const LINK_DRIVE: &[u8] = b"openxbdm-launch:";
const LINK_NAMES: [&[u8]; 2] = [b"\\system??\\openxbdm-launch:", b"\\??\\openxbdm-launch:"];

/// Longest launch path: the drive, a separator, an image name and the NUL.
const PATH_CAPACITY: usize = 0x100;
const THREAD_STACK_SIZE: u32 = 0x4000;
const THREAD_FLAGS: ThreadCreationFlags = kernel::SYSTEM_THREAD_FLAGS
    .union(ThreadCreationFlags::NOT_DEBUGGABLE)
    .union(ThreadCreationFlags::EXEMPT_FROM_STOPS);

/// Set from writing [`PATH`] until the loader thread has copied it.
static BUSY: AtomicBool = AtomicBool::new(false);

/// The NUL-terminated drive path the loader thread passes on; written only
/// by the claimant of [`BUSY`] before the thread starts.
struct LaunchPath(UnsafeCell<[u8; PATH_CAPACITY]>);

// SAFETY: access is serialized by `BUSY`.
unsafe impl Sync for LaunchPath {}

static PATH: LaunchPath = LaunchPath(UnsafeCell::new([0; PATH_CAPACITY]));

pub(super) fn launch(title: &LaunchTitle) -> LoaderLaunch {
    let image = title.image.as_bytes();
    let Some(split) = image.iter().rposition(|&byte| byte == b'\\') else {
        return LoaderLaunch::BadPath;
    };
    let (directory, name) = (&image[..split], &image[split..]);
    if directory.is_empty() || name.len() < 2 || LINK_DRIVE.len() + name.len() >= PATH_CAPACITY {
        return LoaderLaunch::BadPath;
    }

    let Some(launch) = export(SystemModule::Xam, XAM_LOADER_LAUNCH_TITLE) else {
        return LoaderLaunch::Unavailable;
    };
    let delete = export(SystemModule::Kernel, OB_DELETE_SYMBOLIC_LINK);

    if BUSY.swap(true, Ordering::AcqRel) {
        return LoaderLaunch::Busy;
    }

    let target = kernel::ansi_string(directory);
    for link in LINK_NAMES {
        let link = kernel::ansi_string(link);
        if let Some(delete) = delete {
            // SAFETY: the export takes one counted name, live for the call.
            // A missing link is the usual case.
            unsafe { kernel::call_export(delete, &link as *const _ as u32, 0, 0, 0) };
        }
        // SAFETY: both counted names refer to live bytes for the call. The
        // title namespace may be absent on the dashboard; the system link
        // is enough there.
        let _ = unsafe { ObCreateSymbolicLink(&link, &target) };
    }

    // SAFETY: `BUSY` was claimed above, so no loader thread reads `PATH`.
    let path = unsafe { &mut *PATH.0.get() };
    path[..LINK_DRIVE.len()].copy_from_slice(LINK_DRIVE);
    path[LINK_DRIVE.len()..LINK_DRIVE.len() + name.len()].copy_from_slice(name);
    path[LINK_DRIVE.len() + name.len()] = 0;

    LAUNCH_ROUTINE.store(launch, Ordering::Release);
    let mut handle = Handle::NULL.raw();
    let entry = loader_thread as *const () as usize as u32;
    // SAFETY: `handle` is a live local and `entry` has the start signature.
    let status = unsafe {
        xboxkrnl::ExCreateThread(
            &mut handle,
            THREAD_STACK_SIZE,
            core::ptr::null_mut(),
            0,
            entry,
            0,
            THREAD_FLAGS,
        )
    };
    if status.result().is_err() {
        BUSY.store(false, Ordering::Release);
        kernel::debug_print(c"openxbdm: loader thread creation failed\n");
        return LoaderLaunch::Unavailable;
    }
    // SAFETY: the handle came from `ExCreateThread`; the thread keeps
    // running without it.
    unsafe { xboxkrnl::NtClose(Handle::from_raw(handle)) };

    LoaderLaunch::Started
}

/// XamLoaderLaunchTitle, for the loader thread.
static LAUNCH_ROUTINE: AtomicU32 = AtomicU32::new(0);

extern "C" fn loader_thread(_context: u32) -> u32 {
    kernel::debug_print(c"openxbdm: launching through the XAM loader\n");
    let routine = LAUNCH_ROUTINE.load(Ordering::Acquire);
    // SAFETY: `BUSY` is held until the copy is made.
    let path = unsafe { *PATH.0.get() };
    BUSY.store(false, Ordering::Release);
    // SAFETY: the routine is XamLoaderLaunchTitle, which takes a
    // NUL-terminated path and a flags dword.
    unsafe { kernel::call_export(routine, path.as_ptr() as u32, 0, 0, 0) };
    // SAFETY: the thread owns no further state.
    unsafe { xboxkrnl::ExTerminateThread(0) }
}

fn export(module: SystemModule, ordinal: Ordinal) -> Option<u32> {
    let module = module.handle().ok()?;
    resolve_export(module, ordinal)
        .ok()
        .map(|address| address.raw())
}
