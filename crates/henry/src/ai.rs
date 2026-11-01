//! Which model Ghost runs on: `[ai] default_model` when set, otherwise the
//! first provider with a key in Hush, Meta first, at its default model.

use anyhow::{Context, Result, bail};
use hush::{HOST, Hush};
use lease::Provider;

use crate::home::Config;

/// The model to plan with, or `None` when no provider has a key.
pub fn planner(config: &Config, hush: &Hush) -> Result<Option<ghost::Model>> {
    let connect = |provider: Provider, model: &str| -> Result<Option<ghost::Model>> {
        let Some(key) = hush.get(HOST, ghost::key_name(provider))? else { return Ok(None) };
        let base = config.ai.endpoints.get(provider.as_str()).map(String::as_str);
        Ok(Some(ghost::Model::connect(provider, model, key, base)?))
    };
    if let Some(chosen) = &config.ai.default_model {
        let (provider, model) = parse(chosen)?;
        return match connect(provider, model)? {
            Some(m) => Ok(Some(m)),
            None => bail!("[ai] default_model is {chosen}, but henry/{} is not set", ghost::key_name(provider)),
        };
    }
    for provider in Provider::ALL {
        if let Some(m) = connect(provider, ghost::defaults(provider).1)? {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

/// `provider/model`, as leases and henry.toml write it.
pub fn parse(text: &str) -> Result<(Provider, &str)> {
    let (provider, model) = text.split_once('/').context("write the model as provider/model")?;
    let provider = Provider::parse(provider)
        .with_context(|| format!("`{provider}` is not a provider: meta, gemini, claude or openai"))?;
    if model.is_empty() {
        bail!("`{text}` names no model");
    }
    Ok((provider, model))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn hush() -> Hush {
        Hush::new(Arc::new(canon::Canon::open_in_memory().unwrap()), &hush::Key::generate().unwrap())
    }

    #[test]
    fn the_first_provider_with_a_key_is_used_meta_first() {
        let h = hush();
        let config = Config::default();
        assert!(planner(&config, &h).unwrap().is_none(), "no keys, no planner");
        h.set(HOST, "ANTHROPIC_API_KEY", &hush::Secret::new("k")).unwrap();
        assert_eq!(planner(&config, &h).unwrap().unwrap().name(), "claude/claude-sonnet-5-5");
        h.set(HOST, "META_API_KEY", &hush::Secret::new("k")).unwrap();
        assert_eq!(planner(&config, &h).unwrap().unwrap().name(), "meta/muse-spark-1.3");
    }

    #[test]
    fn a_chosen_model_needs_its_key() {
        let h = hush();
        let mut config = Config::default();
        config.ai.default_model = Some("openai/gpt-x".into());
        let err = planner(&config, &h).unwrap_err().to_string();
        assert!(err.contains("henry/OPENAI_API_KEY"), "{err}");
        h.set(HOST, "OPENAI_API_KEY", &hush::Secret::new("k")).unwrap();
        assert_eq!(planner(&config, &h).unwrap().unwrap().name(), "openai/gpt-x");
        assert!(parse("llama/x").is_err());
        assert!(parse("meta/").is_err());
    }
}
