//! `compost agent …`: the documentation for AI agents (docs/agents), built into
//! the binary so it always matches this version.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::error::config_err;

const LLMS: &str = include_str!("../../docs/agents/llms.txt");
const LLMS_FULL: &str = include_str!("../../docs/agents/llms-full.txt");
const SKILL: &str = include_str!("../../docs/agents/skills/compost/SKILL.md");

/// The skill's `references/`: (file name, contents) of the pages of docs/wiki,
/// as `just docs` copied them (see build.rs).
const REFERENCES: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/references.rs"));

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Print llms.txt: what compost is and an index of its documentation
    Llms {
        /// Print llms-full.txt instead: the whole documentation of this version in one file
        #[arg(long)]
        full: bool,
    },
    /// Install the compost Agent Skill, with the documentation of this version
    Skill {
        /// Skills folder; the skill goes in its `compost` subfolder, e.g. `~/.claude/skills` for all your projects or `.agents/skills`
        #[arg(long, default_value = ".claude/skills")]
        dir: PathBuf,
    },
}

pub fn run(command: AgentCommand) -> Result<()> {
    match command {
        AgentCommand::Llms { full } => {
            out!("{}", if full { LLMS_FULL } else { LLMS }.trim_end());
            Ok(())
        }
        AgentCommand::Skill { dir } => install_skill(&dir),
    }
}

/// Write SKILL.md and references/ to `<dir>/compost`, replacing an older copy.
fn install_skill(dir: &Path) -> Result<()> {
    let skill_dir = dir.join("compost");
    let skill_file = skill_dir.join("SKILL.md");
    if let Ok(existing) = fs::read_to_string(&skill_file) {
        if !existing.lines().any(|line| line.trim() == "name: compost") {
            return Err(config_err(format!(
                "{} is another skill; remove it or choose another --dir",
                skill_file.display()
            )));
        }
    }
    let references = skill_dir.join("references");
    if references.exists() {
        fs::remove_dir_all(&references)
            .with_context(|| format!("cannot remove {}", references.display()))?;
    }
    fs::create_dir_all(&references)
        .with_context(|| format!("cannot create {}", references.display()))?;
    fs::write(&skill_file, SKILL)
        .with_context(|| format!("cannot write {}", skill_file.display()))?;
    for (name, contents) in REFERENCES {
        let path = references.join(name);
        fs::write(&path, contents).with_context(|| format!("cannot write {}", path.display()))?;
    }
    out!(
        "Installed the compost skill for compost {} in {}",
        env!("CARGO_PKG_VERSION"),
        skill_dir.display()
    );
    Ok(())
}
