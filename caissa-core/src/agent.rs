use serde::Deserialize;
use std::path::Path;

// ── Fondament definition structs ─────────────────────────────────────────────

/// A skill reference — either a plain string or a versioned object.
/// Matches the SkillRef schema in fondament-core.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum SkillRef {
    Simple(String),
    Versioned { id: String, version: String },
}

impl SkillRef {
    pub fn id(&self) -> &str {
        match self {
            Self::Simple(s) => s,
            Self::Versioned { id, .. } => id,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct FondamentDef {
    pub id: String,
    pub kind: String,
    pub default_model: Option<String>,
    pub context: String,
    #[serde(default)]
    pub skills: Vec<SkillRef>,
    /// Reasoning/discipline modifiers declared by the definition (e.g.
    /// "deconstructive"). Currently informational — formalizes what Caissa's
    /// listen.rs hardcodes — but available for future tooling. Non-breaking:
    /// older definitions without this field default to an empty list.
    #[serde(default)]
    pub modifiers: Vec<String>,
}

impl FondamentDef {
    /// Returns skill IDs as plain strings (for backwards-compatible callers).
    pub fn skill_ids(&self) -> Vec<String> {
        self.skills.iter().map(|s| s.id().to_string()).collect()
    }
}

#[derive(Debug, Deserialize)]
pub struct DomainDef {
    pub id: String,
    pub kind: String,
    pub repo: Option<String>,
    pub default_facet: Option<String>,
    pub context: String,
}

// ── Agent spec parsing ────────────────────────────────────────────────────────

/// Parsed form of what the user passes to `caissa spawn`.
///
/// - `"guilhem"` → generation spec: use image caissa-sandbox:guilhem, no domain injection
/// - `"farga"` → domain spec: use configured generation image, inject farga domain context
/// - `"farga/architect"` → domain + facet: inject farga domain + architect facet context
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// The Docker image tag to use: either the generation name (if bare spec)
    /// or the configured current generation (if domain/facet spec).
    pub image_tag: String,
    /// Domain name (e.g. "farga"). None when spawning the org agent directly.
    pub domain: Option<String>,
    /// Facet name (e.g. "architect"). None when no facet is specified.
    pub facet: Option<String>,
    /// Farga project to fetch live context for. Defaults to domain name if set.
    pub project: Option<String>,
}

impl AgentSpec {
    /// Parse a spawn spec like "guilhem", "farga", or "farga/architect".
    ///
    /// `current_generation` is the configured generation name from caissa.toml,
    /// used as the image tag when a domain spec is given.
    pub fn parse(spec: &str, current_generation: &str, project_override: Option<&str>) -> Self {
        if let Some((domain, facet)) = spec.split_once('/') {
            // domain/facet form — e.g. "farga/architect"
            let project = project_override
                .map(str::to_string)
                .or_else(|| Some(domain.to_string()));
            AgentSpec {
                image_tag: current_generation.to_string(),
                domain: Some(domain.to_string()),
                facet: Some(facet.to_string()),
                project,
            }
        } else if is_known_domain(spec) {
            // bare domain name — e.g. "farga" (no facet specified)
            let project = project_override
                .map(str::to_string)
                .or_else(|| Some(spec.to_string()));
            AgentSpec {
                image_tag: current_generation.to_string(),
                domain: Some(spec.to_string()),
                facet: None,
                project,
            }
        } else {
            // generation name — e.g. "guilhem"
            AgentSpec {
                image_tag: spec.to_string(),
                domain: None,
                facet: None,
                project: project_override.map(str::to_string),
            }
        }
    }
}

/// Known domain names — checked to disambiguate bare spec from generation name.
fn is_known_domain(name: &str) -> bool {
    matches!(
        name,
        "occitan" | "farga" | "gardian" | "amassada" | "charradissa" | "cor" | "caissa" | "fondament"
    )
}

// ── Definition loaders ────────────────────────────────────────────────────────

/// Load a Fondament role definition (fondament/ directory).
/// Used for generation personas and facets.
pub fn load_fondament_def(fondament_path: &str, name: &str) -> anyhow::Result<FondamentDef> {
    let path = Path::new(fondament_path)
        .join("definitions")
        .join("fondament")
        .join(format!("{}.yaml", name));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading {}: {}", path.display(), e))?;
    serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing {}: {}", path.display(), e))
}

/// Load a domain definition (domains/ directory).
pub fn load_domain_def(fondament_path: &str, domain: &str) -> anyhow::Result<DomainDef> {
    let path = Path::new(fondament_path)
        .join("definitions")
        .join("domains")
        .join(format!("{}.yaml", domain));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading {}: {}", path.display(), e))?;
    serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing {}: {}", path.display(), e))
}

/// Map a short facet name to the corresponding fondament definition filename.
///
/// Allows `farga/architect` to resolve `fondament/app-architect.yaml` without
/// requiring callers to know the exact file naming convention.
pub fn resolve_facet_name(facet: &str) -> &str {
    match facet {
        "architect" => "app-architect",
        "developer" => "developer",
        "qa" => "qa-engineer",
        "infra" => "infra-engineer",
        "db" => "data-architect",
        "security" => "security-analyst",
        "moderator" => "tech-moderator",
        other => other,
    }
}

// ── CLAUDE.md assembly ────────────────────────────────────────────────────────

/// Assemble the CLAUDE.md baked into the agent image at ~/.claude/CLAUDE.md.
/// This is the generation layer — identity across time.
pub fn assemble_image_claude_md(def: &FondamentDef) -> String {
    def.context.trim_end().to_string()
}

/// Assemble the workspace CLAUDE.md injected at spawn time into /workspace/CLAUDE.md.
/// This is the situational layer — what domain, what role, what's currently known.
///
/// Layers (all optional):
///   1. Domain context (what component this session is about)
///   2. Facet role (what lens this agent brings)
///   3. Live project context from Farga
pub fn assemble_workspace_md(
    domain: Option<&DomainDef>,
    facet: Option<&FondamentDef>,
    farga_context: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(d) = domain {
        let domain_name = d.id.strip_prefix("domain/").unwrap_or(&d.id);
        parts.push(format!(
            "# Domain: {}\n\n{}",
            capitalize(domain_name),
            d.context.trim_end()
        ));
    }

    if let Some(f) = facet {
        let facet_name = f.id.strip_prefix("fondament/").unwrap_or(&f.id);
        parts.push(format!(
            "# Role: {}\n\n{}",
            capitalize(facet_name),
            f.context.trim_end()
        ));
    }

    if let Some(ctx) = farga_context {
        if !ctx.trim().is_empty() {
            parts.push(format!("# Project context\n\n{}", ctx.trim_end()));
        }
    }

    parts.join("\n\n---\n\n")
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

// ── Legacy compat ─────────────────────────────────────────────────────────────

/// Load a Fondament definition by generation name.
/// Kept for backward compat with `caissa build`.
pub fn load_definition(fondament_path: &str, generation: &str) -> anyhow::Result<FondamentDef> {
    load_fondament_def(fondament_path, generation)
}

/// Assemble the CLAUDE.md baked into the agent image.
/// Kept for backward compat with `caissa build`.
pub fn assemble_claude_md(def: &FondamentDef) -> String {
    assemble_image_claude_md(def)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_generation_spec() {
        let spec = AgentSpec::parse("guilhem", "guilhem", None);
        assert_eq!(spec.image_tag, "guilhem");
        assert!(spec.domain.is_none());
        assert!(spec.facet.is_none());
    }

    #[test]
    fn parse_domain_only_spec() {
        let spec = AgentSpec::parse("farga", "guilhem", None);
        assert_eq!(spec.image_tag, "guilhem");
        assert_eq!(spec.domain.as_deref(), Some("farga"));
        assert!(spec.facet.is_none());
        assert_eq!(spec.project.as_deref(), Some("farga"));
    }

    #[test]
    fn parse_domain_facet_spec() {
        let spec = AgentSpec::parse("farga/architect", "guilhem", None);
        assert_eq!(spec.image_tag, "guilhem");
        assert_eq!(spec.domain.as_deref(), Some("farga"));
        assert_eq!(spec.facet.as_deref(), Some("architect"));
    }

    #[test]
    fn parse_project_override() {
        let spec = AgentSpec::parse("farga/developer", "guilhem", Some("farga-api"));
        assert_eq!(spec.project.as_deref(), Some("farga-api"));
    }

    #[test]
    fn resolve_facet_names() {
        assert_eq!(resolve_facet_name("architect"), "app-architect");
        assert_eq!(resolve_facet_name("developer"), "developer");
        assert_eq!(resolve_facet_name("qa"), "qa-engineer");
        assert_eq!(resolve_facet_name("infra"), "infra-engineer");
        assert_eq!(resolve_facet_name("custom-role"), "custom-role");
    }

    #[test]
    fn assemble_workspace_md_all_layers() {
        let domain = DomainDef {
            id: "domain/farga".into(),
            kind: "domain".into(),
            repo: Some("Farga".into()),
            default_facet: Some("architect".into()),
            context: "Farga is the memory substrate.".into(),
        };
        let facet = FondamentDef {
            id: "fondament/app-architect".into(),
            kind: "role".into(),
            default_model: None,
            context: "You are an application architect.".into(),
            skills: vec![],
            modifiers: vec![],
        };
        let md = assemble_workspace_md(Some(&domain), Some(&facet), Some("## Current state"));
        assert!(md.contains("# Domain: Farga"));
        assert!(md.contains("# Role: App-architect"));
        assert!(md.contains("# Project context"));
        assert!(md.contains("---"));
    }

    #[test]
    fn fondament_def_parses_modifiers_list() {
        let yaml = "id: fondament/guilhem\nkind: role\ncontext: |\n  You are Guilhem.\nmodifiers:\n  - deconstructive\n";
        let def: FondamentDef = serde_yaml::from_str(yaml).expect("parse def with modifiers");
        assert_eq!(def.modifiers, vec!["deconstructive".to_string()]);
    }

    #[test]
    fn fondament_def_modifiers_defaults_empty_when_absent() {
        // Non-breaking: a definition without `modifiers:` still parses.
        let yaml = "id: fondament/legacy\nkind: role\ncontext: |\n  You are a legacy agent.\n";
        let def: FondamentDef = serde_yaml::from_str(yaml).expect("parse def without modifiers");
        assert!(def.modifiers.is_empty());
    }

    #[test]
    fn assemble_image_md_trims_trailing_whitespace() {
        let def = FondamentDef {
            id: "fondament/test".into(),
            kind: "role".into(),
            default_model: None,
            context: "You are a test agent.\n\n".into(),
            skills: vec![],
            modifiers: vec![],
        };
        let md = assemble_image_claude_md(&def);
        assert!(!md.ends_with('\n'));
    }
}
