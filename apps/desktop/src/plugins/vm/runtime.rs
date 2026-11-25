//! One OS thread and independent bounded `QuickJS` heap per plugin session.
use super::{allocator::HeapAllocator, loader::Modules, natives::Bridge, timers::Timers};
use crate::plugins::{Error, Result, host_calls::HostContext, invocation::Pending};
use rquickjs::{Context, Function, Module, Runtime, promise::PromiseState};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as channel,
    },
    time::Duration,
};
use tokio::sync::mpsc;

pub(super) const MAX_FRAME: usize = 16 * 1024 * 1024;
/// An independent plugin heap: not the UI's allocator or runtime.
const WRITE_QUEUE_FRAMES: usize = 32;
const EVENT_QUEUE_ITEMS: usize = 4096;
const HEAP_BYTES: usize = 64 * 1024 * 1024;
const STACK_BYTES: usize = 512 * 1024;
const CPU_BUDGET: Duration = Duration::from_secs(5);

pub(in crate::plugins) struct Session {
    pub writer: channel::SyncSender<Vec<u8>>,
    pub output: Option<mpsc::Receiver<Vec<u8>>>,
    pub stop: Arc<AtomicBool>,
    pub thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn_blocking(move || {
                    let _ = thread.join();
                });
            } else {
                let _ = thread.join();
            }
        }
    }
}

pub(in crate::plugins) fn spawn(
    modules: Modules,
    entry: String,
    pending: &Arc<Pending>,
    host: HostContext,
) -> Result<Session> {
    let (writer, from_host) = channel::sync_channel(WRITE_QUEUE_FRAMES);
    let (to_host, output) = mpsc::channel(EVENT_QUEUE_ITEMS);
    let stop = Arc::new(AtomicBool::new(false));
    let handle = tokio::runtime::Handle::current();
    let thread_stop = Arc::clone(&stop);
    let thread_pending = Arc::clone(pending);
    let thread = std::thread::Builder::new()
        .name("plugin-vm".into())
        .spawn(move || {
            let result = run(
                modules,
                &entry,
                "sdk/worker.js",
                &from_host,
                &to_host,
                &thread_stop,
                Some((Arc::clone(&thread_pending), host, handle)),
            );
            if result.is_err() {
                tracing::warn!(target: "plugin", "plugin VM failed; generation discarded");
            }
            thread_pending.close("plugin VM stopped");
            thread_stop.store(true, Ordering::Release);
        })?;
    Ok(Session {
        writer,
        output: Some(output),
        stop,
        thread: Some(thread),
    })
}

pub(super) fn run(
    modules: Modules,
    entry: &str,
    main: &str,
    input: &channel::Receiver<Vec<u8>>,
    output: &mpsc::Sender<Vec<u8>>,
    stop: &Arc<AtomicBool>,
    capabilities: Option<(Arc<Pending>, HostContext, tokio::runtime::Handle)>,
) -> Result<()> {
    let (allocator, exhausted) = HeapAllocator::new(HEAP_BYTES);
    let rt = Runtime::new_with_alloc(allocator).map_err(vm_error)?;
    rt.set_max_stack_size(STACK_BYTES);
    rt.set_loader(modules, modules);
    let deadline = Rc::new(Cell::new(Duration::MAX));
    let fatal = Arc::new(AtomicBool::new(false));
    rt.set_interrupt_handler(Some(Box::new({
        let stop = Arc::clone(stop);
        let deadline = Rc::clone(&deadline);
        let fatal = Arc::clone(&fatal);
        let exhausted = Arc::clone(&exhausted);
        move || {
            let interrupt = stop.load(Ordering::Acquire)
                || exhausted.load(Ordering::Acquire)
                || charged_now() >= deadline.get();
            if interrupt {
                fatal.store(true, Ordering::Release);
            }
            interrupt
        }
    })));
    let context = Context::full(&rt).map_err(vm_error)?;
    context.with(|ctx| -> Result<()> {
        install_session(&ctx, entry, output, &fatal, stop, &exhausted)?;
        super::globals::install(&ctx).map_err(vm_error)?;
        let timers = Timers::new();
        timers.install(&ctx).map_err(vm_error)?;
        let (done_tx, done_rx) = channel::channel();
        let bridge = capabilities.map(|(pending, host, handle)| {
            Rc::new(Bridge {
                host,
                pending,
                handle,
                done: done_tx,
                waiting: RefCell::new(HashMap::new()),
                sequence: Cell::new(0),
            })
        });
        if let Some(bridge) = &bridge {
            bridge.install(&ctx).map_err(vm_error)?;
        }
        let operation = (|| -> Result<()> {
            deadline.set(charged_now().saturating_add(CPU_BUDGET));
            let source = modules
                .source(main)
                .ok_or_else(|| Error::Runtime("plugin module unavailable".into()))?;
            let loading_started = std::time::Instant::now();
            let promise = Module::evaluate(ctx.clone(), main, source).map_err(vm_error)?;
            loop {
                check_limits(stop, &fatal, &exhausted)?;
                if loading_started.elapsed() > Duration::from_secs(10)
                    && promise.state() == PromiseState::Pending
                {
                    return Err(Error::Runtime("plugin module loading timed out".into()));
                }
                deadline.set(charged_now().saturating_add(CPU_BUDGET));
                drain_jobs(&ctx, stop, &fatal, &exhausted)?;
                if promise.state() == PromiseState::Rejected {
                    // Consume the exception, without exposing upstream text or source paths.
                    let _ = promise.result::<rquickjs::Value<'_>>();
                    let _reason = ctx.catch();
                    return Err(Error::Runtime("plugin module evaluation rejected".into()));
                }
                if main == "sdk/collect.js" && promise.state() == PromiseState::Resolved {
                    return Ok(());
                }
                deadline.set(Duration::MAX);
                match input.recv_timeout(timers.wait()) {
                    Ok(frame) => {
                        if frame.len() > MAX_FRAME {
                            return Err(Error::Runtime("plugin input exceeded limit".into()));
                        }
                        deadline.set(charged_now().saturating_add(CPU_BUDGET));
                        let recv: Function<'_> = ctx.globals().get("__recv").map_err(vm_error)?;
                        let line = String::from_utf8(frame)
                            .map_err(|_| Error::Runtime("invalid plugin frame".into()))?;
                        recv.call::<_, ()>((line,)).map_err(vm_error)?;
                    }
                    Err(channel::RecvTimeoutError::Timeout) => {}
                    Err(channel::RecvTimeoutError::Disconnected) => return Ok(()),
                }
                deadline.set(charged_now().saturating_add(CPU_BUDGET));
                if let Some(bridge) = &bridge {
                    bridge.cancel_finished(&ctx).map_err(vm_error)?;
                    for done in done_rx.try_iter() {
                        bridge.complete(&ctx, done).map_err(vm_error)?;
                    }
                }
                drain_jobs(&ctx, stop, &fatal, &exhausted)?;
                timers.run().map_err(vm_error)?;
            }
        })();
        if let Some(bridge) = &bridge {
            bridge.waiting.borrow_mut().clear();
        }
        timers.clear();
        operation
    })
}

