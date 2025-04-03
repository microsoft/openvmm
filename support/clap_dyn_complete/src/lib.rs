// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dynamic, Rust-driven shell completions for binaries that use `clap`.
//!
//! `clap_dyn_complete` differs from `clap_complete` in two ways:
//!
//! - Supports dynamically generating completion sets at runtime
//!     - e.g: `cargo build -p <TAB>` completions that are generated by parsing
//!       `Cargo.toml` at completion-time to return a list of valid crate names
//!     - vs. `clap_complete`, which can only emit completions for values known
//!       ahead-of-time
//! - _Far_ easier to add support for new shells
//!     - Logic is driven entirely from Rust, so all that's needed is a small
//!       "shell adapter" script to adapt `clap_dyn_complete` output to a
//!       particular shell completion engine.
//!     - vs. `clap_complete`, which requires writing a bespoke code-gen backend
//!       for every kind of shell!
//!
//! That said, `clap_dyn_complete` has one major downside vs. `clap_complete`:
//! increased binary size.
//!
//! `clap_complete` completions are entirely separate from the binary, whereas
//! `clap_dyn_complete` completions call back into binary to drive completions,
//! requiring the binary to include its own completion engine.

#![forbid(unsafe_code)]

use clap::Parser;
use futures::future::BoxFuture;
use futures::future::FutureExt;

/// A `clap`-compatible struct that can be used to generate completions for the
/// current CLI invocation.
#[derive(Parser)]
pub struct Complete {
    /// A single string corresponding to the raw CLI invocation.
    ///
    /// e.g: `$ my-command $((1 + 2))   b<TAB>ar baz` would pass `--raw
    /// "my-command $((1 + 2))   bar baz"`.
    ///
    /// Note the significant whitespace!
    ///
    /// May not always be available, depending on the shell.
    #[clap(long, requires = "position")]
    pub raw: Option<String>,

    /// The cursor's position within the raw CLI invocation.
    ///
    /// e.g: `$ my-command $((1 + 2))   b<TAB>ar baz` would pass `--position 25`
    ///
    /// May not always be available, depending on the shell.
    #[clap(long, requires = "raw")]
    pub position: Option<usize>,

    /// A list of strings corresponding to how the shell has interpreted the
    /// current command.
    ///
    /// e.g: `$ my-command foo $((1 + 2)) bar` would pass `-- my-command foo 3
    /// bar`.
    #[clap(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

impl Complete {
    /// Generate completions for the given `clap` command, and prints them to
    /// stdout in the format the built-in stub scripts expect.
    ///
    /// See [`Complete::generate_completions`] for more info.
    pub async fn println_to_stub_script<Cli: clap::CommandFactory>(
        self,
        maybe_subcommand_of: Option<&str>,
        custom_completer_factory: impl CustomCompleterFactory,
    ) {
        let completions = self
            .generate_completions::<Cli>(maybe_subcommand_of, custom_completer_factory)
            .await;

        for completion in completions {
            log::debug!("suggesting: {}", completion);
            println!("{}", completion);
        }
    }

