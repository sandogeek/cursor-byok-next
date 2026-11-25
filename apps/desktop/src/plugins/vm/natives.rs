//! Native I/O bridge. JS handles stay on their owning VM thread.
use super::host_module::{HostDone, HostError};
use crate::plugins::{
    adapters,
    host_calls::HostContext,
    invocation::{InvocationBudget, Pending},
};
use rquickjs::{Ctx, Exception, Function, Promise};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
};

pub(super) struct Waiting<'js> {
    id: String,
    resolve: Function<'js>,
    reject: Function<'js>,
    budget: Arc<InvocationBudget>,
}
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.budget.release_host_call();
    }
}
pub(super) struct Bridge<'js> {
    pub host: HostContext,
    pub pending: Arc<Pending>,
    pub handle: tokio::runtime::Handle,
    pub done: std::sync::mpsc::Sender<HostDone>,
    pub waiting: RefCell<HashMap<u64, Waiting<'js>>>,
    pub sequence: Cell<u64>,
}
impl<'js> Bridge<'js> {
    pub fn install(self: &Rc<Self>, ctx: &Ctx<'js>) -> rquickjs::Result<()> {
        let bridge = Rc::clone(self);
        let call = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  id: String,
                  method: String,
                  json: String|
                  -> rquickjs::Result<Promise<'js>> {
                let (promise, resolve, reject) = ctx.promise()?;
                let Some((invocation, mut active, budget)) = bridge.pending.authorization(&id)
                else {
                    reject.call::<_, ()>(("plugin invocation is no longer active",))?;
                    return Ok(promise);
                };
                if !budget.try_acquire_host_call() {
                    reject.call::<_, ()>(("too many concurrent host calls",))?;
                    return Ok(promise);
                }
                let sequence = bridge.sequence.get();
                bridge.sequence.set(sequence.wrapping_add(1));
                bridge.waiting.borrow_mut().insert(
                    sequence,
                    Waiting {
                        id,
                        resolve,
                        reject,
                        budget,
                    },
                );
                let host = bridge.host.clone();
                let sender = bridge.done.clone();
                #[cfg(test)]
                let task_guard = host.task_guard();
                bridge.handle.spawn(async move {
                    #[cfg(test)]
                    let _task_guard = task_guard;
                    let result = tokio::select! {
                        biased;
                        _ = active.changed() => Err(HostError { message: "plugin invocation is no longer active".into(), status: None }),
                        () = invocation.cancelled() => Err(crate::Error::Cancelled.into()),
                        result = async {
                            let params = serde_json::from_str(&json).map_err(crate::Error::from)?;
                            let value = if method == "protocol.stream" {
                                adapters::stream(&host, &invocation, params).await?
                            } else {
                                host.call(&invocation, &method, params).await?
                            };
                            serde_json::to_string(&value).map_err(crate::Error::from)
                        } => result.map_err(HostError::from),
                    };
                    let _ = sender.send(HostDone { sequence, result });
                });
                Ok(promise)
            },
        )?;
        ctx.globals().set("__host", call)?;
        let bridge = Rc::clone(self);
        ctx.globals().set(
            "__readRequest",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, id: String| -> rquickjs::Result<String> {
                    let Some((invocation, _, _)) = bridge.pending.authorization(&id) else {
                        return Err(Exception::throw_type(
                            &ctx,
                            "plugin invocation is no longer active",
                        ));
                    };
                    let value = bridge
                        .host
                        .read_request(&invocation)
                        .map_err(|_| Exception::throw_type(&ctx, "model request is unavailable"))?;
                    serde_json::to_string(&value)
                        .map_err(|_| Exception::throw_type(&ctx, "model request is invalid"))
                },
            )?,
        )?;
        Ok(())
    }
    pub fn complete(&self, ctx: &Ctx<'js>, done: HostDone) -> rquickjs::Result<()> {
        let Some(waiting) = self.waiting.borrow_mut().remove(&done.sequence) else {
            return Ok(());
        };
        let result = if self.pending.authorization(&waiting.id).is_some() {
            done.result
        } else {
            Err(HostError {
                message: "plugin invocation is no longer active".into(),
                status: None,
            })
        };
        match result {
            Ok(json) => waiting.resolve.call::<_, ()>((json,)),
            Err(error) => {
                let exception = Exception::from_message(ctx.clone(), &error.message)?;
                if let Some(status) = error.status {
                    exception.as_object().set("status", status)?;
                }
                waiting.reject.call::<_, ()>((exception,))
            }
        }
    }
    pub fn cancel_finished(&self, ctx: &Ctx<'js>) -> rquickjs::Result<()> {
        let finished: Vec<u64> = self
            .waiting
            .borrow()
            .iter()
            .filter(|(_, waiting)| {
                self.pending
                    .authorization(&waiting.id)
                    .is_none_or(|(state, _, _)| state.is_cancelled())
            })
            .map(|(key, _)| *key)
            .collect();
        for sequence in finished {
            self.complete(
                ctx,
                HostDone {
                    sequence,
                    result: Err(crate::Error::Cancelled.into()),
                },
            )?;
        }
        Ok(())
    }
}
