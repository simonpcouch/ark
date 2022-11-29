//
// r_interface.rs
//
// Copyright (C) 2022 by Posit, PBC
//
//

use amalthea::socket::iopub::IOPubMessage;
use harp::lock::R_RUNTIME_LOCK;
use harp::lock::R_RUNTIME_TASKS_PENDING;
use harp::routines::r_register_routines;
use harp::utils::r_get_option;
use libR_sys::*;
use libc::{c_char, c_int};
use log::*;
use std::ffi::{CStr, CString};
use std::os::raw::c_uchar;
use std::sync::MutexGuard;
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::Duration;
use std::time::SystemTime;
use stdext::*;

use crate::kernel::Kernel;
use crate::kernel::KernelInfo;
use crate::request::Request;

extern "C" {
    fn R_ProcessEvents();
    fn run_Rmainloop();
    pub static mut R_PolledEvents: Option<unsafe extern "C" fn()>;
}

// --- Globals ---
// These values must be global in order for them to be accessible from R
// callbacks, which do not have a facility for passing or returning context.

/// The global R kernel state.
pub static mut KERNEL: Option<Arc<Mutex<Kernel>>> = None;

/// A lock guard, used to manage access to the R runtime.  The main thread holds
/// the lock by default, but releases it at opportune times to allow the LSP to
/// access the R runtime where appropriate.
pub static mut R_RUNTIME_LOCK_GUARD: Option<MutexGuard<()>> = None;

/// A channel that sends prompts from R to the kernel
static mut RPROMPT_SEND: Option<Mutex<Sender<String>>> = None;

/// A channel that receives console input from the kernel and sends it to R;
/// sending empty input (None) tells R to shut down
static mut CONSOLE_RECV: Option<Mutex<Receiver<Option<String>>>> = None;

/// Ensures that the kernel is only ever initialized once
static INIT: Once = Once::new();

fn on_console_input(buf: *mut c_uchar, buflen: c_int, mut input: String) {

    // TODO: What if the input is too large for the buffer?
    input.push_str("\n");
    if input.len() > buflen as usize {
        info!("Error: input too large for buffer.");
        return;
    }

    let src = CString::new(input).unwrap();
    unsafe {
        libc::strcpy(buf as *mut c_char, src.as_ptr());
    }

}

/// Invoked by R to read console input from the user.
///
/// * `prompt` - The prompt shown to the user
/// * `buf`    - Pointer to buffer to receive the user's input (type `CONSOLE_BUFFER_CHAR`)
/// * `buflen` - Size of the buffer to receiver user's input
/// * `hist`   - Whether to add the input to the history (1) or not (0)
///
#[no_mangle]
pub extern "C" fn r_read_console(
    prompt: *const c_char,
    buf: *mut c_uchar,
    buflen: c_int,
    _hist: c_int,
) -> i32 {
    let r_prompt = unsafe { CStr::from_ptr(prompt) };
    debug!("R prompt: {}", r_prompt.to_str().unwrap());

    // If the prompt begins with "Save workspace", respond with (n)
    if r_prompt.to_str().unwrap().starts_with("Save workspace") {
        let n = CString::new("n\n").unwrap();
        unsafe {
            libc::strcpy(buf as *mut c_char, n.as_ptr());
        }
        return 1;
    }

    // TODO: if R prompt is +, we need to tell the user their input is incomplete
    let mutex = unsafe { RPROMPT_SEND.as_ref().unwrap() };
    let sender = mutex.lock().unwrap();
    sender
        .send(r_prompt.to_string_lossy().into_owned())
        .unwrap();

    let mutex = unsafe { CONSOLE_RECV.as_ref().unwrap() };
    let receiver = mutex.lock().unwrap();

    // Match with a timeout. Necessary because we need to
    // pump the event loop while waiting for console input.
    loop {

        // Release the R runtime lock while we're waiting for input.
        unsafe { R_RUNTIME_LOCK_GUARD = None };

        match receiver.recv_timeout(Duration::from_millis(200)) {

            Ok(response) => {

                // Take back the lock after we've received some console input.
                unsafe { R_RUNTIME_LOCK_GUARD = Some(R_RUNTIME_LOCK.as_ref().unwrap_unchecked().lock().unwrap()) };

                if let Some(input) = response {
                    on_console_input(buf, buflen, input);
                }

                return 1;

            }

            Err(error) => {

                use std::sync::mpsc::RecvTimeoutError::*;
                unsafe { R_RUNTIME_LOCK_GUARD = Some(R_RUNTIME_LOCK.as_ref().unwrap_unchecked().lock().unwrap()) };

                match error {

                    Timeout => {

                        // Pump the event loop.
                        unsafe { R_ProcessEvents() };

                        // Keep waiting for console input.
                        continue;

                    }

                    Disconnected => {

                        return 1;

                    }
                }
            }
        }

    }

}

