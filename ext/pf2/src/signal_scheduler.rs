#![deny(unsafe_op_in_unsafe_fn)]

mod configuration;
mod timer_installer;

use self::configuration::{Configuration, TimeMode};
use self::timer_installer::TimerInstaller;
use crate::profile::Profile;
use crate::profile_serializer::ProfileSerializer;
use crate::sample::Sample;

use core::panic;
use std::collections::HashSet;
use std::ffi::{c_int, c_void, CString};
use std::mem::ManuallyDrop;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;
use std::{mem, ptr::null_mut};

use rb_sys::*;

use crate::util::*;

#[derive(Debug)]
pub struct SignalScheduler {
    configuration: configuration::Configuration,
    profile: Option<Arc<RwLock<Profile>>>,
}

pub struct SignalHandlerArgs {
    profile: Arc<RwLock<Profile>>,
    context_ruby_thread: VALUE,
}

impl SignalScheduler {
    fn new() -> Self {
        Self {
            configuration: Configuration {
                time_mode: TimeMode::CpuTime,
            },
            profile: None,
        }
    }

    fn start(&mut self, _rbself: VALUE, ruby_threads_rary: VALUE) -> VALUE {
        let profile = Arc::new(RwLock::new(Profile::new()));
        self.start_profile_buffer_flusher_thread(&profile);
        self.install_signal_handler();

        let mut target_ruby_threads = HashSet::new();
        unsafe {
            for i in 0..RARRAY_LEN(ruby_threads_rary) {
                let ruby_thread: VALUE = rb_ary_entry(ruby_threads_rary, i);
                target_ruby_threads.insert(ruby_thread);
            }
        }
        TimerInstaller::install_timer_to_ruby_threads(
            self.configuration.clone(),
            &target_ruby_threads,
            Arc::clone(&profile),
        );

        self.profile = Some(profile);

        Qtrue.into()
    }

    fn stop(&mut self, _rbself: VALUE) -> VALUE {
        if let Some(profile) = &self.profile {
            // Finalize
            match profile.try_write() {
                Ok(mut profile) => {
                    profile.flush_temporary_sample_buffer();
                }
                Err(_) => {
                    println!("[pf2 ERROR] stop: Failed to acquire profile lock.");
                    return Qfalse.into();
                }
            }

            let profile = profile.try_read().unwrap();
            println!(
                "[pf2 DEBUG] Elapsed time: {:?}",
                profile.samples.last().unwrap().timestamp - profile.start_timestamp
            );
            println!("[pf2 DEBUG] Number of samples: {}", profile.samples.len());

            let serialized = ProfileSerializer::serialize(&profile);
            let serialized = CString::new(serialized).unwrap();
            unsafe { rb_str_new_cstr(serialized.as_ptr()) }
        } else {
            panic!("stop() called before start()");
        }
    }

    // Install signal handler for profiling events to the current process.
    fn install_signal_handler(&self) {
        let mut sa: libc::sigaction = unsafe { mem::zeroed() };
        sa.sa_sigaction = Self::signal_handler as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        let err = unsafe { libc::sigaction(libc::SIGALRM, &sa, null_mut()) };
        if err != 0 {
            panic!("sigaction failed: {}", err);
        }
    }

    // Respond to the signal and collect a sample.
    // This function is called when a timer fires.
    //
    // Expected to be async-signal-safe, but the current implementation is not.
    extern "C" fn signal_handler(
        _sig: c_int,
        info: *mut libc::siginfo_t,
        _ucontext: *mut libc::ucontext_t,
    ) {
        let args = unsafe {
            let ptr = extract_si_value_sival_ptr(info) as *mut SignalHandlerArgs;
            ManuallyDrop::new(Box::from_raw(ptr))
        };

        let mut profile = match args.profile.try_write() {
            Ok(profile) => profile,
            Err(_) => {
                // FIXME: Do we want to properly collect GC samples? I don't know yet.
                println!("[pf2 DEBUG] Failed to acquire profile lock (garbage collection possibly in progress). Dropping sample.");
                return;
            }
        };

        let sample = Sample::capture(args.context_ruby_thread); // NOT async-signal-safe
        if profile.temporary_sample_buffer.push(sample).is_err() {
            panic!("[pf2 DEBUG] Temporary sample buffer full. Dropping sample.");
        }
    }

