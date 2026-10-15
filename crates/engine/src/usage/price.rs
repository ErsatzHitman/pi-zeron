//! What a request cost when Pi recorded $0: a provider's own price table, a
//! custom model's `cost` in `models.json`, the catalog of every model Pi can
//! call that its usage ledger writes, or what the same model cost where Pi
//! did record it.
use serde_json::Value;
use std::{collections::HashMap, path::Path};
use zeron_proto::usage::UsageTokens;

/// USD per million tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Rates {
    fn from_json(value: &Value) -> Option<Self> {
        let rate = |field: &str| value[field].as_f64().filter(|n| n.is_finite() && *n >= 0.0);
        if !value.is_object() || rate("input").is_none() && rate("output").is_none() {
            return None;
        }
        Some(Self {
            input: rate("input").unwrap_or(0.0),
            output: rate("output").unwrap_or(0.0),
            cache_read: rate("cacheRead").unwrap_or(0.0),
            cache_write: rate("cacheWrite").unwrap_or(0.0),
        })
    }

    fn free(&self) -> bool {
        self.input == 0.0 && self.output == 0.0 && self.cache_read == 0.0 && self.cache_write == 0.0
    }

    fn cost(&self, tokens: &UsageTokens) -> f64 {
        (tokens.input as f64 * self.input
            + tokens.output as f64 * self.output
            + tokens.cache_read as f64 * self.cache_read
            + tokens.cache_write as f64 * self.cache_write)
            / 1e6
    }
}

/// How a request's cost was found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Price {
    Recorded(f64),
    Estimated(f64),
    Free,
    Unpriced,
}

/// Tokens weighted roughly as providers bill them, to carry a recorded
/// cost over to an unrecorded request of the same model.
fn weighted(tokens: &UsageTokens) -> f64 {
    (tokens.input + tokens.output + tokens.cache_write) as f64 + tokens.cache_read as f64 / 10.0
}

/// A model id without its route (`deepseek/deepseek-v4.1-flash` →
/// `deepseek-v4.1-flash`).
fn bare(model: &str) -> String {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase()
}

fn free_name(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.ends_with("-free") || model.ends_with(":free") || model.starts_with("cline-free/")
}

#[derive(Debug, Clone, Default)]
pub(super) struct PriceBook {
    /// `(provider, model)` → rates from the agent dir.
    tables: HashMap<(String, String), Rates>,
    /// `(provider, bare model)` → every route's model and rates, by name.
    routes: HashMap<(String, String), Vec<(String, Rates)>>,
    /// `(provider, model)` and bare model → USD per weighted token, learned
    /// from recorded requests.
    learned: HashMap<(String, String), f64>,
    learned_bare: HashMap<String, f64>,
}

