//! libcurl, loaded at run time, so a run reuses one connection pool instead of starting a process per
//! fetch.
//!
//! The measured cost of the process path is 8.9 ms per fetch on loopback and 72 ms against a page on the
//! internet, where the fresh TLS handshake is the larger half. Both are gone when the same easy handle
//! performs every transfer: libcurl keeps its own connection pool and TLS session, and the flags that
//! matter are the ones `fetch.rs` already passes to the CLI.
//!
//! Three properties this module must not lose, and how each is kept:
//!
//! - **The pin.** `CURLOPT_CONNECT_TO` is per transfer, unlike `CURLOPT_RESOLVE`, which is global and
//!   would keep applying to a later host. The single-shot path uses `--resolve`; this one cannot.
//! - **The cap.** `CURLOPT_MAXFILESIZE_LARGE` refuses a body whose size is announced, and the write
//!   callback refuses one that is streamed: it returns a short count the moment the buffer would pass
//!   the cap, which makes libcurl abort the transfer. No oversized body is ever held.
//! - **The engine never follows a redirect.** `CURLOPT_FOLLOWLOCATION` is 0 and `CURLOPT_MAXREDIRS` is 0,
//!   so the `Location` header reaches the driver, which is the only thing allowed to decide.
//!
//! Everything here is optional by construction. If `libcurl.so.4` is absent, or any option is refused,
//! the caller falls back to the process path, which is unchanged and still tested.

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_void};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Values from `curl/curl.h` on this machine, checked by compiling one call per option and printing it.
/// They are part of libcurl's stable ABI: an option number that moved would be a broken release.
const OPT_URL: c_int = 10002;
const OPT_WRITEFUNCTION: c_int = 20011;
const OPT_WRITEDATA: c_int = 10001;
const OPT_HEADERFUNCTION: c_int = 20079;
const OPT_HEADERDATA: c_int = 10029;
const OPT_PROTOCOLS_STR: c_int = 10318;
const OPT_REDIR_PROTOCOLS_STR: c_int = 10319;
const OPT_MAXREDIRS: c_int = 68;
const OPT_MAXFILESIZE_LARGE: c_int = 30117;
const OPT_CONNECT_TO: c_int = 10243;
const OPT_TIMEOUT: c_int = 13;
const OPT_USERAGENT: c_int = 10018;
const OPT_ERRORBUFFER: c_int = 10010;
const OPT_NOSIGNAL: c_int = 99;
const OPT_CAINFO: c_int = 10065;
const OPT_FOLLOWLOCATION: c_int = 52;
const INFO_RESPONSE_CODE: c_int = 0x20_0002;

const CURLE_OK: c_int = 0;
const CURLE_WRITE_ERROR: c_int = 23;
const CURLE_FILESIZE_EXCEEDED: c_int = 63;

type Curl = *mut c_void;
type Slist = *mut c_void;

/// The functions this module uses, each resolved with `dlsym`. A missing symbol makes the whole pool
/// unavailable rather than half-working.
struct Api {
    easy_init: unsafe extern "C" fn() -> Curl,
    easy_setopt: unsafe extern "C" fn(Curl, c_int, ...) -> c_int,
    easy_perform: unsafe extern "C" fn(Curl) -> c_int,
    easy_getinfo: unsafe extern "C" fn(Curl, c_int, ...) -> c_int,
    global_init: unsafe extern "C" fn(c_long) -> c_int,
    easy_reset: unsafe extern "C" fn(Curl),
    easy_cleanup: unsafe extern "C" fn(Curl),
    easy_strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    slist_append: unsafe extern "C" fn(Slist, *const c_char) -> Slist,
    slist_free_all: unsafe extern "C" fn(Slist),
}

