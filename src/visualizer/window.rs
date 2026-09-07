//! Small GTK/WebKit host, isolated from iced in its own process. Use the C ABI
//! because WebKitGTK's Rust bindings and GStreamer's bindings use different
//! versions of GLib. All GTK calls and callbacks run on this process's main thread.
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use anyhow::{Context, Result, bail};
use gstreamer::glib::{ffi, gobject_ffi};

#[link(name = "gtk-3")]
unsafe extern "C" {
    fn gtk_init_check(argc: *mut c_int, argv: *mut *mut *mut c_char) -> c_int;
    fn gtk_window_new(kind: c_int) -> *mut c_void;
    fn gtk_window_set_title(window: *mut c_void, title: *const c_char);
    fn gtk_window_set_default_size(window: *mut c_void, width: c_int, height: c_int);
    fn gtk_window_set_decorated(window: *mut c_void, decorated: c_int);
    fn gtk_container_add(container: *mut c_void, widget: *mut c_void);
    fn gtk_widget_show_all(widget: *mut c_void);
    fn gtk_main();
    fn gtk_main_quit();
}

#[link(name = "webkit2gtk-4.1")]
unsafe extern "C" {
    fn webkit_web_context_new_ephemeral() -> *mut c_void;
    fn webkit_web_context_allow_tls_certificate_for_host(
        context: *mut c_void,
        certificate: *mut c_void,
        host: *const c_char,
    );
    fn webkit_web_view_new_with_context(context: *mut c_void) -> *mut c_void;
    fn webkit_web_view_load_uri(view: *mut c_void, uri: *const c_char);
    fn webkit_web_view_get_settings(view: *mut c_void) -> *mut c_void;
    fn webkit_settings_set_enable_webgl(settings: *mut c_void, enabled: c_int);
    fn webkit_settings_set_enable_fullscreen(settings: *mut c_void, enabled: c_int);
    fn webkit_settings_set_enable_developer_extras(settings: *mut c_void, enabled: c_int);
    fn webkit_settings_set_enable_write_console_messages_to_stdout(
        settings: *mut c_void,
        enabled: c_int,
    );
    fn webkit_navigation_policy_decision_get_request(decision: *mut c_void) -> *mut c_void;
    fn webkit_uri_request_get_uri(request: *mut c_void) -> *const c_char;
    fn webkit_policy_decision_ignore(decision: *mut c_void);
    fn webkit_download_set_destination(download: *mut c_void, uri: *const c_char);
    fn webkit_download_cancel(download: *mut c_void);
}

#[link(name = "gio-2.0")]
unsafe extern "C" {
    fn g_tls_certificate_new_from_pem(
        data: *const c_char,
        length: isize,
        error: *mut *mut ffi::GError,
    ) -> *mut c_void;
}

unsafe extern "C" fn destroyed(_: *mut c_void, _: *mut c_void) {
    // SAFETY: GTK invokes this on its main thread during window destruction.
    unsafe { gtk_main_quit() };
}

unsafe extern "C" fn enter_fullscreen(_: *mut c_void, window: *mut c_void) -> c_int {
    // SAFETY: this signal runs on GTK's main thread; the containing window
    // outlives its WebView. Returning false lets WebKit fullscreen that window.
    unsafe {
        gtk_window_set_decorated(window, 0);
    }
    0
}

unsafe extern "C" fn leave_fullscreen(_: *mut c_void, window: *mut c_void) -> c_int {
    // SAFETY: same ownership as enter_fullscreen; WebKit restores window geometry.
    unsafe {
        gtk_window_set_decorated(window, 1);
    }
    0
}

unsafe extern "C" fn load_failed(
    _: *mut c_void,
    _: c_int,
    _: *const c_char,
    error: *mut ffi::GError,
    _: *mut c_void,
) -> c_int {
    if !error.is_null() {
        // SAFETY: WebKit borrows the GError for the duration of this signal.
        unsafe {
            crate::app_log!(
                "[VISUALIZER] local page load failed, code {}: {}",
                (*error).code,
                CStr::from_ptr((*error).message).to_string_lossy()
            );
        }
    }
    0
}

fn navigation_allowed(uri: &str, origin: &str) -> bool {
    uri == "about:blank" || uri.starts_with(&format!("{origin}/"))
}

unsafe extern "C" fn decide_policy(
    _: *mut c_void,
    decision: *mut c_void,
    kind: c_int,
    data: *mut c_void,
) -> c_int {
    if kind != 0 && kind != 1 {
        return 0;
    }
    // SAFETY: the signal supplies a navigation decision; data points to the
    // boxed origin kept alive throughout gtk_main below. URI is borrowed only here.
    unsafe {
        let request = webkit_navigation_policy_decision_get_request(decision);
        if request.is_null() {
            return 0;
        }
        let uri = webkit_uri_request_get_uri(request);
        if !uri.is_null()
            && navigation_allowed(
                &CStr::from_ptr(uri).to_string_lossy(),
                &*(data as *const String),
            )
        {
            return 0;
        }
        webkit_policy_decision_ignore(decision);
    }
    1
}

