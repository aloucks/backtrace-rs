//! A library for acquiring a backtrace at runtime
//!
//! This library is meant to supplement the `RUST_BACKTRACE=1` support of the
//! standard library by allowing an acquisition of a backtrace at runtime
//! programmatically. The backtraces generated by this library do not need to be
//! parsed, for example, and expose the functionality of multiple backend
//! implementations.
//!
//! # Implementation
//!
//! This library makes use of a number of strategies for actually acquiring a
//! backtrace. For example unix uses libgcc's libunwind bindings by default to
//! acquire a backtrace, but coresymbolication or dladdr is used on OSX to
//! acquire symbol names while linux uses gcc's libbacktrace.
//!
//! When using the default feature set of this library the "most reasonable" set
//! of defaults is chosen for the current platform, but the features activated
//! can also be controlled at a finer granularity.
//!
//! # Platform Support
//!
//! Currently this library is verified to work on Linux, OSX, and Windows, but
//! it may work on other platforms as well. Note that the quality of the
//! backtrace may vary across platforms.
//!
//! # API Principles
//!
//! This library attempts to be as flexible as possible to accommodate different
//! backend implementations of acquiring a backtrace. Consequently the currently
//! exported functions are closure-based as opposed to the likely expected
//! iterator-based versions. This is done due to limitations of the underlying
//! APIs used from the system.
//!
//! # Usage
//!
//! First, add this to your Cargo.toml
//!
//! ```toml
//! [dependencies]
//! backtrace = "0.2"
//! ```
//!
//! Next:
//!
//! ```
//! extern crate backtrace;
//!
//! fn main() {
//! # // Unsafe here so test passes on no_std.
//! # #[cfg(feature = "std")] {
//!     backtrace::trace(|frame| {
//!         let ip = frame.ip();
//!         let symbol_address = frame.symbol_address();
//!
//!         // Resolve this instruction pointer to a symbol name
//!         backtrace::resolve(ip, |symbol| {
//!             if let Some(name) = symbol.name() {
//!                 // ...
//!             }
//!             if let Some(filename) = symbol.filename() {
//!                 // ...
//!             }
//!         });
//!
//!         true // keep going to the next frame
//!     });
//! }
//! # }
//! ```

#![doc(html_root_url = "https://docs.rs/backtrace")]
#![deny(missing_docs)]
#![no_std]
#![cfg_attr(target_env = "sgx", feature(sgx_platform))]

#[cfg(feature = "std")]
#[macro_use] extern crate std;

#[cfg(any(unix, target_env = "sgx"))]
extern crate libc;
#[cfg(windows)]
extern crate winapi;

#[cfg(feature = "serde_derive")]
#[cfg_attr(feature = "serde_derive", macro_use)]
extern crate serde_derive;

#[cfg(feature = "rustc-serialize")]
extern crate rustc_serialize;

#[macro_use]
extern crate cfg_if;

extern crate rustc_demangle;

#[cfg(feature = "cpp_demangle")]
extern crate cpp_demangle;

cfg_if! {
    if #[cfg(all(feature = "gimli-symbolize", unix, target_os = "linux"))] {
        extern crate addr2line;
        extern crate findshlibs;
        extern crate gimli;
        extern crate memmap;
        extern crate object;
    }
}

#[allow(dead_code)] // not used everywhere
#[cfg(unix)]
#[macro_use]
mod dylib;

pub use backtrace::{trace_unsynchronized, Frame};
mod backtrace;

pub use symbolize::{resolve_unsynchronized, Symbol, SymbolName};
mod symbolize;

pub use types::BytesOrWideString;
mod types;

cfg_if! {
    if #[cfg(feature = "std")] {
        pub use backtrace::trace;
        pub use symbolize::resolve;
        pub use capture::{Backtrace, BacktraceFrame, BacktraceSymbol};
        mod capture;
    }
}

#[allow(dead_code)]
struct Bomb {
    enabled: bool,
}

#[allow(dead_code)]
impl Drop for Bomb {
    fn drop(&mut self) {
        if self.enabled {
            panic!("cannot panic during the backtrace function");
        }
    }
}

#[allow(dead_code)]
#[cfg(feature = "std")]
mod lock {
    use std::cell::Cell;
    use std::boxed::Box;
    use std::sync::{Once, Mutex, MutexGuard, ONCE_INIT};

    pub struct LockGuard(MutexGuard<'static, ()>);

    static mut LOCK: *mut Mutex<()> = 0 as *mut _;
    static INIT: Once = ONCE_INIT;
    thread_local!(static LOCK_HELD: Cell<bool> = Cell::new(false));

    impl Drop for LockGuard {
        fn drop(&mut self) {
            LOCK_HELD.with(|slot| {
                assert!(slot.get());
                slot.set(false);
            });
        }
    }

    pub fn lock() -> Option<LockGuard> {
        if LOCK_HELD.with(|l| l.get()) {
            return None
        }
        LOCK_HELD.with(|s| s.set(true));
        unsafe {
            INIT.call_once(|| {
                LOCK = Box::into_raw(Box::new(Mutex::new(())));
            });
            Some(LockGuard((*LOCK).lock().unwrap()))
        }
    }
}

#[cfg(all(windows, feature = "dbghelp"))]
struct Cleanup {
    handle: winapi::um::winnt::HANDLE,
    opts: winapi::shared::minwindef::DWORD,
}

#[cfg(all(windows, feature = "dbghelp"))]
unsafe fn dbghelp_init() -> Option<Cleanup> {
    use winapi::shared::minwindef;
    use winapi::um::{dbghelp, processthreadsapi};

    use std::sync::{Mutex, Once, ONCE_INIT};
    use std::boxed::Box;

    // Initializing symbols has significant overhead, but initializing only once
    // without cleanup causes problems for external sources. For example, the
    // standard library checks the result of SymInitializeW (which returns an
    // error if attempting to initialize twice) and in the event of an error,
    // will not print a backtrace on panic. Presumably, external debuggers may
    // have similar issues.
    //
    // As a compromise, we'll keep track of the number of internal initialization
    // requests within a single API call in order to minimize the number of
    // init/cleanup cycles.
    static mut REF_COUNT: *mut Mutex<usize> = 0 as *mut _;
    static mut INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        REF_COUNT = Box::into_raw(Box::new(Mutex::new(0)));
    });

    // Not sure why these are missing in winapi
    const SYMOPT_DEFERRED_LOADS: minwindef::DWORD = 0x00000004;
    extern "system" {
        fn SymGetOptions() -> minwindef::DWORD;
        fn SymSetOptions(options: minwindef::DWORD);
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            unsafe {
                let mut ref_count_guard = (&*REF_COUNT).lock().unwrap();
                *ref_count_guard -= 1;

                if *ref_count_guard == 0 {
                    dbghelp::SymCleanup(self.handle);
                    SymSetOptions(self.opts);
                }
            }
        }
    }

    let opts = SymGetOptions();
    let handle = processthreadsapi::GetCurrentProcess();

    let mut ref_count_guard = (&*REF_COUNT).lock().unwrap();

    if *ref_count_guard > 0 {
        *ref_count_guard += 1;
        return Some(Cleanup { handle, opts });
    }

    SymSetOptions(opts | SYMOPT_DEFERRED_LOADS);

    let ret = dbghelp::SymInitializeW(handle,
                                      0 as *mut _,
                                      minwindef::TRUE);

    if ret != minwindef::TRUE {
        // Symbols may have been initialized by another library or an external debugger
        None
    } else {
        *ref_count_guard += 1;
        Some(Cleanup { handle, opts })
    }
}