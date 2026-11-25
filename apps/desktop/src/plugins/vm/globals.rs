//! Minimal globals. No NSH, Node, filesystem, process or ambient networking.
use rquickjs::{Ctx, Exception, Function};
pub(super) fn install(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let parse_url = Function::new(
        ctx.clone(),
        |ctx: Ctx<'_>, input: String| -> rquickjs::Result<String> {
            let url =
                url::Url::parse(&input).map_err(|_| Exception::throw_type(&ctx, "invalid URL"))?;
            Ok(serde_json::json!({
                "href": url.as_str(),
                "protocol": format!("{}:", url.scheme()),
                "username": url.username(),
                "password": url.password().unwrap_or_default(),
                "hostname": url.host_str().unwrap_or_default(),
                "origin": url.origin().ascii_serialization(),
            })
            .to_string())
        },
    )?;
    ctx.globals().set("__url", parse_url)?;
    let log = Function::new(ctx.clone(), |level: String, message: String| {
        let message = message.chars().take(4096).collect::<String>();
        tracing::info!(target: "plugin", level, message, "plugin console");
    })?;
    ctx.globals().set("__log", log)?;
    ctx.eval::<(), _>(r"
(() => {
 const parse = globalThis.__url, log = globalThis.__log;
 delete globalThis.__url; delete globalThis.__log;
 globalThis.URL = class URL {
   constructor(value) { Object.assign(this, JSON.parse(parse(String(value)))); }
   toString() { return this.href; }
 };
 globalThis.AbortController = class AbortController {
   constructor() {
     const listeners = new Set();
     this.signal = { aborted: false, addEventListener(type, listener) { if (type === 'abort') listeners.add(listener); }, removeEventListener(type, listener) { if (type === 'abort') listeners.delete(listener); } };
     this.abort = () => { if (this.signal.aborted) return; this.signal.aborted = true; for (const listener of [...listeners]) { try { listener({type:'abort', target:this.signal}); } catch {} } listeners.clear(); };
   }
 };
 globalThis.console = Object.fromEntries(['log','info','warn','error','debug'].map(level => [level, (...args) => log(level, args.map(value => { try { return typeof value === 'string' ? value : JSON.stringify(value); } catch { return '[unprintable]'; } }).join(' '))]));
})();
")?;
    // Make absence intentional rather than relying on an external module loader.
    for name in [
        "fetch",
        "WebSocket",
        "process",
        "require",
        "structuredClone",
    ] {
        ctx.globals().remove(name)?;
    }
    Ok(())
}