unsafe extern "C" fn destination(
    download: *mut c_void,
    suggested: *const c_char,
    _: *mut c_void,
) -> c_int {
    let result = (|| -> Result<CString> {
        // SAFETY: WebKit supplies a NUL-terminated suggested basename during this callback.
        let suggested = if suggested.is_null() {
            "cartridge.png".into()
        } else {
            unsafe { CStr::from_ptr(suggested) }
                .to_string_lossy()
                .into_owned()
        };
        let basename: String = suggested
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
            .take(140)
            .collect();
        let directory = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .context("No home directory available for the cartridge download")?
            .join("Downloads/Static");
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{}-{basename}", uuid::Uuid::new_v4()));
        let uri =
            url::Url::from_file_path(path).map_err(|_| anyhow::anyhow!("Invalid download path"))?;
        Ok(CString::new(uri.as_str())?)
    })();
    // SAFETY: download is owned by WebKit and live for this signal. The setter copies URI.
    unsafe {
        match result {
            Ok(uri) => webkit_download_set_destination(download, uri.as_ptr()),
            Err(err) => {
                crate::app_log!("[VISUALIZER] download destination failed: {err}");
                webkit_download_cancel(download);
            }
        }
    }
    1
}

unsafe extern "C" fn download_started(_: *mut c_void, download: *mut c_void, _: *mut c_void) {
    // SAFETY: signal names/signatures are WebKit's public ABI; null data needs no destructor.
    unsafe {
        gobject_ffi::g_signal_connect_data(
            download.cast(),
            c"decide-destination".as_ptr(),
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> c_int,
                unsafe extern "C" fn(),
            >(destination)),
            ptr::null_mut(),
            None,
            0,
        );
    }
}

pub fn run(page_url: &str, certificate_pem: &str) -> Result<()> {
    let url = url::Url::parse(page_url)?;
    if url.scheme() != "https" || url.host_str() != Some("127.0.0.1") {
        bail!("Visualizer must use its local HTTPS server");
    }
    let mut origin = Box::new(url.origin().ascii_serialization());
    let uri = CString::new(page_url)?;
    let pem = CString::new(certificate_pem)?;
    // SAFETY: this entry point runs in a dedicated process's main thread. GTK
    // owns its widgets; context/certificate have explicit refs released below.
    // CString arguments and callback data outlive the GTK event loop.
    unsafe {
        if gtk_init_check(ptr::null_mut(), ptr::null_mut()) == 0 {
            bail!("No graphical display is available for the visualizer");
        }
        let context = webkit_web_context_new_ephemeral();
        let mut error = ptr::null_mut();
        let certificate = g_tls_certificate_new_from_pem(pem.as_ptr(), -1, &mut error);
        if certificate.is_null() {
            let detail = if error.is_null() {
                "Certificate could not be loaded".to_owned()
            } else {
                let detail = CStr::from_ptr((*error).message)
                    .to_string_lossy()
                    .into_owned();
                ffi::g_error_free(error);
                detail
            };
            gobject_ffi::g_object_unref(context.cast());
            bail!("{detail}");
        }
        webkit_web_context_allow_tls_certificate_for_host(
            context,
            certificate,
            c"127.0.0.1".as_ptr(),
        );
        gobject_ffi::g_object_unref(certificate.cast());
        let view = webkit_web_view_new_with_context(context);
        let window = gtk_window_new(0);
        gtk_window_set_title(window, c"Static Visualiser".as_ptr());
        gtk_window_set_default_size(window, 1200, 850);
        let settings = webkit_web_view_get_settings(view);
        webkit_settings_set_enable_webgl(settings, 1);
        webkit_settings_set_enable_fullscreen(settings, 1);
        for (signal, handler) in [
            (
                c"enter-fullscreen",
                enter_fullscreen as unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
            ),
            (
                c"leave-fullscreen",
                leave_fullscreen as unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
            ),
        ] {
            gobject_ffi::g_signal_connect_data(
                view.cast(),
                signal.as_ptr(),
                Some(std::mem::transmute::<
                    unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
                    unsafe extern "C" fn(),
                >(handler)),
                window,
                None,
                0,
            );
        }
        webkit_settings_set_enable_developer_extras(settings, 0);
        webkit_settings_set_enable_write_console_messages_to_stdout(
            settings,
            i32::from(std::env::var_os("STATIC_VISUALIZER_DEBUG").is_some()),
        );
        gobject_ffi::g_signal_connect_data(
            view.cast(),
            c"load-failed".as_ptr(),
            Some(std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut c_void,
                    c_int,
                    *const c_char,
                    *mut ffi::GError,
                    *mut c_void,
                ) -> c_int,
                unsafe extern "C" fn(),
            >(load_failed)),
            ptr::null_mut(),
            None,
            0,
        );
        gobject_ffi::g_signal_connect_data(
            window.cast(),
            c"destroy".as_ptr(),
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut c_void, *mut c_void),
                unsafe extern "C" fn(),
            >(destroyed)),
            ptr::null_mut(),
            None,
            0,
        );
        gobject_ffi::g_signal_connect_data(
            view.cast(),
            c"decide-policy".as_ptr(),
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, *mut c_void) -> c_int,
                unsafe extern "C" fn(),
            >(decide_policy)),
            (&mut *origin as *mut String).cast(),
            None,
            0,
        );
        gobject_ffi::g_signal_connect_data(
            context.cast(),
            c"download-started".as_ptr(),
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
                unsafe extern "C" fn(),
            >(download_started)),
            ptr::null_mut(),
            None,
            0,
        );
        gtk_container_add(window, view);
        webkit_web_view_load_uri(view, uri.as_ptr());
        gtk_widget_show_all(window);
        gtk_main();
        gobject_ffi::g_object_unref(context.cast());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn window_navigation_stays_on_its_exact_local_server() {
        let origin = "https://127.0.0.1:40001";
        assert!(navigation_allowed(
            "https://127.0.0.1:40001/#token=test",
            origin
        ));
        assert!(navigation_allowed(
            "https://127.0.0.1:40001/cartridges/test",
            origin
        ));
        for uri in [
            "https://127.0.0.1:40002/",
            "https://127.0.0.1:40001.evil/",
            "https://example.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            assert!(!navigation_allowed(uri, origin));
        }
    }
}
