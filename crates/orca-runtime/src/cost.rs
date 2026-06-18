use orca_core::cost_types::UsageTotals;
use orca_core::provider_types::Usage;

#[derive(Clone, Debug)]
pub struct CostTracker {
    totals: UsageTotals,
    pricing: ModelPricing,
}

#[derive(Clone, Copy, Debug)]
struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
    cache_per_million: f64,
}

impl CostTracker {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            totals: UsageTotals::default(),
            pricing: ModelPricing::for_model(model),
        }
    }

    pub fn add_usage(&mut self, usage: Usage) -> UsageTotals {
        self.totals.input_tokens += usage.input_tokens;
        self.totals.output_tokens += usage.output_tokens;
        self.totals.cache_tokens += usage.cache_tokens;
        self.totals.estimated_cost_usd += self.pricing.estimate(usage);
        self.totals
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.pricing = ModelPricing::for_model(model);
    }

    pub fn merge(&mut self, other: &CostTracker) {
        self.totals.input_tokens += other.totals.input_tokens;
        self.totals.output_tokens += other.totals.output_tokens;
        self.totals.cache_tokens += other.totals.cache_tokens;
        self.totals.estimated_cost_usd += other.totals.estimated_cost_usd;
    }
}

impl ModelPricing {
    fn for_model(model: Option<&str>) -> Self {
        match model.unwrap_or("") {
            m if m.contains("v4-pro") => Self {
                input_per_million: 0.435,
                output_per_million: 0.87,
                cache_per_million: 0.044,
            },
            // V4-Flash is the default and the low-cost option when the model is omitted.
            _ => Self {
                input_per_million: 0.14,
                output_per_million: 0.28,
                cache_per_million: 0.014,
            },
        }
    }

    fn estimate(self, usage: Usage) -> f64 {
        // DeepSeek pricing: cache_tokens are a subset of input_tokens that hit cache.
        // Charge: (input - cache) at input price, cache at cache price, output at output price.
        let non_cache_input = usage.input_tokens.saturating_sub(usage.cache_tokens);
        (non_cache_input as f64 * self.input_per_million
            + usage.cache_tokens as f64 * self.cache_per_million
            + usage.output_tokens as f64 * self.output_per_million)
            / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_tokens_and_cost() {
        let mut tracker = CostTracker::new(Some("deepseek-v4-flash"));

        let totals = tracker.add_usage(Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache_tokens: 10,
        });

        assert_eq!(totals.input_tokens, 120);
        assert_eq!(totals.output_tokens, 30);
        assert_eq!(totals.cache_tokens, 10);
        // total_tokens = input + output (cache is subset of input)
        assert_eq!(totals.total_tokens(), 150);
        assert!(totals.estimated_cost_usd > 0.0);
        // V4-Flash: (120-10)*0.14 + 10*0.014 + 30*0.28 = 110*0.14 + 0.14 + 8.4 = 15.4+0.14+8.4 = 23.94 per million
        let expected = (110.0 * 0.14 + 10.0 * 0.014 + 30.0 * 0.28) / 1_000_000.0;
        assert!((totals.estimated_cost_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn merge_accumulates_from_child_tracker() {
        let mut parent = CostTracker::new(Some("deepseek-v4-flash"));
        parent.add_usage(Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_tokens: 20,
        });

        let mut child = CostTracker::new(Some("deepseek-v4-flash"));
        child.add_usage(Usage {
            input_tokens: 200,
            output_tokens: 80,
            cache_tokens: 30,
        });

        let parent_cost_before = parent.totals.estimated_cost_usd;
        let child_cost = child.totals.estimated_cost_usd;

        parent.merge(&child);

        assert_eq!(parent.totals.input_tokens, 300);
        assert_eq!(parent.totals.output_tokens, 130);
        assert_eq!(parent.totals.cache_tokens, 50);
        assert!(
            (parent.totals.estimated_cost_usd - (parent_cost_before + child_cost)).abs() < 1e-12
        );
    }
}