    fn start_profile_buffer_flusher_thread(&self, profile: &Arc<RwLock<Profile>>) {
        let profile = Arc::clone(profile);
        thread::spawn(move || loop {
            println!("[pf2 DEBUG] Flushing temporary sample buffer");
            match profile.try_write() {
                Ok(mut profile) => {
                    profile.flush_temporary_sample_buffer();
                }
                Err(_) => {
                    println!("[pf2 ERROR] Failed to acquire profile lock");
                }
            }
            thread::sleep(Duration::from_millis(500));
        });
    }

    // Ruby Methods

    pub unsafe extern "C" fn rb_start(rbself: VALUE, ruby_threads: VALUE) -> VALUE {
        let mut collector = unsafe { Self::get_struct_from(rbself) };
        collector.start(rbself, ruby_threads)
    }

    pub unsafe extern "C" fn rb_stop(rbself: VALUE) -> VALUE {
        let mut collector = unsafe { Self::get_struct_from(rbself) };
        collector.stop(rbself)
    }

    // Functions for TypedData

    // Extract the SignalScheduler struct from a Ruby object
    unsafe fn get_struct_from(obj: VALUE) -> ManuallyDrop<Box<Self>> {
        unsafe {
            let ptr = rb_check_typeddata(obj, &RBDATA);
            ManuallyDrop::new(Box::from_raw(ptr as *mut SignalScheduler))
        }
    }

    #[allow(non_snake_case)]
    pub unsafe extern "C" fn rb_alloc(_rbself: VALUE) -> VALUE {
        let collector = Box::new(SignalScheduler::new());
        unsafe { Arc::increment_strong_count(&collector) };

        unsafe {
            let rb_mPf2: VALUE = rb_define_module(cstr!("Pf2"));
            let rb_cSignalScheduler =
                rb_define_class_under(rb_mPf2, cstr!("SignalScheduler"), rb_cObject);

            // "Wrap" the SignalScheduler struct into a Ruby object
            rb_data_typed_object_wrap(
                rb_cSignalScheduler,
                Box::into_raw(collector) as *mut c_void,
                &RBDATA,
            )
        }
    }

    unsafe extern "C" fn dmark(ptr: *mut c_void) {
        unsafe {
            let collector = ManuallyDrop::new(Box::from_raw(ptr as *mut SignalScheduler));
            if let Some(profile) = &collector.profile {
                match profile.read() {
                    Ok(profile) => {
                        profile.dmark();
                    }
                    Err(_) => {
                        panic!("[pf2 FATAL] dmark: Failed to acquire profile lock.");
                    }
                }
            }
        }
    }

    unsafe extern "C" fn dfree(ptr: *mut c_void) {
        unsafe {
            drop(Box::from_raw(ptr as *mut SignalScheduler));
        }
    }

    unsafe extern "C" fn dsize(_: *const c_void) -> size_t {
        // FIXME: Report something better
        mem::size_of::<SignalScheduler>() as size_t
    }
}

static mut RBDATA: rb_data_type_t = rb_data_type_t {
    wrap_struct_name: cstr!("SignalScheduler"),
    function: rb_data_type_struct__bindgen_ty_1 {
        dmark: Some(SignalScheduler::dmark),
        dfree: Some(SignalScheduler::dfree),
        dsize: Some(SignalScheduler::dsize),
        dcompact: None,
        reserved: [null_mut(); 1],
    },
    parent: null_mut(),
    data: null_mut(),
    flags: 0,
};
