//! Autonomy knobs (ARCHITECTURE §18): how much of an item's lifecycle runs
//! without the operator, and the ceiling that bounds every item.
//!
//! Each knob's values are ordered from least to most autonomous. A ceiling
//! is an [`Autonomy`] naming the most autonomous value allowed per knob.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::config::StructuralLimit;
use crate::error::ArchonError;
use crate::id::DirectiveId;

/// One autonomy knob: its values, least autonomous first.
pub trait Knob: Copy + Eq + std::fmt::Debug + 'static {
    const NAME: &'static str;
    const ORDER: &'static [Self];

    fn as_str(self) -> &'static str;

    fn rank(self) -> usize {
        Self::ORDER
            .iter()
            .position(|v| *v == self)
            .expect("ORDER lists every value")
    }

    /// The less autonomous of `self` and `other`.
    fn lower(self, other: Self) -> Self {
        if self.rank() <= other.rank() {
            self
        } else {
            other
        }
    }

    fn parse(text: &str) -> Result<Self, ArchonError> {
        Self::ORDER
            .iter()
            .copied()
            .find(|v| v.as_str() == text)
            .ok_or_else(|| {
                ArchonError::Invalid(format!(
                    "{} {text:?} is not one of {}",
                    Self::NAME,
                    names(Self::ORDER).join(", ")
                ))
            })
    }
}

fn names<K: Knob>(values: &[K]) -> Vec<&'static str> {
    values.iter().map(|v| v.as_str()).collect()
}

/// The values allowed under `max`, least autonomous first.
fn allowed_up_to<K: Knob>(max: K) -> Vec<&'static str> {
    names(&K::ORDER[..=max.rank()])
}

macro_rules! knob {
    ($(#[$doc:meta])* $name:ident, $label:literal, [$($variant:ident = $text:literal),+ $(,)?]) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "kebab-case")]
        pub enum $name {
            $($variant),+
        }

        impl Knob for $name {
            const NAME: &'static str = $label;
            const ORDER: &'static [Self] = &[$(Self::$variant),+];

            fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = ArchonError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                <Self as Knob>::parse(s)
            }
        }
    };
}

knob!(
    /// Which review family a shipping item runs through.
    ReviewLevel, "review", [Strict = "strict", Standard = "standard", None = "none"]
);
knob!(
    /// Whether a proposal is written and approved before implementing.
    ProposalMode, "proposal", [Lavish = "lavish", Off = "off"]
);
knob!(
    /// Whether a change report is written before shipping, and whether the
    /// operator must approve it first.
    ReportMode, "report", [BeforePr = "before-pr", WithPr = "with-pr", Off = "off"]
);
knob!(
    /// Whether the pull request merges by itself once its checks allow.
    MergeMode, "merge", [Manual = "manual", Auto = "auto"]
);
knob!(
    /// How much of Aether's tool surface Archon's agents get.
    ToolsMode, "tools", [Guarded = "guarded", Yolo = "yolo"]
);

/// Every knob set. The default is standard review, no proposal or report,
/// manual merge and guarded tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Autonomy {
    pub review: ReviewLevel,
    pub proposal: ProposalMode,
    pub report: ReportMode,
    pub merge: MergeMode,
    pub tools: ToolsMode,
}

impl Default for Autonomy {
    fn default() -> Self {
        Self {
            review: ReviewLevel::Standard,
            proposal: ProposalMode::Off,
            report: ReportMode::Off,
            merge: MergeMode::Manual,
            tools: ToolsMode::Guarded,
        }
    }
}

/// One knob of an item above its ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excess {
    pub knob: &'static str,
    pub value: &'static str,
    pub allowed: Vec<&'static str>,
}

impl std::fmt::Display for Excess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}={} (allowed: {})",
            self.knob,
            self.value,
            self.allowed.join(", ")
        )
    }
}

fn excess_of<K: Knob>(value: K, max: K) -> Option<Excess> {
    (value.rank() > max.rank()).then(|| Excess {
        knob: K::NAME,
        value: value.as_str(),
        allowed: allowed_up_to(max),
    })
}

impl Autonomy {
    /// The ceiling when configuration sets none: everything except
    /// `merge = auto`.
    pub fn default_ceiling() -> Self {
        Self {
            review: ReviewLevel::None,
            proposal: ProposalMode::Off,
            report: ReportMode::Off,
            merge: MergeMode::Manual,
            tools: ToolsMode::Yolo,
        }
    }

