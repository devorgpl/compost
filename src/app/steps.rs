//! Steps of `backup.before_backup` and the like: commands run in a service of
//! the instance, with `docker compose exec` or `docker compose run`.

use anyhow::{Context, Result};

use super::instance;
use crate::config::app::{AppConfig, Step};
use crate::connector::shell_quote;
use crate::shared::ServerCtx;

/// Run the steps in order; the first that fails stops the rest.
/// `key` names the list in `backup`, for messages.
pub fn run(ctx: &ServerCtx, app: &AppConfig, key: &str, steps: &[Step]) -> Result<()> {
    let compose = instance::compose_command(ctx, app);
    for (index, step) in steps.iter().enumerate() {
        let (service, exec) = step.target();
        let verb = if exec { "exec" } else { "run" };
        out!("  {key}: {verb} {service} ...");
        let args: Vec<String> = step
            .command
            .iter()
            .flat_map(|c| c.args())
            .map(|arg| shell_quote(&arg))
            .collect();
        let script = format!(
            "{compose} {} -T {}{}{}",
            if exec { "exec" } else { "run --rm" },
            shell_quote(service),
            if args.is_empty() { "" } else { " " },
            args.join(" ")
        );
        let output = ctx
            .connector
            .exec(&script)
            .with_context(|| format!("backup.{key}[{index}] ({verb} {service})"))?;
        for line in output.lines().filter(|l| !l.trim().is_empty()) {
            out!("    {line}");
        }
    }
    Ok(())
}

/// Run cleanup steps (`after_*`), also after Ctrl-C. Their failure is a
/// warning: the data is already saved or loaded, or an error is on its way.
pub fn run_after(ctx: &ServerCtx, app: &AppConfig, key: &str, steps: &[Step]) {
    if steps.is_empty() {
        return;
    }
    let _cleanup = crate::interrupt::cleanup();
    if let Err(err) = run(ctx, app, key, steps) {
        eprintln!("warning: {} {err:#}", app.instance());
    }
}