impl PriceBook {
    /// Every `<provider>-prices.json` table (pi-clinepass writes one) and
    /// every `models.json` model with a `cost`, in Pi's agent directory, then
    /// the ledger's `catalog.json`: every model Pi's registry had, extension
    /// and `/login` providers included.
    pub fn load(agent_dir: &Path, ledger_dir: &Path) -> Self {
        let mut book = Self::default();
        let read = |path: &Path| -> Option<Value> {
            serde_json::from_slice(&std::fs::read(path).ok()?).ok()
        };
        if let Ok(entries) = std::fs::read_dir(agent_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(provider) = name.strip_suffix("-prices.json") else {
                    continue;
                };
                let Some(table) = read(&entry.path()) else {
                    continue;
                };
                for (model, rates) in table["models"].as_object().into_iter().flatten() {
                    if let Some(rates) = Rates::from_json(rates) {
                        book.tables
                            .insert((provider.to_owned(), model.clone()), rates);
                    }
                }
            }
        }
        for path in [
            agent_dir.join("models.json"),
            ledger_dir.join("catalog.json"),
        ] {
            let Some(models) = read(&path) else {
                continue;
            };
            for (provider, config) in models["providers"].as_object().into_iter().flatten() {
                for model in config["models"].as_array().into_iter().flatten() {
                    if let (Some(id), Some(rates)) =
                        (model["id"].as_str(), Rates::from_json(&model["cost"]))
                    {
                        book.tables
                            .entry((provider.clone(), id.to_owned()))
                            .or_insert(rates);
                    }
                }
            }
        }
        for ((provider, model), rates) in &book.tables {
            book.routes
                .entry((provider.clone(), bare(model)))
                .or_default()
                .push((model.clone(), *rates));
        }
        for routes in book.routes.values_mut() {
            routes.sort_by(|a, b| a.0.cmp(&b.0));
        }
        book
    }

    /// Learn each model's going rate from the requests Pi recorded a cost for.
    pub fn learn<'a>(
        &mut self,
        recorded: impl Iterator<Item = (&'a str, &'a str, UsageTokens, f64)>,
    ) {
        let mut exact: HashMap<(String, String), (f64, f64)> = HashMap::new();
        let mut by_bare: HashMap<String, (f64, f64)> = HashMap::new();
        for (provider, model, tokens, cost) in recorded {
            let weight = weighted(&tokens);
            if cost <= 0.0 || weight <= 0.0 {
                continue;
            }
            for sum in [
                exact
                    .entry((provider.to_owned(), model.to_owned()))
                    .or_default(),
                by_bare.entry(bare(model)).or_default(),
            ] {
                sum.0 += cost;
                sum.1 += weight;
            }
        }
        self.learned = exact.into_iter().map(|(k, (c, w))| (k, c / w)).collect();
        self.learned_bare = by_bare.into_iter().map(|(k, (c, w))| (k, c / w)).collect();
    }

    fn table(&self, provider: &str, model: &str) -> Option<Rates> {
        if let Some(rates) = self.tables.get(&(provider.to_owned(), model.to_owned())) {
            return Some(*rates);
        }
        // The same model under another route of this provider; a paid route
        // over a free one, since the request was not on a free model.
        let routes = self.routes.get(&(provider.to_owned(), bare(model)))?;
        routes
            .iter()
            .find(|(_, rates)| !rates.free())
            .or(routes.first())
            .map(|(_, rates)| *rates)
    }

    pub fn price(&self, provider: &str, model: &str, tokens: &UsageTokens, recorded: f64) -> Price {
        if recorded > 0.0 {
            return Price::Recorded(recorded);
        }
        if let Some(rates) = self
            .tables
            .get(&(provider.to_owned(), model.to_owned()))
            .copied()
        {
            return if rates.free() {
                Price::Free
            } else {
                Price::Estimated(rates.cost(tokens))
            };
        }
        if free_name(model) {
            return Price::Free;
        }
        if let Some(rates) = self.table(provider, model) {
            return if rates.free() {
                Price::Free
            } else {
                Price::Estimated(rates.cost(tokens))
            };
        }
        let rate = self
            .learned
            .get(&(provider.to_owned(), model.to_owned()))
            .or_else(|| self.learned_bare.get(&bare(model)));
        match rate {
            Some(rate) => Price::Estimated(rate * weighted(tokens)),
            None => Price::Unpriced,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tokens(input: u64, output: u64) -> UsageTokens {
        UsageTokens {
            input,
            output,
            ..Default::default()
        }
    }

    fn book() -> (tempfile::TempDir, PriceBook) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("clinepass-prices.json"),
            json!({"version":1,"models":{
                "cline-pass/deepseek-v4.1-flash":{"input":0.3,"output":1.2,"cacheRead":0.006},
                "cline-free/deepseek-v4.1-flash":{"input":0,"output":0,"cacheRead":0},
                "cline-pass/glm-5.3":{"input":1.4,"output":4.4,"cacheRead":0.26}
            }})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            json!({"providers":{
                "runinfra":{"baseUrl":"x","models":[
                    {"id":"deepseek-ai/DeepSeek-V4.1-Flash","cost":{"input":0.2,"output":0.8,"cacheRead":0,"cacheWrite":0}},
                    {"id":"no-cost-model"}
                ]},
                "clinepass":{"models":[{"id":"cline-pass/glm-5.3","cost":{"input":9,"output":9}}]}
            }})
            .to_string(),
        )
        .unwrap();
        let book = PriceBook::load(dir.path(), &dir.path().join("usage-ledger"));
        (dir, book)
    }

    #[test]
    fn the_ledger_catalog_prices_providers_nothing_else_knows_and_yields_to_models_json() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("usage-ledger");
        std::fs::create_dir_all(&ledger).unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            json!({"providers":{"runinfra":{"models":[
                {"id":"m","cost":{"input":0.2,"output":0.8}}
            ]}}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            ledger.join("catalog.json"),
            json!({"v":1,"providers":{
                "freeflow":{"models":[{"id":"big-pickle","cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}]},
                "new-login":{"models":[{"id":"x-1","cost":{"input":2,"output":4,"cacheRead":0,"cacheWrite":0}}]},
                "runinfra":{"models":[{"id":"m","cost":{"input":9,"output":9,"cacheRead":0,"cacheWrite":0}}]}
            }})
            .to_string(),
        )
        .unwrap();
        let book = PriceBook::load(dir.path(), &ledger);
        assert_eq!(
            book.price("freeflow", "big-pickle", &tokens(5, 5), 0.0),
            Price::Free
        );
        assert_eq!(
            book.price("new-login", "x-1", &tokens(1_000_000, 1_000_000), 0.0),
            Price::Estimated(6.0)
        );
        assert_eq!(
            book.price("runinfra", "m", &tokens(1_000_000, 0), 0.0),
            Price::Estimated(0.2)
        );
        // Without a catalog the new provider is unpriced, as before.
        let bare = PriceBook::load(dir.path(), &dir.path().join("elsewhere"));
        assert_eq!(
            bare.price("new-login", "x-1", &tokens(5, 5), 0.0),
            Price::Unpriced
        );
    }

    #[test]
    fn a_recorded_cost_is_trusted() {
        let (_dir, book) = book();
        assert_eq!(
            book.price("anthropic", "claude-opus-5-5", &tokens(10, 10), 0.25),
            Price::Recorded(0.25)
        );
    }

    #[test]
    fn a_zero_cost_is_priced_from_the_providers_own_table_before_models_json() {
        let (_dir, book) = book();
        assert_eq!(
            book.price(
                "clinepass",
                "cline-pass/glm-5.3",
                &tokens(1_000_000, 1_000_000),
                0.0
            ),
            Price::Estimated(5.8)
        );
        // Another route to the same model takes the paid rate.
        let Price::Estimated(cost) = book.price(
            "clinepass",
            "deepseek/deepseek-v4.1-flash",
            &tokens(1_000_000, 0),
            0.0,
        ) else {
            panic!("not estimated");
        };
        assert!((cost - 0.3).abs() < 1e-9, "{cost}");
        assert_eq!(
            book.price(
                "clinepass",
                "cline-free/deepseek-v4.1-flash",
                &tokens(5, 5),
                0.0
            ),
            Price::Free
        );
        assert_eq!(
            book.price(
                "runinfra",
                "deepseek-ai/DeepSeek-V4.1-Flash",
                &tokens(1_000_000, 0),
                0.0
            ),
            Price::Estimated(0.2)
        );
    }

    #[test]
    fn free_models_are_free_and_unknown_ones_unpriced_until_a_recorded_cost_teaches_a_rate() {
        let (_dir, mut book) = book();
        assert_eq!(
            book.price("opencode", "deepseek-v4-flash-free", &tokens(5, 5), 0.0),
            Price::Free
        );
        assert_eq!(
            book.price("runinfra", "no-cost-model", &tokens(5, 5), 0.0),
            Price::Unpriced
        );
        assert_eq!(
            book.price("acme-llm", "x-1", &tokens(5, 5), 0.0),
            Price::Unpriced
        );
        let recorded = [
            ("cursor", "composer-2.5", tokens(1_000, 1_000), 0.02),
            ("opencode-go", "deepseek-v4-flash", tokens(2_000, 0), 0.002),
        ];
        book.learn(recorded.iter().map(|(p, m, t, c)| (*p, *m, *t, *c)));
        let Price::Estimated(cost) = book.price("cursor", "composer-2.5", &tokens(500, 500), 0.0)
        else {
            panic!("not estimated");
        };
        assert!((cost - 0.01).abs() < 1e-9, "{cost}");
        // The same model on another provider.
        let Price::Estimated(cost) =
            book.price("opencode", "deepseek-v4-flash", &tokens(1_000, 0), 0.0)
        else {
            panic!("not estimated");
        };
        assert!((cost - 0.001).abs() < 1e-9, "{cost}");
    }
}