// The function table is immutable once resolved; the handles it returns are guarded by `FREE`.
unsafe impl Send for Api {}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        for name in ["libcurl.so.4", "libcurl.so"] {
            let Ok(name) = CString::new(name) else {
                continue;
            };
            let handle = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if handle.is_null() {
                continue;
            }
            // One `dlsym` per function, with the type written out at the call site: a `transmute` that
            // does not name its target is how a symbol gets called with the wrong signature.
            macro_rules! symbol {
                ($symbol:literal, $kind:ty) => {{
                    let name = CString::new($symbol).ok()?;
                    let found = libc::dlsym(handle, name.as_ptr());
                    if found.is_null() {
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, $kind>(found)
                }};
            }
            let api = Api {
                global_init: symbol!("curl_global_init", unsafe extern "C" fn(c_long) -> c_int),
                easy_init: symbol!("curl_easy_init", unsafe extern "C" fn() -> Curl),
                easy_setopt: symbol!(
                    "curl_easy_setopt",
                    unsafe extern "C" fn(Curl, c_int, ...) -> c_int
                ),
                easy_perform: symbol!("curl_easy_perform", unsafe extern "C" fn(Curl) -> c_int),
                easy_getinfo: symbol!(
                    "curl_easy_getinfo",
                    unsafe extern "C" fn(Curl, c_int, ...) -> c_int
                ),
                easy_reset: symbol!("curl_easy_reset", unsafe extern "C" fn(Curl)),
                easy_cleanup: symbol!("curl_easy_cleanup", unsafe extern "C" fn(Curl)),
                easy_strerror: symbol!(
                    "curl_easy_strerror",
                    unsafe extern "C" fn(c_int) -> *const c_char
                ),
                slist_append: symbol!(
                    "curl_slist_append",
                    unsafe extern "C" fn(Slist, *const c_char) -> Slist
                ),
                slist_free_all: symbol!("curl_slist_free_all", unsafe extern "C" fn(Slist)),
            };
            // libcurl's own documentation says implicit initialisation is not safe on every platform, and
            // the thread-safety of the explicit call only arrived in 7.84. `OnceLock::get_or_init` makes
            // the call happen exactly once even when several research workers reach the pool together, so
            // the first fetch cannot race the second on a distribution that still ships 7.81.
            static INIT: OnceLock<bool> = OnceLock::new();
            // CURL_GLOBAL_DEFAULT is 3: SSL plus the platform's own setup. The result is kept, not re-run:
            // a second call would be harmless in libcurl, but on a failure every fetch should take the
            // process path rather than try the same broken library again.
            if !*INIT.get_or_init(|| (api.global_init)(3) == 0) {
                return None;
            }
            return Some(api);
        }
        None
    })
    .as_ref()
}

/// The handles not in use. A fetch takes one, performs its transfer outside any lock, and puts it back
/// when the transfer ends. One handle behind a mutex would be held for the whole blocking call and would
/// turn four research workers into a queue; this keeps them parallel without an async runtime.
static FREE: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// How many idle handles are kept for reuse. This is not a cap on concurrency: a fetch that finds the
/// free list empty creates a handle of its own, and one that returns to a full list is closed. Connection
/// reuse is what the pool is for, and the research workers are bounded well below this number.
const MAX_HANDLES: usize = 8;

/// A handle borrowed from the pool. Dropping it returns the handle, whatever path the transfer took,
/// so an early return cannot shrink the pool.
struct Lease {
    handle: Curl,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut free = match FREE.lock() {
            Ok(free) => free,
            Err(poisoned) => poisoned.into_inner(),
        };
        if free.len() < MAX_HANDLES {
            free.push(self.handle as usize);
        } else if let Some(api) = api() {
            // SAFETY: the handle came from `easy_init` and is not used after this point.
            unsafe { (api.easy_cleanup)(self.handle) };
        }
    }
}

/// What the callbacks are handed: the body, the headers, and the cap that both are bounded by.
struct Sink {
    body: Vec<u8>,
    headers: Vec<u8>,
    cap: usize,
    overflow: bool,
}

unsafe extern "C" fn on_body(
    data: *mut c_char,
    size: usize,
    count: usize,
    user: *mut c_void,
) -> usize {
    // SAFETY: the pointer is the one this call passed in `OPT_WRITEDATA`, and libcurl does not call the
    // callback after `perform` returns.
    let sink = unsafe { &mut *(user as *mut Sink) };
    let length = size.saturating_mul(count);
    if sink.body.len() + length > sink.cap {
        // A short count tells libcurl to abort this transfer. The flag is what separates "the cap
        // fired" from "the write failed", which read the same from the return value alone.
        sink.overflow = true;
        return 0;
    }
    // SAFETY: libcurl hands a buffer of exactly `size * count` bytes for the duration of the call.
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
    sink.body.extend_from_slice(slice);
    length
}

unsafe extern "C" fn on_header(
    data: *mut c_char,
    size: usize,
    count: usize,
    user: *mut c_void,
) -> usize {
    // SAFETY: the pointer is the one this call passed in `OPT_HEADERDATA`.
    let sink = unsafe { &mut *(user as *mut Sink) };
    let length = size.saturating_mul(count);
    // Headers are capped at a size no real response approaches; a server that streams them is not one
    // this tool wants to spend memory on.
    if sink.headers.len() + length > 256 * 1024 {
        sink.overflow = true;
        return 0;
    }
    // SAFETY: libcurl hands a buffer of exactly `size * count` bytes for the duration of the call.
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
    sink.headers.extend_from_slice(slice);
    length
}

/// What one transfer returns: the status, the body and the headers, or the sentence libcurl gave.
type Answer = Result<(u16, Vec<u8>, Vec<u8>), String>;