    /// Generate completions for the given `clap` command.
    ///
    /// Set `maybe_subcommand_of` to the root command's value if the binary may
    /// be invoked as subcommand. e.g: if the binary is invoked as `cargo
    /// xtask`, pass `Some("cargo")`.
    pub async fn generate_completions<Cli: clap::CommandFactory>(
        self,
        maybe_subcommand_of: Option<&str>,
        custom_completer_factory: impl CustomCompleterFactory,
    ) -> Vec<String> {
        let Self { position, raw, cmd } = self;

        // check if invoked as subcommand (e.g: `cargo foobar`), and if so, we
        // should skip "cargo" before continuing
        let cmd = {
            let mut cmd = cmd;
            if let Some(maybe_subcommand_of) = maybe_subcommand_of {
                if cmd.first().map(|s| s.as_str()) == Some(maybe_subcommand_of) {
                    cmd.remove(0);
                }
            }

            cmd
        };

        log::debug!("");
        log::debug!("cmd: [{}]", cmd.clone().join(" , "));
        log::debug!("raw: '{}'", raw.as_deref().unwrap_or_default());
        log::debug!("position: {:?}", position);

        let (prev_arg, to_complete) = match (position, &raw) {
            (Some(position), Some(raw)) => {
                if position <= raw.len() {
                    let (before, _) = raw.split_at(position);
                    log::debug!("completing from: '{}'", before);
                    let mut before_toks = before.split_whitespace().collect::<Vec<_>>();
                    if before.ends_with(|c: char| c.is_whitespace()) {
                        before_toks.push("")
                    }
                    match before_toks.as_slice() {
                        [] => ("", ""),
                        [a] => ("", *a),
                        [.., a, b] => (*a, *b),
                    }
                } else {
                    (cmd.last().unwrap().as_str(), "")
                }
            }
            _ => match cmd.as_slice() {
                [] => ("", ""),
                [a] => ("", a.as_ref()),
                [.., a, b] => (a.as_ref(), b.as_ref()),
            },
        };

        let base_command = Cli::command();
        // "massage" the command to make it more amenable to completions
        let command = {
            command_visitor(
                base_command.clone(),
                &mut |arg| {
                    // do any arg-level tweaks
                    loosen_value_parser(arg)
                },
                &mut |mut command| {
                    // avoid treating the help flag as something special. make
                    // it just another arg for the purposes of shell completion
                    if !command.is_disable_help_flag_set() {
                        command = command.disable_help_flag(true).arg(
                            clap::Arg::new("my_help")
                                .short('h')
                                .long("help")
                                .action(clap::ArgAction::SetTrue),
                        )
                    }

                    command
                },
            )
        };

        let matches = (command.clone())
            .ignore_errors(true)
            .try_get_matches_from(&cmd)
            .unwrap();

        let ctx = RootCtx {
            command: base_command,
            matches: matches.clone(),
            to_complete,
            prev_arg,
        };

        log::debug!("to_complete: {to_complete}");
        log::debug!("prev_arg: {prev_arg}");

        let mut completions = recurse_completions(
            &ctx,
            Vec::new(),
            &command,
            &matches,
            Box::new(custom_completer_factory.build(&ctx).await),
        )
        .await;

        // only suggest words that match what the user has already entered
        completions.retain(|x| x.starts_with(to_complete));

        // we want "whole words" to appear before flags
        completions.sort_by(|a, b| match (a.starts_with('-'), b.starts_with('-')) {
            (true, true) => a.cmp(b),
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => a.cmp(b),
        });

        completions
    }
}

/// Tweaks an Arg to "loosen" how strict its value parsers is, while leaving
/// existing lists of `possible_values` intact.
///
/// Without this pass, providing mid-word completions to args that have a value
/// parser is impossible, as clap will eagerly parse the half-complete input as
/// though it was a full input, fail, and then omit the result entirely from
/// `ArgMatches` (which the subsequent completion code relies on to check if the
/// arg was present or not).
fn loosen_value_parser(arg: clap::Arg) -> clap::Arg {
    use clap::builder::TypedValueParser;

    #[derive(Clone)]
    struct PossibleValueString(Vec<clap::builder::PossibleValue>);

    impl TypedValueParser for PossibleValueString {
        type Value = <clap::builder::StringValueParser as TypedValueParser>::Value;

        fn parse_ref(
            &self,
            cmd: &clap::Command,
            arg: Option<&clap::Arg>,
            value: &std::ffi::OsStr,
        ) -> Result<Self::Value, clap::Error> {
            clap::builder::StringValueParser::new().parse_ref(cmd, arg, value)
        }

        fn possible_values(
            &self,
        ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
            Some(Box::new(self.0.clone().into_iter()))
        }
    }

    let possible_vals = arg.get_possible_values();
    arg.value_parser(PossibleValueString(possible_vals))
}

/// Do a recursive depth-first visit to all arguments and subcommands of the
/// given command.
///
/// `on_subcommand` is invoked _before_ recursing into the subcommand.
fn command_visitor(
    mut command: clap::Command,
    mut on_arg: &mut dyn FnMut(clap::Arg) -> clap::Arg,
    mut on_subcommand: &mut dyn FnMut(clap::Command) -> clap::Command,
) -> clap::Command {
    for subcommand in command.clone().get_subcommands() {
        command = command
            .mut_subcommand(subcommand.get_name(), &mut *on_subcommand)
            .mut_subcommand(subcommand.get_name(), |command| {
                command_visitor(command, &mut on_arg, &mut on_subcommand)
            })
    }

    for arg in command.clone().get_arguments() {
        command = command.mut_arg(arg.get_id(), &mut *on_arg);
    }

    command
}

/// Context for the current CLI invocation.
#[derive(Debug)]
pub struct RootCtx<'a> {
    /// The command being completed
    pub command: clap::Command,
    /// The current set of matches
    pub matches: clap::ArgMatches,
    /// The current word being completed
    pub to_complete: &'a str,
    /// The previous argument (useful when completing flag values)
    pub prev_arg: &'a str,
}

