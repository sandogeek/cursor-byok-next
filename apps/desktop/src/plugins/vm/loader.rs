//! Resolves only modules present in the embedded table; never touches the disk.
use rquickjs::{
    Ctx, Module,
    loader::{Loader, Resolver},
    module::Declared,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Modules(pub &'static [(&'static str, &'static str)]);
impl Modules {
    pub fn embedded() -> Self {
        Self(super::super::builtin::MODULES)
    }
    pub fn source(self, name: &str) -> Option<&'static str> {
        self.0
            .iter()
            .find(|(id, _)| *id == name)
            .map(|(_, source)| *source)
    }
    pub fn resolve(self, base: &str, name: &str) -> Option<String> {
        let id = if let Some(suffix) = name.strip_prefix("cursor-byok-next:") {
            if !["plugin", "provider", "model", "resource", "host"].contains(&suffix) {
                return None;
            }
            format!("sdk/{suffix}.js")
        } else if name.starts_with("./") || name.starts_with("../") {
            let mut parts: Vec<&str> = base.rsplit_once('/')?.0.split('/').collect();
            for part in name.split('/') {
                match part {
                    "." => {}
                    ".." => {
                        parts.pop()?;
                    }
                    "" => return None,
                    value => parts.push(value),
                }
            }
            parts.join("/")
        } else if self.source(name).is_some() {
            name.to_owned()
        } else {
            return None;
        };
        self.source(&id).map(|_| id)
    }
}
impl Resolver for Modules {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        (*self)
            .resolve(base, name)
            .ok_or_else(|| rquickjs::Error::new_resolving(base, name))
    }
}
impl Loader for Modules {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        Module::declare(
            ctx.clone(),
            name,
            self.source(name)
                .ok_or_else(|| rquickjs::Error::new_loading(name))?,
        )
    }
}