#[no_mangle]
pub extern "C" fn r_write_console(buf: *const c_char, _buflen: i32, otype: i32) {
    let content = unsafe { CStr::from_ptr(buf) };
    let mutex = unsafe { KERNEL.as_ref().unwrap() };
    let mut kernel = mutex.lock().unwrap();
    kernel.write_console(content.to_str().unwrap(), otype);
}

#[no_mangle]
pub unsafe extern "C" fn r_polled_events() {

    // NOTE: Routines here (currently) panic intentionally if we cannot
    // unwrap or acquire the requisite locks, as these events basically
    // should never happen and we don't have a way to recover if they do.
    //
    // This routine is called very frequently when the console is idle,
    // to ensure that the LSP has an opportunity to respond to completion
    // requests. It's important that calls be as cheap as possible when
    // the LSP does not require access to the R main thread.
    if !R_RUNTIME_TASKS_PENDING {
        return;
    }

    // Release the runtime lock.
    info!("Tasks are pending; the main thread is releasing the R runtime lock.");
    let now = SystemTime::now();
    unsafe { R_RUNTIME_LOCK_GUARD = None };

    // Take back the runtime lock.
    unsafe { R_RUNTIME_LOCK_GUARD = Some(R_RUNTIME_LOCK.as_ref().unwrap_unchecked().lock().unwrap()) };
    info!("The main thread re-acquired the R runtime lock after {} milliseconds.", now.elapsed().unwrap().as_millis());

}

pub fn start_r(
    iopub: SyncSender<IOPubMessage>,
    receiver: Receiver<Request>,
    initializer: Sender<KernelInfo>,
) {
    use std::borrow::BorrowMut;

    // The main thread owns the R runtime lock by default, but releases
    // it when appropriate to give other threads a chance to execute.
    unsafe { R_RUNTIME_LOCK_GUARD = Some(R_RUNTIME_LOCK.as_ref().unwrap_unchecked().lock().unwrap()) };

    // Start building the channels + kernel objects
    let (console_send, console_recv) = channel::<Option<String>>();
    let (rprompt_send, rprompt_recv) = channel::<String>();
    let console = console_send.clone();
    let kernel = Kernel::new(iopub, console, initializer);

    // Initialize kernel (ensure we only do this once!)
    INIT.call_once(|| unsafe {
        *CONSOLE_RECV.borrow_mut() = Some(Mutex::new(console_recv));
        *RPROMPT_SEND.borrow_mut() = Some(Mutex::new(rprompt_send));
        *KERNEL.borrow_mut() = Some(Arc::new(Mutex::new(kernel)));
    });

    // Start thread to listen to execution requests
    thread::spawn(move || listen(receiver, rprompt_recv));

    unsafe {

        let mut args = cargs!["ark", "--interactive"];
        R_running_as_main_program = 1;
        R_SignalHandlers = 0;
        Rf_initialize_R(args.len() as i32, args.as_mut_ptr() as *mut *mut c_char);

        // Disable stack checking; R doesn't know the starting point of the
        // stack for threads other than the main thread. Consequently, it will
        // report a stack overflow if we don't disable it. This is a problem
        // on all platforms, but is most obvious on aarch64 Linux due to how
        // thread stacks are allocated on that platform.
        //
        // See https://cran.r-project.org/doc/manuals/R-exts.html#Threading-issues
        // for more information.
        R_CStackLimit = usize::MAX;

        // Log the value of R_HOME, so we can know if something hairy is afoot
        let home = CStr::from_ptr(R_HomeDir());
        trace!("R_HOME: {:?}", home);

        // Mark R session as interactive
        R_Interactive = 1;

        // Redirect console
        R_Consolefile = std::ptr::null_mut();
        R_Outputfile = std::ptr::null_mut();

        ptr_R_WriteConsole = None;
        ptr_R_WriteConsoleEx = Some(r_write_console);
        ptr_R_ReadConsole = Some(r_read_console);

        // Listen for polled events
        R_PolledEvents = Some(r_polled_events);

        // Set up main loop
        setup_Rmainloop();

        // Register embedded routines
        r_register_routines();

        // Run the main loop -- does not return
        run_Rmainloop();
    }
}

