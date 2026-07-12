use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::commands::OutputFormat;

const RDNY_BROWSER: &str = include_str!("../../skills/rdny-browser/SKILL.md");

#[derive(Clone, Copy, Debug)]
struct Skill {
    name: &'static str,
    markdown: &'static str,
}

const BUNDLED: &[Skill] = &[Skill {
    name: "rdny-browser",
    markdown: RDNY_BROWSER,
}];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillInfo {
    name: &'static str,
    description: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InstalledSkill {
    name: &'static str,
    path: PathBuf,
}

pub fn list(format: OutputFormat) -> Result<()> {
    validate_bundle()?;
    let skills = BUNDLED.iter().map(skill_info).collect::<Result<Vec<_>>>()?;
    if format == OutputFormat::Human {
        for skill in skills {
            println!("{}\t{}", skill.name, skill.description);
        }
    } else {
        format.emit_json(
            &serde_json::json!({"schemaVersion":1,"kind":"skills.list","skills":skills}),
        )?;
    }
    Ok(())
}

pub fn show(name: &str, format: OutputFormat) -> Result<()> {
    let skill = find(name)?;
    let info = skill_info(skill)?;
    if format == OutputFormat::Human {
        print!("{}", skill.markdown);
    } else {
        format.emit_json(&serde_json::json!({"schemaVersion":1,"kind":"skills.show","skill":{"name":info.name,"description":info.description,"markdown":skill.markdown}}))?;
    }
    Ok(())
}

pub fn install(
    names: &[String],
    dir: Option<PathBuf>,
    force: bool,
    format: OutputFormat,
) -> Result<()> {
    let selection = select(names)?;
    let root = install_root(dir, std::env::var_os("HOME"))?;
    let planned = selection
        .iter()
        .map(|skill| {
            validate(skill)?;
            let path = root.join(skill.name).join("SKILL.md");
            if path.exists() && !force {
                bail!("refusing to overwrite {}; pass --force", path.display());
            }
            Ok((*skill, path))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut installed = Vec::new();
    for (skill, path) in planned {
        let parent = path.parent().expect("skill file has parent");
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        std::fs::write(&path, skill.markdown)
            .with_context(|| format!("writing {}", path.display()))?;
        if format == OutputFormat::Human {
            println!("installed {} -> {}", skill.name, path.display());
        }
        installed.push(InstalledSkill {
            name: skill.name,
            path,
        });
    }
    if format != OutputFormat::Human {
        format.emit_json(
            &serde_json::json!({"schemaVersion":1,"kind":"skills.install","installed":installed}),
        )?;
    }
    Ok(())
}

fn select(names: &[String]) -> Result<Vec<&'static Skill>> {
    if names.is_empty() {
        return Ok(BUNDLED.iter().collect());
    }
    let mut seen = HashSet::new();
    names
        .iter()
        .map(|name| {
            if !seen.insert(name.as_str()) {
                bail!("duplicate skill name: {name}");
            }
            find(name)
        })
        .collect()
}

fn install_root(dir: Option<PathBuf>, home: Option<std::ffi::OsString>) -> Result<PathBuf> {
    match dir {
        Some(dir) => Ok(dir),
        None => Ok(home
            .map(PathBuf::from)
            .context("HOME is not set; pass --dir <path>")?
            .join(".agents/skills")),
    }
}

fn find(name: &str) -> Result<&'static Skill> {
    BUNDLED
        .iter()
        .find(|skill| skill.name == name)
        .with_context(|| format!("unknown skill: {name}"))
}

fn skill_info(skill: &Skill) -> Result<SkillInfo> {
    validate(skill)?;
    Ok(SkillInfo {
        name: skill.name,
        description: frontmatter_value(skill.markdown, "description")?,
    })
}

fn validate_bundle() -> Result<()> {
    let mut seen = HashSet::new();
    for skill in BUNDLED {
        if !seen.insert(skill.name) {
            bail!("duplicate bundled skill name: {}", skill.name);
        }
        validate(skill)?;
    }
    Ok(())
}

fn validate(skill: &Skill) -> Result<()> {
    if !is_safe_name(skill.name) {
        bail!("invalid bundled skill name: {}", skill.name);
    }
    if frontmatter_value(skill.markdown, "name")? != skill.name {
        bail!("invalid bundled metadata for {}", skill.name);
    }
    if skill.markdown.len() > 4096
        || skill.markdown.lines().count() > 60
        || !skill.markdown.ends_with('\n')
        || skill.markdown.ends_with("\n\n")
        || skill.markdown.contains('\t')
        || skill.markdown.contains('\r')
        || skill.markdown.contains("Usage: rdny")
        || skill.markdown.contains("Commands:")
        || skill.markdown.contains("Options:")
    {
        bail!("invalid bundled skill content for {}", skill.name);
    }
    if skill
        .markdown
        .lines()
        .any(|line| line.ends_with(' ') || line.ends_with('\t'))
    {
        bail!("invalid bundled skill whitespace for {}", skill.name);
    }
    Ok(())
}

fn frontmatter_value(markdown: &str, key: &str) -> Result<String> {
    let mut lines = markdown.lines();
    if lines.next() != Some("---") {
        bail!("missing frontmatter");
    }
    for line in lines {
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix(&format!("{key}:")) {
            return Ok(value.trim().trim_matches('"').to_string());
        }
    }
    bail!("missing frontmatter key: {key}")
}

fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && Path::new(name).components().count() == 1
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_skill_metadata_content_limits_and_anchors_are_stable() {
        assert_eq!(BUNDLED.len(), 1);
        let skill = &BUNDLED[0];
        validate(skill).unwrap();
        assert_eq!(skill.name, "rdny-browser");
        assert_eq!(
            frontmatter_value(skill.markdown, "name").unwrap(),
            skill.name
        );
        let description = frontmatter_value(skill.markdown, "description").unwrap();
        assert!(description.starts_with("Use when "));
        assert!(skill.markdown.lines().count() <= 60);
        assert!(skill.markdown.len() <= 4096);
        assert!(skill.markdown.ends_with('\n'));
        assert!(!skill.markdown.ends_with("\n\n"));
        for forbidden in ["\t", "\r", "Usage: rdny", "Commands:", "Options:"] {
            assert!(
                !skill.markdown.contains(forbidden),
                "forbidden {forbidden:?}"
            );
        }
        for anchor in [
            "rdny list",
            "--instance",
            "--state-dir",
            "Before meaningful mutation",
            "observable waits",
            "Use a short sleep",
            "Use structured output when useful",
            "--pierce",
            "Use `js`",
            "trusted input",
            "untrusted content",
            "irreversible actions",
            "Protect credentials",
            "Stop task-owned sessions",
            "rdny COMMAND --help",
            "rdny key --help",
            "rdny pointer --help",
        ] {
            assert!(skill.markdown.contains(anchor), "missing anchor {anchor}");
        }
    }

    #[test]
    fn all_bundled_skill_names_are_unique_and_validated() {
        validate_bundle().unwrap();
        let names = BUNDLED
            .iter()
            .map(|skill| skill.name)
            .collect::<HashSet<_>>();
        assert_eq!(names.len(), BUNDLED.len());
    }

    #[test]
    fn stable_human_and_json_shapes_match_metadata() {
        let info = skill_info(&BUNDLED[0]).unwrap();
        let human = format!("{}\t{}", info.name, info.description);
        assert!(human.starts_with("rdny-browser\tUse when "));
        let list = serde_json::json!({"schemaVersion":1,"kind":"skills.list","skills":[{"name":info.name,"description":info.description}]});
        assert_eq!(list["schemaVersion"], 1);
        assert_eq!(list["kind"], "skills.list");
        assert!(!serde_json::to_string(&list).unwrap().contains('\n'));
    }

    #[test]
    fn selection_preserves_order_rejects_duplicates_unknowns_and_unsafe_names() {
        assert_eq!(select(&[]).unwrap()[0].name, "rdny-browser");
        assert_eq!(
            select(&["rdny-browser".into()]).unwrap()[0].name,
            "rdny-browser"
        );
        assert!(
            select(&["rdny-browser".into(), "rdny-browser".into()])
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        assert!(
            select(&["missing".into()])
                .unwrap_err()
                .to_string()
                .contains("unknown")
        );
        for name in ["../x", ".hidden", "bad_name", "Bad", "a/b"] {
            assert!(!is_safe_name(name));
        }
    }

    #[test]
    fn install_is_all_or_nothing_and_writes_exact_embedded_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        install(&[], Some(root.clone()), false, OutputFormat::Human).unwrap();
        let path = root.join("rdny-browser/SKILL.md");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), RDNY_BROWSER);
        assert!(
            install(&[], Some(root.clone()), false, OutputFormat::Human)
                .unwrap_err()
                .to_string()
                .contains("overwrite")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), RDNY_BROWSER);
        install(
            &["rdny-browser".into()],
            Some(root),
            true,
            OutputFormat::Human,
        )
        .unwrap();
    }

    #[test]
    fn default_install_root_uses_home_and_missing_home_errors() {
        assert_eq!(
            install_root(None, Some(std::ffi::OsString::from("/home/alice"))).unwrap(),
            PathBuf::from("/home/alice/.agents/skills")
        );
        assert!(
            install_root(None, None)
                .unwrap_err()
                .to_string()
                .contains("HOME")
        );
        assert_eq!(
            install_root(Some(PathBuf::from("/tmp/skills")), None).unwrap(),
            PathBuf::from("/tmp/skills")
        );
    }
}
