use std::fs;
use std::path::{Path, PathBuf};

use orca_core::tool_types::{ToolRequest, ToolResult};
use serde::Deserialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub path: PathBuf,
    pub body: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillSource {
    User,
    Project,
}

impl SkillSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Deserialize)]
struct ReadSkillArgs {
    id: String,
}

pub fn execute_list(request: &ToolRequest, cwd: &Path) -> ToolResult {
    match discover_from_env(cwd) {
        Ok(skills) => {
            let output = format_skill_list(&skills);
            ToolResult::completed(request, output, false)
        }
        Err(error) => ToolResult::failed(request, error, None),
    }
}

pub fn execute_read(request: &ToolRequest, cwd: &Path) -> ToolResult {
    let args = match parse_read_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    match discover_from_env(cwd) {
        Ok(skills) => match skills.into_iter().find(|skill| skill.id == args.id) {
            Some(skill) => ToolResult::completed(request, format_skill(&skill), false),
            None => ToolResult::failed(request, format!("unknown skill: {}", args.id), None),
        },
        Err(error) => ToolResult::failed(request, error, None),
    }
}

pub fn discover_from_env(cwd: &Path) -> Result<Vec<Skill>, String> {
    let orca_home = std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")));
    discover(cwd, orca_home.as_deref())
}

pub fn discover(cwd: &Path, orca_home: Option<&Path>) -> Result<Vec<Skill>, String> {
    let project_root = project_root(cwd);
    let mut skills = Vec::new();
    if let Some(home) = orca_home {
        collect_skills(&home.join("skills"), SkillSource::User, None, &mut skills)?;
    }
    if let Some(project_root) = project_root {
        collect_skills(
            &project_root.join(".orca/skills"),
            SkillSource::Project,
            Some(&project_root),
            &mut skills,
        )?;
    }
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    skills.dedup_by(|left, right| left.id == right.id);
    Ok(skills)
}

fn collect_skills(
    skills_dir: &Path,
    source: SkillSource,
    allowed_root: Option<&Path>,
    skills: &mut Vec<Skill>,
) -> Result<(), String> {
    if !skills_dir.is_dir() {
        return Ok(());
    }
    let entries = fs::read_dir(skills_dir)
        .map_err(|error| format!("failed to read skills dir {}: {error}", skills_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !valid_skill_id(id) {
            continue;
        }
        let skill_path = path.join("SKILL.md");
        if !skill_path.is_file() {
            continue;
        }
        if let Some(root) = allowed_root
            && !canonical_or_self(&skill_path).starts_with(&canonical_or_self(root))
        {
            continue;
        }
        if let Ok(skill) = parse_skill(id, source, &skill_path) {
            skills.push(skill);
        }
    }
    Ok(())
}

fn parse_skill(id: &str, source: SkillSource, path: &Path) -> Result<Skill, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("failed to read skill {}: {error}", path.display()))?;
    let (frontmatter, body) = split_frontmatter(&content);
    let (name, description) = parse_frontmatter(frontmatter);
    Ok(Skill {
        id: id.to_string(),
        name: name.unwrap_or_else(|| id.to_string()),
        description: description.unwrap_or_default(),
        source,
        path: path.to_path_buf(),
        body: body.trim().to_string(),
    })
}

fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let Some(rest) = content.strip_prefix("---\n") else {
        return (None, content);
    };
    let Some(end) = rest.find("\n---\n") else {
        return (None, content);
    };
    let frontmatter = &rest[..end];
    let body = &rest[end + "\n---\n".len()..];
    (Some(frontmatter), body)
}

fn parse_frontmatter(frontmatter: Option<&str>) -> (Option<String>, Option<String>) {
    let mut name = None;
    let mut description = None;
    let Some(frontmatter) = frontmatter else {
        return (name, description);
    };
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    (name, description)
}

fn format_skill_list(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return "(no skills found)".to_string();
    }
    skills
        .iter()
        .map(|skill| {
            format!(
                "{} [{}] - {}",
                skill.id,
                skill.source.as_str(),
                if skill.description.is_empty() {
                    &skill.name
                } else {
                    &skill.description
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_skill(skill: &Skill) -> String {
    format!(
        "# {}\nsource: {}\nid: {}\npath: {}\n\n{}",
        skill.name,
        skill.source.as_str(),
        skill.id,
        skill.path.display(),
        skill.body
    )
}

fn parse_read_args(request: &ToolRequest) -> Result<ReadSkillArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "missing read_skill arguments JSON".to_string())?;
    let args: ReadSkillArgs = serde_json::from_str(raw)
        .map_err(|error| format!("invalid read_skill arguments JSON: {error}"))?;
    if args.id.trim().is_empty() {
        return Err("missing required read_skill argument: id".to_string());
    }
    Ok(args)
}

fn valid_skill_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn project_root(cwd: &Path) -> Option<PathBuf> {
    for start in [Some(cwd.to_path_buf()), cwd.canonicalize().ok()]
        .into_iter()
        .flatten()
    {
        for candidate in start.ancestors() {
            if candidate.join(".git").exists()
                || candidate.join("Cargo.toml").exists()
                || candidate.join("package.json").exists()
            {
                return Some(candidate.to_path_buf());
            }
        }
    }
    Some(cwd.to_path_buf())
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_user_and_project_skills_with_frontmatter() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        write_skill(
            &home.path().join("skills/debugging"),
            "Debugging",
            "Find root causes",
            "Use logs first.",
        );
        write_skill(
            &project.path().join(".orca/skills/review"),
            "Review",
            "Review code safely",
            "Read the diff.",
        );

        let skills = discover(project.path(), Some(home.path())).unwrap();

        assert!(skills.iter().any(|skill| skill.id == "debugging"));
        assert!(skills.iter().any(|skill| skill.id == "review"));
        assert!(skills.iter().any(|skill| skill.description == "Review code safely"));
    }

    #[test]
    fn read_skill_formats_body_without_frontmatter() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill(
            &home.path().join("skills/debugging"),
            "Debugging",
            "Find root causes",
            "Use logs first.",
        );

        let skill = discover(project.path(), Some(home.path()))
            .unwrap()
            .into_iter()
            .find(|skill| skill.id == "debugging")
            .unwrap();

        let formatted = format_skill(&skill);
        assert!(formatted.contains("# Debugging"));
        assert!(formatted.contains("Use logs first."));
        assert!(!formatted.contains("---"));
    }

    fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"
            ),
        )
        .unwrap();
    }
}
