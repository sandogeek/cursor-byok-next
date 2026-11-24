//! Resolves Task model selections to configured plugin model identities.

use crate::{Error, Result};

/// A configured model, borrowed by the resolution so picking one allocates
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByokModel<'a> {
    pub hash: &'a str,
    pub model_id: &'a str,
    pub display_name: &'a str,
}

/// Resolves a Task model selection to a configured model hash.
///
/// A hash or an exact model id wins; otherwise a display name is matched with
/// and without its trailing `(...)` suffix.
///
/// # Errors
///
/// Returns [`Error::Protocol`] when a display name matches several configured
/// models, because guessing would record the call under the wrong model.
pub(crate) fn resolve<'models>(
    models: &'models [ByokModel<'_>],
    requested: &'models str,
) -> Result<&'models str> {
    if let Some(model) = models.iter().find(|model| model.hash == requested) {
        return Ok(model.hash);
    }
    let key = display_key(requested);
    let mut matches = models
        .iter()
        .filter(|model| model.model_id == requested || display_key(model.display_name) == key)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        let key = display_key(without_suffix(requested));
        matches = models
            .iter()
            .filter(|model| display_key(without_suffix(model.display_name)) == key)
            .collect();
    }
    match matches.as_slice() {
        [] => Ok(requested),
        [model] => Ok(model.hash),
        _ => Err(Error::Protocol(format!(
            "Task model {requested:?} is ambiguous; use a configured model hash: {}",
            matches
                .iter()
                .map(|model| model.hash)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn display_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

fn without_suffix(name: &str) -> &str {
    let name = name.trim_end();
    match name.strip_suffix(')').and_then(|rest| rest.rfind('(')) {
        Some(start) => name[..start].trim_end(),
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models<'a>(entries: &'a [(&'a str, &'a str, &'a str)]) -> Vec<ByokModel<'a>> {
        entries
            .iter()
            .map(|&(hash, model_id, display_name)| ByokModel {
                hash,
                model_id,
                display_name,
            })
            .collect()
    }

    #[test]
    fn hashes_take_precedence_over_other_models_aliases() {
        let entries = [
            ("hash-a", "hash-b", "Model A"),
            ("hash-b", "other-id", "Model B"),
        ];
        let configured = models(&entries);
        assert_eq!(resolve(&configured, "hash-b").unwrap(), "hash-b");
    }

    #[test]
    fn a_display_name_without_its_suffix_still_resolves() {
        let entries = [("hash-a", "model-a", "Model A (Thinking)")];
        let configured = models(&entries);
        assert_eq!(resolve(&configured, "Model A").unwrap(), "hash-a");
    }

    #[test]
    fn an_unknown_selection_passes_through() {
        let entries = [("hash-a", "model-a", "Model A")];
        let configured = models(&entries);
        assert_eq!(resolve(&configured, "default").unwrap(), "default");
    }
}