fn handle_r_request(req: &Request, prompt_recv: &Receiver<String>) {
    // Service the request.
    let mutex = unsafe { KERNEL.as_ref().unwrap() };
    {
        let mut kernel = mutex.lock().unwrap();
        kernel.fulfill_request(&req)
    }

    // If this is an execution request, complete it by waiting for R to prompt
    // us before we process another request
    if let Request::ExecuteCode(_, _, _) = req {
        complete_execute_request(req, prompt_recv);
    }
}

fn complete_execute_request(req: &Request, prompt_recv: &Receiver<String>) {
    let mutex = unsafe { KERNEL.as_ref().unwrap() };

    // Wait for R to prompt us again. This signals that the
    // execution is finished and R is ready for input again.
    trace!("Waiting for R prompt signaling completion of execution...");
    let prompt = prompt_recv.recv().unwrap();
    let kernel = mutex.lock().unwrap();

    // if the prompt is '+', we need to tell the kernel the request is incomplete
    if prompt.starts_with("+") {
        trace!("Got R prompt '{}', marking request incomplete", prompt);
        return kernel.report_incomplete_request(&req);
    }

    // Figure out what the default prompt looks like.
    let default_prompt = unsafe { r_get_option::<String>("prompt") };
    let default_prompt = unwrap!(default_prompt, Err(error) => {
        warn!("Failed to read R prompt: {}", error);
        return kernel.finish_request();
    });

    // If the current prompt doesn't match the default prompt, assume that
    // we're reading use input, e.g. via 'readline()'.
    if prompt != default_prompt {

        trace!("Got R prompt '{}', asking user for input", prompt);
        if let Request::ExecuteCode(_, originator, _) = req {
            kernel.request_input(originator, &prompt);
        } else {
            warn!("No originator for input request, omitting");
            let originator: Vec<u8> = Vec::new();
            kernel.request_input(&originator, &prompt);
        }

        trace!("Input requested, waiting for reply...");
        return;
    }

    // Default prompt, finishing request
    trace!("Got R prompt '{}', completing execution", prompt);
    return kernel.finish_request();

}

pub fn listen(exec_recv: Receiver<Request>, prompt_recv: Receiver<String>) {
    // Before accepting execution requests from the front end, wait for R to
    // prompt us for input.
    trace!("Waiting for R's initial input prompt...");
    let prompt = prompt_recv.recv().unwrap();
    trace!(
        "Got initial R prompt '{}', ready for execution requests",
        prompt
    );

    // Mark kernel as initialized as soon as we get the first input prompt from R
    let mutex = unsafe { KERNEL.as_ref().unwrap() };
    {
        let mut kernel = mutex.lock().unwrap();
        kernel.complete_intialization();
    }

    loop {
        // Wait for an execution request from the front end.
        match exec_recv.recv() {
            Ok(req) => handle_r_request(&req, &prompt_recv),
            Err(err) => warn!("Could not receive execution request from kernel: {}", err),
        }
    }
}