/// A factory for [`CustomCompleter`]s.
///
/// Having a two-step construction flow is useful to avoid constantly
/// re-initializing "expensive" objects during custom completion (e.g:
/// re-parsing a TOML file on every invocation to `CustomComplete::complete`).
pub trait CustomCompleterFactory: Send + Sync {
    /// The concrete [`CustomCompleter`].
    type CustomCompleter: CustomCompleter + 'static;

    /// Build a new [`CustomCompleter`].
    fn build(&self, ctx: &RootCtx<'_>) -> impl std::future::Future<Output = Self::CustomCompleter>;
}

/// A custom completer for a particular argument.
pub trait CustomCompleter: Send + Sync {
    /// Generates a list of completions for the given argument.
    fn complete(
        &self,
        ctx: &RootCtx<'_>,
        subcommand_path: &[&str],
        arg_id: &str,
    ) -> impl Send + std::future::Future<Output = Vec<String>>;
}

#[async_trait::async_trait]
trait DynCustomCompleter: Send + Sync {
    async fn complete(
        &self,
        ctx: &RootCtx<'_>,
        subcommand_path: &[&str],
        arg_id: &str,
    ) -> Vec<String>;
}

#[async_trait::async_trait]
impl<T: CustomCompleter> DynCustomCompleter for T {
    async fn complete(
        &self,
        ctx: &RootCtx<'_>,
        subcommand_path: &[&str],
        arg_id: &str,
    ) -> Vec<String> {
        self.complete(ctx, subcommand_path, arg_id).await
    }
}

impl CustomCompleterFactory for () {
    type CustomCompleter = ();
    async fn build(&self, _ctx: &RootCtx<'_>) -> Self::CustomCompleter {}
}

impl CustomCompleter for () {
    async fn complete(
        &self,
        _ctx: &RootCtx<'_>,
        _subcommand_path: &[&str],
        _arg_id: &str,
    ) -> Vec<String> {
        Vec::new()
    }
}

// drills-down through subcommands to generate the right completion set
fn recurse_completions<'a>(
    ctx: &'a RootCtx<'_>,
    subcommand_path: Vec<&'a str>,
    command: &'a clap::Command,
    matches: &'a clap::ArgMatches,
    custom_complete_fn: Box<dyn DynCustomCompleter>,
) -> BoxFuture<'a, Vec<String>> {
    async move {
        let mut subcommand_path = subcommand_path;
        subcommand_path.push(command.get_name());

        let mut completions = Vec::new();

        // begin by recursing down into the subcommands
        // TODO: before recursing, add suppose for inherited args
        if let Some((name, matches)) = matches.subcommand() {
            let subcommand = command
                .get_subcommands()
                .find(|s| s.get_name() == name)
                .unwrap();
            let mut new_completions = recurse_completions(
                ctx,
                subcommand_path,
                subcommand,
                matches,
                custom_complete_fn,
            )
            .await;
            new_completions.extend_from_slice(&completions);
            return new_completions;
        }

        // check if `prev_arg` was a `-` arg or a `--` arg that accepts a
        // free-form completion value.
        //
        // do this first, since if it turns out we are completing a flag value,
        // we want to limit our suggestions to just the things that arg expects
        for arg in command.get_arguments() {
            let long = arg.get_long().map(|x| format!("--{x}")).unwrap_or_default();
            let short = arg.get_short().map(|x| format!("-{x}")).unwrap_or_default();

            if ctx.prev_arg != long && ctx.prev_arg != short {
                continue;
            }

            if !matches!(
                arg.get_action(),
                clap::ArgAction::Append | clap::ArgAction::Set
            ) {
                continue;
            }

            // ah, ok, the current completion corresponds to the value of the
            // prev_arg!

            for val in arg.get_possible_values() {
                completions.push(val.get_name().into())
            }

            completions.extend(
                custom_complete_fn
                    .complete(ctx, &subcommand_path, arg.get_id().as_str())
                    .await,
            );

            // immediately stop suggesting
            return completions;
        }

        // check positional args
        //
        // TODO: think about how to handle multiple-invoked conditionals
        let mut is_completing_positional = false;
        for positional in command.get_positionals() {
            // check if the arg has already been set, and if so: skip its
            // corresponding suggestions
            if matches
                .try_contains_id(positional.get_id().as_str())
                .unwrap_or(true)
            {
                // ...but if the user is actively overriding the already-set
                // arg, then _don't_ skip it!
                let val = matches
                    .get_raw(positional.get_id().as_str())
                    .unwrap_or_default()
                    .next_back()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or_default();

                if ctx.to_complete.is_empty() || ctx.to_complete.starts_with('-') {
                    continue;
                }

                if !val.starts_with(ctx.to_complete) {
                    continue;
                }
            }

            is_completing_positional = true;

            let possible_vals = positional.get_possible_values();
            if !possible_vals.is_empty() {
                for val in possible_vals {
                    completions.push(val.get_name().into())
                }
            }

            completions.extend(
                custom_complete_fn
                    .complete(ctx, &subcommand_path, positional.get_id().as_str())
                    .await,
            );

            // don't want to suggest values for subsequent positionals
            break;
        }

        // suggest all `-` and `--` args
        for arg in command.get_arguments() {
            if matches!(
                matches.value_source(arg.get_id().as_str()),
                Some(clap::parser::ValueSource::CommandLine)
            ) {
                // check if the arg was already set, and if so, don't suggest it again
                //
                // FIXME: handle args that can be set multiple times
                if let Some(x) = matches.get_raw_occurrences(arg.get_id().as_str()) {
                    if x.flatten().count() != 0 {
                        continue;
                    }
                }
            }

            if let Some(long) = arg.get_long() {
                completions.push(format!("--{long}"))
            }

            if let Some(short) = arg.get_short() {
                completions.push(format!("-{short}"))
            }
        }

        // suggest all subcommands
        //
        // ...unless we're completing a positional, since we don't want to
        // suggest subcommand names as valid options for the positional
        if command.has_subcommands() && !is_completing_positional {
            if !command.is_disable_help_subcommand_set() {
                completions.push("help".into());
            }

            for subcommand in command.get_subcommands() {
                if let Some((name, _)) = matches.subcommand() {
                    if !ctx.to_complete.is_empty() && name.starts_with(ctx.to_complete) {
                        completions.push(subcommand.get_name().into())
                    }
                } else {
                    completions.push(subcommand.get_name().into())
                }
            }
        }

        completions
    }
    .boxed()
}

