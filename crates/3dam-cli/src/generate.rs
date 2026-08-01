//! Shell completions and man pages, rendered from the live clap tree (ADR 0009 §10).
//!
//! These are generated at **runtime** by the shipped binary (`3dam completions <shell>`,
//! `3dam man`) rather than by a build script, so the command tree in `args.rs` stays the single
//! source of truth — there is no second copy of the grammar for a `build.rs`/`xtask` to parse, and
//! the artifacts a release ships are by construction the ones that binary actually accepts. The
//! release pipeline just runs the binary it built (tech-spec 15 §15.5).
//!
//! The one wrinkle is [`full_command`]: `serve` and `mcp` are separate roles chosen by
//! `dam_frontend::classify` *before* the CLI grammar runs (tech-spec 01 §5), so their arguments
//! live in their own `Parser` structs and are invisible to `Cli::command()`. Completions and man
//! pages describe the binary, not the CLI role, so they are grafted back on here.

use super::*;
use clap::CommandFactory;
use clap_complete::Shell;
use std::path::Path;

/// The whole `3dam` binary as one clap `Command`: the CLI verb tree plus the `serve` and `mcp`
/// roles that `classify` intercepts ahead of it.
///
/// Generation-only. Nothing parses through this — grafting the roles into the real parser would
/// make them unreachable dead branches, since `classify` never lets `serve`/`mcp` reach `Cli`.
fn full_command() -> clap::Command {
    // Both structs already carry `#[command(name = …)]`, so only the missing `about` is added.
    Cli::command()
        .subcommand(
            ServeArgs::command()
                .about("Run the HTTP/WS server with the embedded web client and MCP endpoint."),
        )
        .subcommand(
            McpArgs::command().about("Run the MCP server over stdio against the local library."),
        )
}

/// `3dam completions <shell> [--out DIR]`.
///
/// Writes to stdout by default, which is the form users actually want
/// (`eval "$(3dam completions bash)"`). With `--out` it uses clap_complete's own naming
/// (`3dam.bash`, `_3dam`, `3dam.fish`, `_3dam.ps1`) so the files can be dropped straight into the
/// conventional completion directories, and prints each path it wrote.
pub(crate) fn completions(shell: Shell, out: Option<&Path>) -> anyhow::Result<()> {
    let mut cmd = full_command();
    match out {
        None => clap_complete::generate(shell, &mut cmd, BIN, &mut std::io::stdout()),
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            let path = clap_complete::generate_to(shell, &mut cmd, BIN, dir)?;
            println!("{}", path.display());
        }
    }
    Ok(())
}

/// `3dam man [--out DIR]`.
///
/// Stdout renders only the top-level `3dam(1)` page (roff, pipe it to `man -l -`). `--out` writes
/// the full recursive set — `3dam.1`, `3dam-scan.1`, `3dam-admin-token-add.1`, … — because a man
/// page per subcommand is what `man 3dam-scan` needs to resolve, and the deep `admin` tree is
/// exactly where a reader is most likely to look.
pub(crate) fn man(out: Option<&Path>) -> anyhow::Result<()> {
    let cmd = full_command();
    match out {
        None => clap_mangen::Man::new(cmd).render(&mut std::io::stdout())?,
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            clap_mangen::generate_to(cmd, dir)?;
            println!("{}", dir.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the graft in [`full_command`]: `serve`/`mcp` are invisible to `Cli::command()`
    /// because `classify` owns them, so completions would silently omit two of the binary's four
    /// roles if this were dropped.
    #[test]
    fn the_generated_tree_covers_all_four_roles() {
        let cmd = full_command();
        let names: Vec<_> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        for want in ["scan", "search", "admin", "serve", "mcp"] {
            assert!(names.contains(&want), "missing `{want}` in {names:?}");
        }
    }

    /// The completion scripts embed the binary name; `3dam` starts with a digit and the package is
    /// `dam` (ADR 0010), so this is exactly the kind of thing that drifts silently.
    #[test]
    fn completions_render_for_every_shipped_shell() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
            let mut buf = Vec::new();
            clap_complete::generate(shell, &mut full_command(), BIN, &mut buf);
            let script = String::from_utf8(buf).expect("completion script is not UTF-8");
            assert!(
                script.contains("3dam"),
                "{shell} completions never mention the binary name"
            );
            assert!(script.contains("serve"), "{shell} completions omit `serve`");
        }
    }

    #[test]
    fn the_man_page_renders() {
        let mut buf = Vec::new();
        clap_mangen::Man::new(full_command())
            .render(&mut buf)
            .expect("roff render failed");
        let page = String::from_utf8(buf).expect("man page is not UTF-8");
        // `.TH` is the roff title macro every man page opens with, and `.SH` its section headings.
        assert!(page.contains(".TH"), "not a man page: {:?}", &page[..80]);
        assert!(page.contains(".SH SYNOPSIS"));
        assert!(page.contains("3dam"));
    }
}