fn install_session(
    ctx: &rquickjs::Ctx<'_>,
    entry: &str,
    output: &mpsc::Sender<Vec<u8>>,
    fatal: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    exhausted: &Arc<AtomicBool>,
) -> Result<()> {
    let entry = entry.to_owned();
    let entry_native = Function::new(ctx.clone(), move || entry.clone()).map_err(vm_error)?;
    ctx.globals()
        .set("__entry", entry_native)
        .map_err(vm_error)?;
    let send_native = Function::new(ctx.clone(), {
        let output = output.clone();
        let fatal = Arc::clone(fatal);
        let stop = Arc::clone(stop);
        let exhausted = Arc::clone(exhausted);
        move |frame: String| {
            if fatal.load(Ordering::Acquire)
                || stop.load(Ordering::Acquire)
                || exhausted.load(Ordering::Acquire)
            {
                return;
            }
            if frame.len() > MAX_FRAME || output.try_send(frame.into_bytes()).is_err() {
                fatal.store(true, Ordering::Release);
                tracing::warn!(target: "plugin", "plugin output exceeded session limit");
            }
        }
    })
    .map_err(vm_error)?;
    ctx.globals().set("__send", send_native).map_err(vm_error)?;
    Ok(())
}

fn check_limits(stop: &AtomicBool, fatal: &AtomicBool, exhausted: &AtomicBool) -> Result<()> {
    if stop.load(Ordering::Acquire)
        || fatal.load(Ordering::Acquire)
        || exhausted.load(Ordering::Acquire)
    {
        return Err(Error::Runtime("plugin VM resource limit or stop".into()));
    }
    Ok(())
}

fn drain_jobs(
    ctx: &rquickjs::Ctx<'_>,
    stop: &AtomicBool,
    fatal: &AtomicBool,
    exhausted: &AtomicBool,
) -> Result<()> {
    // One budget is shared by the entire drain, including recursively enqueued jobs.
    while ctx.execute_pending_job() {
        check_limits(stop, fatal, exhausted)?;
    }
    check_limits(stop, fatal, exhausted)
}

fn vm_error(_error: rquickjs::Error) -> Error {
    Error::Runtime("plugin VM evaluation failed".into())
}
#[cfg(unix)]
fn charged_now() -> Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the VM thread supplies a writable timespec; this clock measures only this thread.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut time) };
    if result != 0 {
        return Duration::MAX;
    }
    Duration::new(
        u64::try_from(time.tv_sec).unwrap_or_default(),
        u32::try_from(time.tv_nsec).unwrap_or_default(),
    )
}
#[cfg(windows)]
fn charged_now() -> Duration {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentThread, GetThreadTimes},
    };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: pseudo-handle of the current thread and four writable FILETIMEs.
    let success = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if success == 0 {
        return Duration::MAX;
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Duration::from_nanos(
        ticks(kernel)
            .saturating_add(ticks(user))
            .saturating_mul(100),
    )
}