/// Shell with in-tree completion stub scripts available
#[derive(Clone, clap::ValueEnum)]
pub enum Shell {
    /// [Fish](https://fishshell.com/)
    Fish,
    /// [Powershell](https://docs.microsoft.com/en-us/powershell/)
    Powershell,
    /// [Zsh](https://www.zsh.org/)
    Zsh,
}

/// Emits a minimal "shell adapter" script, tailored to the particular bin.
///
/// `completion_subcommand` should be a string corresponding to the name of the
/// subcommand that invokes [`Complete`].
///
/// - e.g: a bin that has `my-bin complete ...` should pass `"complete"`
/// - e.g: a bin that has `my-bin dev complete ...` should pass `"dev complete"`
///
/// **NOTE:** Feel free to ignore / modify these stubs to suit your particular
/// use-case! e.g: These stubs will not work in cases where the binary is
/// invoked as a subcommand (e.g: `cargo xtask`). In those cases, you may need
/// to write additional shell-specific logic!
pub fn emit_completion_stub(
    shell: Shell,
    bin_name: &str,
    completion_subcommand: &str,
    buf: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let stub = match shell {
        Shell::Fish => include_str!("./templates/complete.fish"),
        Shell::Powershell => include_str!("./templates/complete.ps1"),
        Shell::Zsh => include_str!("./templates/complete.zsh"),
    };

    let stub = stub
        .replace("__COMMAND_NAME__", bin_name)
        .replace("__COMMAND_NAME_NODASH__", &bin_name.replace('-', "_"))
        .replace("__COMPLETION_SUBCOMMAND__", completion_subcommand);

    buf.write_all(stub.as_bytes())
}
