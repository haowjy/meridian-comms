use anyhow::{Context, Result, bail};

use crate::config::Config;

const GUIDE: &str = include_str!("../docs/slack-setup.md");

/// Prints the Slack app manifest from the setup guide, with the owner filled
/// in, and on stderr what Meridian's config needs for `deliver`.
pub fn run(owner: Option<String>) -> Result<()> {
    let owner = match owner {
        Some(o) => o,
        None => Config::load().context("pass --owner NAME, or write the config first")?.owner,
    };
    if owner.is_empty()
        || !owner
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        bail!("owner {owner:?} must be lowercase letters, digits, '-' or '_': it becomes the bot's name");
    }
    let manifest = GUIDE
        .split("```json\n")
        .nth(1)
        .and_then(|s| s.split("```").next())
        .context("docs/slack-setup.md has no JSON manifest")?;
    let mut cap = owner.clone();
    cap[..1].make_ascii_uppercase();
    print!(
        "{}",
        manifest
            .replace("jimmy-agents", &format!("{owner}-agents"))
            .replace("Jimmy's", &format!("{cap}'s"))
    );
    eprintln!("\n{}", crate::source::meridian::people::SETUP);
    Ok(())
}
