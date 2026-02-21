use std::fs;
use std::path::PathBuf;

pub trait ActiveAppProvider: Send + Sync {
    fn active_bundle_id(&self) -> Option<String>;
    fn active_window_title(&self) -> Option<String>;
}

pub struct DefaultActiveAppProvider;

impl ActiveAppProvider for DefaultActiveAppProvider {
    fn active_bundle_id(&self) -> Option<String> {
        macos_bridge::frontmost_bundle_id()
    }

    fn active_window_title(&self) -> Option<String> {
        macos_bridge::frontmost_window_title()
    }
}

pub mod macos_bridge {
    #[cfg(target_os = "macos")]
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}
    #[cfg(target_os = "macos")]
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {}

    #[cfg(target_os = "macos")]
    pub fn frontmost_bundle_id() -> Option<String> {
        use std::ffi::CStr;
        use std::os::raw::c_char;

        use objc2::rc::autoreleasepool;
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};

        autoreleasepool(|_| {
            // NSWorkspace.sharedWorkspace.frontmostApplication.bundleIdentifier
            let workspace: *mut AnyObject =
                unsafe { msg_send![class!(NSWorkspace), sharedWorkspace] };
            if workspace.is_null() {
                return None;
            }
            let app: *mut AnyObject = unsafe { msg_send![workspace, frontmostApplication] };
            if app.is_null() {
                return None;
            }
            let identifier: *mut AnyObject = unsafe { msg_send![app, bundleIdentifier] };
            if identifier.is_null() {
                return None;
            }
            let utf8: *const c_char = unsafe { msg_send![identifier, UTF8String] };
            if utf8.is_null() {
                return None;
            }
            Some(
                unsafe { CStr::from_ptr(utf8) }
                    .to_string_lossy()
                    .to_string(),
            )
        })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn frontmost_bundle_id() -> Option<String> {
        None
    }

    #[cfg(target_os = "macos")]
    pub fn frontmost_window_title() -> Option<String> {
        use std::ffi::c_void;
        use std::ptr;

        use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
        use core_foundation::string::{CFString, CFStringRef};
        use objc2::rc::autoreleasepool;
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};

        type AXUIElementRef = *const c_void;
        type AXError = i32;

        unsafe extern "C" {
            fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
            fn AXUIElementCopyAttributeValue(
                element: AXUIElementRef,
                attribute: CFStringRef,
                value: *mut CFTypeRef,
            ) -> AXError;
        }

        autoreleasepool(|_| {
            let workspace: *mut AnyObject =
                unsafe { msg_send![class!(NSWorkspace), sharedWorkspace] };
            if workspace.is_null() {
                return None;
            }
            let app: *mut AnyObject = unsafe { msg_send![workspace, frontmostApplication] };
            if app.is_null() {
                return None;
            }
            let pid: i32 = unsafe { msg_send![app, processIdentifier] };
            if pid <= 0 {
                return None;
            }

            let app_el = unsafe { AXUIElementCreateApplication(pid) };
            if app_el.is_null() {
                return None;
            }

            let focused_window_attr = CFString::new("AXFocusedWindow");
            let mut window_value: CFTypeRef = ptr::null();
            let window_status = unsafe {
                AXUIElementCopyAttributeValue(
                    app_el,
                    focused_window_attr.as_concrete_TypeRef(),
                    &mut window_value,
                )
            };
            unsafe {
                CFRelease(app_el.cast_mut());
            }
            if window_status != 0 || window_value.is_null() {
                return None;
            }

            let title_attr = CFString::new("AXTitle");
            let mut title_value: CFTypeRef = ptr::null();
            let title_status = unsafe {
                AXUIElementCopyAttributeValue(
                    window_value.cast(),
                    title_attr.as_concrete_TypeRef(),
                    &mut title_value,
                )
            };
            unsafe {
                CFRelease(window_value.cast_mut());
            }
            if title_status != 0 || title_value.is_null() {
                return None;
            }

            let title =
                unsafe { CFString::wrap_under_create_rule(title_value as CFStringRef).to_string() };
            Some(title)
        })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn frontmost_window_title() -> Option<String> {
        None
    }
}

pub fn load_window_title_allowlist() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Vec::new();
    }

    let path = PathBuf::from(home).join(".ambient").join("config.toml");
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };

    parse_allowlist_from_toml(&raw)
}

pub fn parse_allowlist_from_toml(raw: &str) -> Vec<String> {
    let mut in_section = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[") {
            in_section = trimmed == "[window_title_allowlist]";
            continue;
        }
        if !in_section {
            continue;
        }
        if !trimmed.starts_with("apps") {
            continue;
        }

        let Some((_, rhs)) = trimmed.split_once('=') else {
            continue;
        };
        let rhs = rhs.trim();
        if !(rhs.starts_with('[') && rhs.ends_with(']')) {
            continue;
        }

        let inner = &rhs[1..rhs.len() - 1];
        return inner
            .split(',')
            .map(str::trim)
            .filter_map(|v| {
                if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
                    Some(v[1..v.len() - 1].to_string())
                } else {
                    None
                }
            })
            .collect();
    }

    Vec::new()
}