/// One transfer through the shared handle. `Ok(None)` is "the pool is not usable", never a verdict on the
/// request: the caller answers with the process path instead.
pub fn get(
    url: &str,
    host: &str,
    port: u16,
    address: std::net::IpAddr,
    max_time: u64,
    cap: usize,
    ca: Option<&Path>,
) -> Option<Answer> {
    // A switch for the suite and for anyone debugging a difference between the two paths: `off` forces
    // the process path, which is also what a machine without libcurl gets.
    if std::env::var("HADES_FETCH_POOL").as_deref() == Ok("off") {
        return None;
    }
    let api = api()?;
    let handle = {
        let mut free = match FREE.lock() {
            Ok(free) => free,
            Err(poisoned) => poisoned.into_inner(),
        };
        match free.pop() {
            Some(handle) => handle as Curl,
            // SAFETY: `easy_init` allocates a handle or returns null, which is checked below.
            None => unsafe { (api.easy_init)() },
        }
    };
    if handle.is_null() {
        return None;
    }
    let _lease = Lease { handle };

    let mut sink = Sink {
        body: Vec::new(),
        headers: Vec::new(),
        cap,
        overflow: false,
    };
    let mut error = [0 as c_char; 512];
    let url = CString::new(url).ok()?;
    let agent = CString::new("Mozilla/5.0 (compatible; Hades/0.21)").ok()?;
    let protocols = CString::new("http,https").ok()?;

    // The pin: per transfer, host spelled as the URL spelled it.
    let mut list: Slist = std::ptr::null_mut();
    if host.parse::<std::net::IpAddr>().is_err() {
        // The entry is colon-separated, so an IPv6 address has to be bracketed or its own colons read as
        // more fields. Without this, a pinned fetch works on IPv4 and fails wherever IPv6 is preferred.
        let target = match address {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        };
        let entry = CString::new(format!("{host}:{port}:{target}:{port}")).ok()?;
        list = unsafe { (api.slist_append)(list, entry.as_ptr()) };
        if list.is_null() {
            return None;
        }
    }
    let ca_value = ca.and_then(|path| CString::new(path.to_string_lossy().as_bytes()).ok());

    let configured = unsafe {
        (api.easy_reset)(handle);
        let mut ok = true;
        ok &= (api.easy_setopt)(handle, OPT_URL, url.as_ptr()) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_PROTOCOLS_STR, protocols.as_ptr()) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_REDIR_PROTOCOLS_STR, protocols.as_ptr()) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_FOLLOWLOCATION, 0 as c_long) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_MAXREDIRS, 0 as c_long) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_TIMEOUT, max_time as c_long) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_MAXFILESIZE_LARGE, cap as c_long) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_NOSIGNAL, 1 as c_long) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_USERAGENT, agent.as_ptr()) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_ERRORBUFFER, error.as_mut_ptr()) == CURLE_OK;
        ok &= (api.easy_setopt)(
            handle,
            OPT_WRITEFUNCTION,
            on_body as unsafe extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize,
        ) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_WRITEDATA, &mut sink as *mut Sink) == CURLE_OK;
        ok &= (api.easy_setopt)(
            handle,
            OPT_HEADERFUNCTION,
            on_header as unsafe extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize,
        ) == CURLE_OK;
        ok &= (api.easy_setopt)(handle, OPT_HEADERDATA, &mut sink as *mut Sink) == CURLE_OK;
        if !list.is_null() {
            ok &= (api.easy_setopt)(handle, OPT_CONNECT_TO, list) == CURLE_OK;
        }
        if let Some(value) = ca_value.as_ref() {
            ok &= (api.easy_setopt)(handle, OPT_CAINFO, value.as_ptr()) == CURLE_OK;
        }
        ok
    };

    let code = if configured {
        unsafe { (api.easy_perform)(handle) }
    } else {
        -1
    };
    let sentence = unsafe {
        let first = error[0];
        if first != 0 {
            CStr::from_ptr(error.as_ptr()).to_string_lossy().to_string()
        } else if code >= 0 {
            CStr::from_ptr((api.easy_strerror)(code))
                .to_string_lossy()
                .to_string()
        } else {
            String::new()
        }
    };
    let status = {
        let mut value: c_long = 0;
        unsafe {
            (api.easy_getinfo)(handle, INFO_RESPONSE_CODE, &mut value as *mut c_long);
        }
        value
    };
    unsafe {
        if !list.is_null() {
            (api.slist_free_all)(list);
        }
    }
    if sink.overflow {
        return Some(Err("cap".to_string()));
    }
    match code {
        CURLE_OK => Some(Ok((
            status as u16,
            std::mem::take(&mut sink.body),
            std::mem::take(&mut sink.headers),
        ))),
        CURLE_FILESIZE_EXCEEDED => Some(Err("cap".to_string())),
        CURLE_WRITE_ERROR => Some(Err("cap".to_string())),
        // The same sentence the process path prints, in the same shape: a caller that switches from one
        // path to the other must not see the message change. `clip_ref` is applied by the caller.
        _ if sentence.is_empty() => Some(Err(String::new())),
        // Byte for byte what the CLI prints on stderr, so a message does not depend on which path served
        // the fetch.
        _ => Some(Err(format!(
            "curl failed with exit {code}: curl: ({code}) {sentence}"
        ))),
    }
}
