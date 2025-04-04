// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CLI argument parsing for the underhill init process.

#![warn(missing_docs)]

// We've made our own parser here instead of using something like clap in order
// to save on compiled file size. We don't need all the features a crate can provide.
/// underhill init command-line options.
#[derive(Debug, Default)]
pub struct Options {
    /// additional setup commands to run before starting underhill
    pub setup_script: Vec<String>,

    /// additional args to run underhill with
    pub underhill_args: Vec<String>,
}

impl Options {
    pub(crate) fn parse() -> Self {
        let mut opts = Self::default();

        let args: Vec<_> = std::env::args_os().collect();
        // Skip our own filename.
        let mut i = 1;

        while let Some(next) = args.get(i) {
            let arg = next.to_string_lossy();

            if arg.starts_with("--") && arg.len() > 2 {
                if let Some(eq) = arg.find('=') {
                    let (name, value) = arg.split_at(eq);
                    let parsed = Self::parse_value_arg(&mut opts, name, &value[1..]); // Don't forget to exclude the '=' itself.

                    if !parsed {
                        break;
                    }
                } else {
                    if let Some(value) = args.get(i + 1).map(|x| x.to_string_lossy()) {
                        let parsed = Self::parse_value_arg(&mut opts, &arg, &value);

                        if parsed {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            } else if arg == "--" {
                i += 1;
                break;
            } else {
                break;
            }

            i += 1;
        }

        opts.underhill_args = args
            .into_iter()
            .skip(i)
            .map(|x| x.to_string_lossy().into_owned())
            .collect();

        opts
    }

    #[must_use]
    fn parse_value_arg(opts: &mut Self, name: &str, value: &str) -> bool {
        match name {
            "--setup-script" => {
                opts.setup_script.push(value.to_owned());
            }
            _ => return false,
        }

        true
    }
}