    /// Per knob, the less autonomous of `self` and `other`.
    pub fn narrowed(self, other: Self) -> Self {
        Self {
            review: self.review.lower(other.review),
            proposal: self.proposal.lower(other.proposal),
            report: self.report.lower(other.report),
            merge: self.merge.lower(other.merge),
            tools: self.tools.lower(other.tools),
        }
    }

    /// Every knob of `self` above `ceiling`, in knob order.
    pub fn excess(self, ceiling: Self) -> Vec<Excess> {
        let out: Vec<Excess> = [
            excess_of(self.review, ceiling.review),
            excess_of(self.proposal, ceiling.proposal),
            excess_of(self.report, ceiling.report),
            excess_of(self.merge, ceiling.merge),
            excess_of(self.tools, ceiling.tools),
        ]
        .into_iter()
        .flatten()
        .collect();
        debug_assert_eq!(out.is_empty(), self.narrowed(ceiling) == self);
        out
    }

    /// `review=standard, proposal=off, …`, for messages and listings.
    pub fn describe(self) -> String {
        AutonomyPatch::from(self).describe()
    }

    /// A ceiling as the values it allows, `review=strict|standard|none, …`.
    /// Its own `proposal=off` would read as "no proposals" when it allows both.
    pub fn describe_allowed(self) -> String {
        let pairs: Vec<String> = self
            .allowed()
            .iter()
            .map(|(knob, allowed)| format!("{knob}={allowed}"))
            .collect();
        pairs.join(", ")
    }

    /// Each knob of a ceiling with the values it allows, `strict|standard|none`.
    pub fn allowed(self) -> [(&'static str, String); 5] {
        let knobs = [
            (ReviewLevel::NAME, allowed_up_to(self.review)),
            (ProposalMode::NAME, allowed_up_to(self.proposal)),
            (ReportMode::NAME, allowed_up_to(self.report)),
            (MergeMode::NAME, allowed_up_to(self.merge)),
            (ToolsMode::NAME, allowed_up_to(self.tools)),
        ];
        assert!(knobs.iter().all(|(_, allowed)| !allowed.is_empty()));
        knobs.map(|(knob, allowed)| (knob, allowed.join("|")))
    }
}

/// Some knobs; unset ones come from elsewhere. Used for configured
/// defaults and ceilings, a repository's registry ceiling, and enqueues.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutonomyPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProposalMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<ReportMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsMode>,
}

impl From<Autonomy> for AutonomyPatch {
    fn from(a: Autonomy) -> Self {
        Self {
            review: Some(a.review),
            proposal: Some(a.proposal),
            report: Some(a.report),
            merge: Some(a.merge),
            tools: Some(a.tools),
        }
    }
}

impl AutonomyPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `base` with every set knob replaced.
    pub fn over(&self, base: Autonomy) -> Autonomy {
        Autonomy {
            review: self.review.unwrap_or(base.review),
            proposal: self.proposal.unwrap_or(base.proposal),
            report: self.report.unwrap_or(base.report),
            merge: self.merge.unwrap_or(base.merge),
            tools: self.tools.unwrap_or(base.tools),
        }
    }

    /// `base` with every set knob lowered to at most its value; never raises.
    pub fn lower(&self, base: Autonomy) -> Autonomy {
        let lowered = base.narrowed(self.over(base));
        debug_assert!(lowered.excess(base).is_empty(), "lowering never raises");
        lowered
    }

    /// Set knobs above `ceiling`.
    pub fn excess(&self, ceiling: Autonomy) -> Vec<Excess> {
        let excess = self.over(ceiling).excess(ceiling);
        debug_assert!(excess.len() <= self.set_count());
        excess
    }

    fn set_count(&self) -> usize {
        [
            self.review.is_some(),
            self.proposal.is_some(),
            self.report.is_some(),
            self.merge.is_some(),
            self.tools.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count()
    }

    /// `review=none,merge=auto`, as `archon repo set --autonomy-max` takes
    /// it; empty text is the empty patch.
    pub fn parse_list(text: &str) -> Result<Self, ArchonError> {
        let mut patch = Self::default();
        for pair in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (knob, value) = pair.split_once('=').ok_or_else(|| {
                ArchonError::Invalid(format!("{pair:?} is not knob=value (e.g. merge=auto)"))
            })?;
            patch.set(knob.trim(), value.trim())?;
        }
        Ok(patch)
    }

    fn set(&mut self, knob: &str, value: &str) -> Result<(), ArchonError> {
        match knob {
            "review" => self.review = Some(ReviewLevel::parse(value)?),
            "proposal" => self.proposal = Some(ProposalMode::parse(value)?),
            "report" => self.report = Some(ReportMode::parse(value)?),
            "merge" => self.merge = Some(MergeMode::parse(value)?),
            "tools" => self.tools = Some(ToolsMode::parse(value)?),
            other => {
                return Err(ArchonError::Invalid(format!(
                    "unknown autonomy knob {other:?}; knobs: review, proposal, report, merge, tools"
                )));
            }
        }
        Ok(())
    }

    /// The set knobs as `review=none, merge=auto`; `-` when none is set.
    pub fn describe(&self) -> String {
        let pairs: Vec<String> = [
            self.review.map(|v| format!("review={v}")),
            self.proposal.map(|v| format!("proposal={v}")),
            self.report.map(|v| format!("report={v}")),
            self.merge.map(|v| format!("merge={v}")),
            self.tools.map(|v| format!("tools={v}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if pairs.is_empty() {
            return "-".into();
        }
        pairs.join(", ")
    }
}

/// `[autonomy]` in either configuration file: `default` knobs for items
/// that name none, and the `max` ceiling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutonomyConfig {
    pub default: AutonomyPatch,
    pub max: AutonomyPatch,
}

