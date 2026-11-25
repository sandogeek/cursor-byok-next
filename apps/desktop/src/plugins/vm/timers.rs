//! Bounded timer handles owned and called on the VM thread.
use rquickjs::{Ctx, Exception, Function, function::Opt};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::{Duration, Instant},
};
struct Timer<'js> {
    due: Instant,
    interval: Option<Duration>,
    callback: Function<'js>,
}
pub(super) struct Timers<'js> {
    entries: RefCell<BTreeMap<u64, Timer<'js>>>,
    sequence: Cell<u64>,
}
impl<'js> Timers<'js> {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            entries: RefCell::new(BTreeMap::new()),
            sequence: Cell::new(0),
        })
    }
    pub fn install(self: &Rc<Self>, ctx: &Ctx<'js>) -> rquickjs::Result<()> {
        for (name, repeat) in [("setTimeout", false), ("setInterval", true)] {
            let timers = Rc::clone(self);
            ctx.globals().set(
                name,
                Function::new(
                    ctx.clone(),
                    move |ctx: Ctx<'js>,
                          callback: Function<'js>,
                          delay: Opt<f64>|
                          -> rquickjs::Result<u64> {
                        if timers.entries.borrow().len() >= 1024 {
                            return Err(Exception::throw_range(&ctx, "too many plugin timers"));
                        }
                        let ms = delay.0.unwrap_or_default().clamp(1.0, 2_147_483_647.0);
                        let duration = Duration::from_secs_f64(if ms.is_finite() {
                            ms / 1000.0
                        } else {
                            0.001
                        });
                        let id = timers.sequence.get();
                        timers.sequence.set(id.wrapping_add(1));
                        timers.entries.borrow_mut().insert(
                            id,
                            Timer {
                                due: Instant::now() + duration,
                                interval: repeat.then_some(duration),
                                callback,
                            },
                        );
                        Ok(id)
                    },
                )?,
            )?;
        }
        for name in ["clearTimeout", "clearInterval"] {
            let timers = Rc::clone(self);
            ctx.globals().set(
                name,
                Function::new(ctx.clone(), move |id: u64| {
                    timers.entries.borrow_mut().remove(&id);
                })?,
            )?;
        }
        Ok(())
    }
    pub fn run(&self) -> rquickjs::Result<()> {
        let now = Instant::now();
        let due: Vec<_> = self
            .entries
            .borrow()
            .iter()
            .filter(|(_, timer)| timer.due <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let Some(mut timer) = self.entries.borrow_mut().remove(&id) else {
                continue;
            };
            let callback = timer.callback.clone();
            if let Some(interval) = timer.interval {
                timer.due = now + interval;
                self.entries.borrow_mut().insert(id, timer);
            }
            callback.call::<_, ()>(())?;
        }
        Ok(())
    }
    pub fn wait(&self) -> Duration {
        self.entries
            .borrow()
            .values()
            .map(|timer| timer.due.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_millis(5))
            .min(Duration::from_millis(5))
    }
    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
    }
}
