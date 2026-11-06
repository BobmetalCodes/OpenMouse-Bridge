//! Keeps Bridge from loading a GPU driver for every kind of GPU the system has
//! packages for. The window asks the graphics stack for all its adapters, which
//! loads each Vulkan driver installed plus the OpenGL ones, and they stay loaded:
//! on one AMD laptop with an NVIDIA GPU this took the open panel from 113 MB to
//! 204 MB. Drawing a small panel needs only the driver of the primary GPU.
//!
//! The limit is set through the environment, which only reaches this process's
//! graphics stack if it is set before anything reads it, so [`limit`] runs first
//! thing in `main`. [`release`] takes it away once the graphics stack is up, so
//! programs Bridge starts, such as the browser, do not inherit it.
//!
//! Set `OPENMOUSE_BRIDGE_ALL_GPU_DRIVERS` to skip this.

use std::{
    env, fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

/// Which Vulkan driver the loader may use.
const VULKAN_DRIVERS: &str = "VK_DRIVER_FILES";
/// Which OpenGL vendors may load. Pointing it at nothing turns the OpenGL
/// fallback off, so that it does not load the drivers of every GPU either.
const EGL_VENDORS: &str = "__EGL_VENDOR_LIBRARY_FILENAMES";
const NO_EGL_VENDORS: &str = "/nonexistent/openmouse-bridge-no-egl.json";
/// Set by the user: anything here means the choice is already made.
const LEFT_ALONE_FOR: [&str; 4] = [
    VULKAN_DRIVERS,
    "VK_ICD_FILENAMES",
    EGL_VENDORS,
    "OPENMOUSE_BRIDGE_ALL_GPU_DRIVERS",
];
const DRIVER_DIRECTORIES: [&str; 2] = ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"];

static LIMITED: AtomicBool = AtomicBool::new(false);

/// Limits the graphics stack to the primary GPU's Vulkan driver, when that driver
/// can be found. Anything unexpected leaves everything as it was.
///
/// Call before any thread starts: changing the environment is only safe then.
pub fn limit() {
    if LEFT_ALONE_FOR
        .iter()
        .any(|name| env::var_os(name).is_some())
    {
        return;
    }
    let Some(stem) = primary_vendor().and_then(|vendor| driver_stem(&vendor)) else {
        return;
    };
    let Some(driver) = DRIVER_DIRECTORIES
        .iter()
        .find_map(|directory| find_driver(Path::new(directory), stem))
    else {
        return;
    };
    // SAFETY: `main` calls this before it starts any threads.
    unsafe {
        env::set_var(VULKAN_DRIVERS, driver);
        env::set_var(EGL_VENDORS, NO_EGL_VENDORS);
    }
    LIMITED.store(true, Ordering::Relaxed);
}

/// Takes the limit away again, once a window has brought the graphics stack up.
/// Later calls do nothing, so every window can make this call.
pub fn release() {
    if LIMITED.swap(false, Ordering::Relaxed) {
        // SAFETY: threads are running by now, so this relies on none of them
        // reading these two variables: only the graphics stack does, on this
        // thread, while the first window opens, and that has finished.
        unsafe {
            env::remove_var(VULKAN_DRIVERS);
            env::remove_var(EGL_VENDORS);
        }
    }
}

/// The PCI vendor of the GPU the firmware booted on, such as `0x1002`.
fn primary_vendor() -> Option<String> {
    fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .find_map(|card| {
            let device = card.path().join("device");
            let boot = fs::read_to_string(device.join("boot_vga")).ok()?;
            (boot.trim() == "1")
                .then(|| fs::read_to_string(device.join("vendor")).ok())
                .flatten()
                .map(|vendor| vendor.trim().to_owned())
        })
}

/// How a vendor's Vulkan driver manifest starts, like `radeon_icd.json`.
fn driver_stem(vendor: &str) -> Option<&'static str> {
    match vendor {
        "0x1002" => Some("radeon_icd"),
        "0x8086" => Some("intel_icd"),
        "0x10de" => Some("nvidia_icd"),
        _ => None,
    }
}

fn find_driver(directory: &Path, stem: &str) -> Option<std::path::PathBuf> {
    let mut names = fs::read_dir(directory)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    // Directory order is arbitrary; keep the choice stable.
    names.sort();
    names
        .into_iter()
        .find(|name| matches_driver(name, stem))
        .map(|name| directory.join(name))
}

/// A manifest for this architecture's driver: `radeon_icd.json` or
/// `radeon_icd.x86_64.json`, but not `radeon_icd.i686.json`.
fn matches_driver(name: &str, stem: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(stem)
        .and_then(|rest| rest.strip_suffix(".json"))
    else {
        return false;
    };
    rest.is_empty() || rest == format!(".{}", env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::{driver_stem, matches_driver};

    #[test]
    fn known_gpu_vendors_have_a_driver_and_others_do_not() {
        assert_eq!(driver_stem("0x1002"), Some("radeon_icd"));
        assert_eq!(driver_stem("0x8086"), Some("intel_icd"));
        assert_eq!(driver_stem("0x10de"), Some("nvidia_icd"));
        assert_eq!(driver_stem("0x1af4"), None);
    }

    #[test]
    fn a_driver_manifest_matches_only_for_this_architecture() {
        assert!(matches_driver("radeon_icd.json", "radeon_icd"));
        let native = format!("radeon_icd.{}.json", std::env::consts::ARCH);
        assert!(matches_driver(&native, "radeon_icd"));
        assert!(!matches_driver("radeon_icd.other-arch.json", "radeon_icd"));
        assert!(!matches_driver("intel_icd.json", "radeon_icd"));
        assert!(!matches_driver("intel_hasvk_icd.json", "intel_icd"));
    }
}