impl AutonomyConfig {
    /// Global configuration: an explicit default above the ceiling is a
    /// mistake worth refusing, not clamping.
    pub fn validate(&self) -> Result<(), ArchonError> {
        let ceiling = self.max.over(Autonomy::default_ceiling());
        let excess = self.default.excess(ceiling);
        if excess.is_empty() {
            return Ok(());
        }
        let listed: Vec<String> = excess.iter().map(ToString::to_string).collect();
        Err(ArchonError::Config(format!(
            "autonomy.default exceeds autonomy.max: {}",
            listed.join("; ")
        )))
    }
}

/// The knobs an item starts from and the ceiling it must stay within, for
/// one repository (or none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomyPolicy {
    pub default: Autonomy,
    pub ceiling: Autonomy,
}

impl AutonomyPolicy {
    /// Global `[autonomy]`, then the registry's per-repository ceiling
    /// (operator-set, so it replaces the global one per knob), then the
    /// repository's own file, which may only lower either.
    pub fn resolve(
        global: &AutonomyConfig,
        registry_max: Option<&AutonomyPatch>,
        repo: Option<&AutonomyConfig>,
    ) -> Self {
        let mut ceiling = global.max.over(Autonomy::default_ceiling());
        if let Some(registry) = registry_max {
            ceiling = registry.over(ceiling);
        }
        let mut default = global.default.over(Autonomy::default());
        if let Some(repo) = repo {
            ceiling = repo.max.lower(ceiling);
            default = repo.default.lower(default);
        }
        let policy = Self {
            default: default.narrowed(ceiling),
            ceiling,
        };
        assert!(policy.default.excess(policy.ceiling).is_empty());
        policy
    }

    /// Refuse `autonomy` knobs the ceiling does not allow, naming what is allowed.
    /// `applies` filters knobs that mean nothing for the item's pipeline.
    pub fn check(
        &self,
        autonomy: Autonomy,
        applies: impl Fn(&str) -> bool,
        scope: &str,
    ) -> Result<(), ArchonError> {
        let excess: Vec<String> = autonomy
            .excess(self.ceiling)
            .iter()
            .filter(|e| applies(e.knob))
            .map(ToString::to_string)
            .collect();
        if excess.is_empty() {
            return Ok(());
        }
        Err(ArchonError::Forbidden(format!(
            "autonomy for {scope} asks for more than its ceiling allows: {}. The ceiling lists the \
             values each knob may take; choose one of them, or have the operator raise the ceiling \
             (`[autonomy] max` in config.toml or `archon repo set --autonomy-max`)",
            excess.join("; ")
        )))
    }
}

/// Longest directive text, so every trigger payload stays small.
pub const MAX_DIRECTIVE_CHARS: usize = 500;
/// Most directives in force at once.
pub const MAX_DIRECTIVES: usize = 20;

/// Said whenever a directive is recorded, so one asking for more than the
/// ceiling allows is not mistaken for permission.
pub const DIRECTIVE_CEILING_NOTE: &str =
    "The autonomy ceiling still applies: a directive steers choices within it and never raises it.";

/// Who recorded a directive: the operator from the CLI, or the orchestrator
/// relaying what the operator said in chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveSource {
    Operator,
    Orchestrator,
}

/// A standing operator instruction ("fully autonomous today") that steers
/// the orchestrator's knob choices within the ceiling; it never raises it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directive {
    pub id: DirectiveId,
    pub text: String,
    pub source: DirectiveSource,
    pub set_at: Timestamp,
    pub expires_at: Option<Timestamp>,
}

impl Directive {
    pub fn is_active(&self, now: Timestamp) -> bool {
        self.expires_at.is_none_or(|at| at > now)
    }

    /// Refuse empty or oversized text; `expires_in_hours` bounds a day-long
    /// instruction to a month at most.
    pub fn new(
        text: &str,
        source: DirectiveSource,
        expires_in_hours: Option<u32>,
        now: Timestamp,
    ) -> Result<Self, ArchonError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ArchonError::Invalid("a directive needs text".into()));
        }
        if text.chars().count() > MAX_DIRECTIVE_CHARS {
            return Err(ArchonError::Invalid(format!(
                "a directive is at most {MAX_DIRECTIVE_CHARS} characters"
            )));
        }
        let expires_at = match expires_in_hours {
            None => None,
            Some(hours @ 1..=720) => Some(now + chrono::Duration::hours(i64::from(hours))),
            Some(hours) => {
                return Err(ArchonError::Invalid(format!(
                    "expires_in_hours {hours} must be between 1 and 720"
                )));
            }
        };
        Ok(Self {
            id: DirectiveId::new(),
            text: text.to_owned(),
            source,
            set_at: now,
            expires_at,
        })
    }
}

/// What `archon autonomy` and the `autonomy_policy` tool show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomyView {
    /// The repository the policy was resolved for; `None` is global.
    pub repo: Option<String>,
    pub policy: AutonomyPolicy,
    pub directives: Vec<Directive>,
    /// The repositories whose `grant_merge` is on: the one asked about, or
    /// every one for the global view (ARCHITECTURE §18.6).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge_granted: Vec<String>,
    /// Every structural limit in force; empty from an archond that
    /// predates them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<StructuralLimit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knobs_order_from_least_to_most_autonomous() {
        assert!(ReviewLevel::None.rank() > ReviewLevel::Standard.rank());
        assert!(ReviewLevel::Standard.rank() > ReviewLevel::Strict.rank());
        assert!(ReportMode::Off.rank() > ReportMode::WithPr.rank());
        assert!(ReportMode::WithPr.rank() > ReportMode::BeforePr.rank());
        assert!(ProposalMode::Off.rank() > ProposalMode::Lavish.rank());
        assert!(MergeMode::Auto.rank() > MergeMode::Manual.rank());
        assert!(ToolsMode::Yolo.rank() > ToolsMode::Guarded.rank());
        assert_eq!(
            ReportMode::WithPr.lower(ReportMode::Off),
            ReportMode::WithPr
        );
    }

    #[test]
    fn wire_names_are_kebab_case_and_parse_back() {
        assert_eq!(
            serde_json::to_string(&ReportMode::BeforePr).unwrap(),
            "\"before-pr\""
        );
        assert_eq!("with-pr".parse::<ReportMode>().unwrap(), ReportMode::WithPr);
        let err = "sometimes".parse::<MergeMode>().unwrap_err().to_string();
        assert!(err.contains("manual, auto"), "{err}");
    }

    #[test]
    fn the_default_ceiling_allows_everything_but_auto_merge() {
        let ceiling = Autonomy::default_ceiling();
        let everything = Autonomy {
            review: ReviewLevel::None,
            proposal: ProposalMode::Off,
            report: ReportMode::Off,
            merge: MergeMode::Auto,
            tools: ToolsMode::Yolo,
        };
        let excess = everything.excess(ceiling);
        assert_eq!(excess.len(), 1);
        assert_eq!(excess[0].to_string(), "merge=auto (allowed: manual)");
        assert!(Autonomy::default().excess(ceiling).is_empty());
        assert_eq!(
            ceiling.describe_allowed(),
            "review=strict|standard|none, proposal=lavish|off, report=before-pr|with-pr|off, \
             merge=manual, tools=guarded|yolo"
        );
    }

    #[test]
    fn a_refusal_says_what_the_ceiling_allows_and_how_to_raise_it() {
        let policy = AutonomyPolicy::resolve(&AutonomyConfig::default(), None, None);
        let wanted = Autonomy {
            merge: MergeMode::Auto,
            ..Autonomy::default()
        };
        let text = policy
            .check(wanted, |_| true, "repo web")
            .unwrap_err()
            .to_string();
        assert!(
            text.contains("asks for more than its ceiling allows"),
            "{text}"
        );
        assert!(text.contains("values each knob may take"), "{text}");
        assert!(text.contains("archon repo set --autonomy-max"), "{text}");
        assert!(!text.contains("above the ceiling"), "{text}");
    }

    #[test]
    fn a_patch_parses_from_a_list_and_describes_itself() {
        let patch = AutonomyPatch::parse_list("review=none, merge=auto").unwrap();
        assert_eq!(patch.review, Some(ReviewLevel::None));
        assert_eq!(patch.merge, Some(MergeMode::Auto));
        assert_eq!(patch.describe(), "review=none, merge=auto");
        assert!(AutonomyPatch::parse_list("").unwrap().is_empty());
        assert!(AutonomyPatch::parse_list("speed=max").is_err());
        assert!(AutonomyPatch::parse_list("merge").is_err());
    }

    #[test]
    fn the_registry_replaces_the_global_ceiling_and_the_repo_only_lowers() {
        let global = AutonomyConfig::default();
        let registry = AutonomyPatch {
            merge: Some(MergeMode::Auto),
            ..AutonomyPatch::default()
        };
        let repo = AutonomyConfig {
            default: AutonomyPatch {
                tools: Some(ToolsMode::Yolo),
                review: Some(ReviewLevel::Strict),
                ..AutonomyPatch::default()
            },
            max: AutonomyPatch {
                tools: Some(ToolsMode::Guarded),
                merge: Some(MergeMode::Auto),
                ..AutonomyPatch::default()
            },
        };
        let policy = AutonomyPolicy::resolve(&global, Some(&registry), Some(&repo));
        assert_eq!(policy.ceiling.merge, MergeMode::Auto);
        assert_eq!(policy.ceiling.tools, ToolsMode::Guarded);
        assert_eq!(
            policy.default.tools,
            ToolsMode::Guarded,
            "repo cannot raise"
        );
        assert_eq!(policy.default.review, ReviewLevel::Strict);
        let alone = AutonomyPolicy::resolve(&global, None, Some(&repo));
        assert_eq!(
            alone.ceiling.merge,
            MergeMode::Manual,
            "repo file cannot raise"
        );
    }

    #[test]
    fn check_names_what_is_allowed_and_skips_knobs_that_do_not_apply() {
        let policy = AutonomyPolicy::resolve(&AutonomyConfig::default(), None, None);
        let wanted = Autonomy {
            merge: MergeMode::Auto,
            ..Autonomy::default()
        };
        let err = policy.check(wanted, |_| true, "repo web").unwrap_err();
        assert!(matches!(err, ArchonError::Forbidden(_)));
        assert!(err.to_string().contains("merge=auto (allowed: manual)"));
        policy
            .check(wanted, |knob| knob != "merge", "repo web")
            .unwrap();
    }

    #[test]
    fn directives_are_bounded_and_expire() {
        let now = crate::now();
        let today = Directive::new(
            " autonomous today ",
            DirectiveSource::Operator,
            Some(8),
            now,
        )
        .unwrap();
        assert_eq!(today.text, "autonomous today");
        assert!(today.is_active(now));
        assert!(!today.is_active(now + chrono::Duration::hours(9)));
        assert!(Directive::new("", DirectiveSource::Operator, None, now).is_err());
        assert!(Directive::new("x", DirectiveSource::Operator, Some(0), now).is_err());
        let long = "x".repeat(MAX_DIRECTIVE_CHARS + 1);
        assert!(Directive::new(&long, DirectiveSource::Orchestrator, None, now).is_err());
    }

    #[test]
    fn an_explicit_default_above_the_ceiling_is_a_config_error() {
        let config = AutonomyConfig {
            default: AutonomyPatch {
                merge: Some(MergeMode::Auto),
                ..AutonomyPatch::default()
            },
            max: AutonomyPatch::default(),
        };
        assert!(matches!(config.validate(), Err(ArchonError::Config(_))));
        let allowed = AutonomyConfig {
            max: AutonomyPatch {
                merge: Some(MergeMode::Auto),
                ..AutonomyPatch::default()
            },
            ..config
        };
        allowed.validate().unwrap();
    }
}
