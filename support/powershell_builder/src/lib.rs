// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PowerShell Command Builder
//!
//! Provides a builder for constructing PowerShell commands with various
//! argument data types and pipelining.

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// A PowerShell script builder
pub struct PowerShellBuilder(Command);

impl PowerShellBuilder {
    // use pwsh if it exists, otherwise powershell. cache the result
    fn get_program() -> &'static str {
        const PWSH: &str = "pwsh.exe";
        const POWERSHELL: &str = "powershell.exe";
        static PROGRAM: OnceLock<&'static str> = OnceLock::new();
        PROGRAM.get_or_init(|| {
            if Self::new_inner(PWSH)
                .cmdlet("exit")
                .finish()
                .build()
                .status()
                .is_ok_and(|r| r.success())
            {
                PWSH
            } else {
                POWERSHELL
            }
        })
    }

    /// Create a new PowerShell command
    pub fn new() -> Self {
        Self::new_inner(Self::get_program())
    }

    fn new_inner(program: &'static str) -> Self {
        PowerShellCmdletBuilder(Command::new(program))
            .flag("NoProfile")
            .flag("NonInteractive")
            .flag("Command")
            .finish()
    }

    /// Start a new Cmdlet
    ///
    /// `cmdlet` is emitted verbatim and must be trusted input.
    pub fn cmdlet<S: AsRef<str>>(self, cmdlet: S) -> PowerShellCmdletBuilder {
        PowerShellCmdletBuilder(self.0).positional(RawVal::new(cmdlet.as_ref()))
    }

    /// Assign the output of the cmdlet to a variable
    ///
    /// `cmdlet` is emitted verbatim and must be trusted input.
    pub fn cmdlet_to_var<S: AsRef<str>>(
        self,
        cmdlet: S,
        varname: &Variable,
    ) -> PowerShellCmdletBuilder {
        PowerShellCmdletBuilder(self.0)
            .positional(varname)
            .positional(RawVal::new("="))
            .finish()
            .cmdlet(cmdlet)
    }

    /// Finish building the powershell script and return the inner `Command`
    pub fn build(self) -> Command {
        self.0
    }
}

/// A PowerShell Cmdlet builder
pub struct PowerShellCmdletBuilder(Command);

impl PowerShellCmdletBuilder {
    /// Add a flag to the cmdlet
    ///
    /// `flag` is emitted verbatim and must be trusted input.
    pub fn flag<S: AsRef<OsStr>>(mut self, flag: S) -> Self {
        let mut arg = OsString::from("-");
        arg.push(flag);
        self.0.arg(arg);
        self
    }

    /// Optionally add a flag to the cmdlet
    pub fn flag_opt<S: AsRef<OsStr>>(self, flag: Option<S>) -> Self {
        if let Some(flag) = flag {
            self.flag(flag)
        } else {
            self
        }
    }

    /// Add a positional argument to the cmdlet
    pub fn positional<S: AsVal>(mut self, positional: S) -> Self {
        self.0.arg(positional.as_val());
        self
    }

    /// Optionally add a positional argument to the cmdlet
    pub fn positional_opt<S: AsVal>(self, positional: Option<S>) -> Self {
        if let Some(positional) = positional {
            self.positional(positional)
        } else {
            self
        }
    }

    /// Add a named argument to the cmdlet
    ///
    /// `name` is emitted verbatim and must be trusted input; `value` is quoted.
    pub fn arg<S: AsRef<OsStr>, T: AsVal>(self, name: S, value: T) -> Self {
        self.flag(name).positional(value)
    }

    /// Optionally add a named argument to the cmdlet
    pub fn arg_opt<S: AsRef<OsStr>, T: AsVal>(self, name: S, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.arg(name, value)
        } else {
            self
        }
    }

    /// Finish the cmdlet
    pub fn finish(self) -> PowerShellBuilder {
        PowerShellBuilder(self.0)
    }

    /// Finish the cmdlet with a pipeline operator
    pub fn pipeline(mut self) -> PowerShellBuilder {
        self.0.arg("|");
        self.finish()
    }

    /// Finish the cmdlet with a semicolon
    pub fn next(mut self) -> PowerShellBuilder {
        self.0.arg(";");
        self.finish()
    }
}

/// A powershell value
pub struct Value(OsString);

impl Value {
    /// Create a new powershell value
    pub fn new(val: impl AsVal) -> Self {
        Self(val.as_val().as_ref().to_owned())
    }
}

impl AsVal for Value {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        &self.0
    }
}

/// Trait for converting to powershell value in raw OsStr form
pub trait AsVal {
    /// Convert to powershell value OsStr
    fn as_val(&self) -> impl '_ + AsRef<OsStr>;
}

impl<T: AsVal + ?Sized> AsVal for &T {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        (*self).as_val()
    }
}

/// Quote a string as a PowerShell single-quoted string literal.
///
/// Single-quoted literals suppress all expansion (`$var`, `$(...)`, backtick
/// escapes), so the only character that needs escaping is `'` itself, which is
/// escaped by doubling it. Using double quotes here would allow any value
/// containing `$(...)` to execute arbitrary PowerShell.
pub fn quote_str(s: &OsStr) -> OsString {
    let mut quoted = OsString::from("'");
    if let Some(s) = s.to_str() {
        quoted.push(s.replace(r#"'"#, r#"''"#));
    } else {
        todo!("quote_str: non-UTF8 string {:?}", s);
    }
    quoted.push("'");
    quoted
}

macro_rules! str {
        ($($ty:ty),* $(,)?) => {
            $(
                impl AsVal for $ty {
                    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
                        quote_str(self.as_ref())
                    }
                }
            )*
        }
    }

str!(&str, String, Path, PathBuf);

/// Implement [`AsVal`] by converting to a string
#[macro_export]
macro_rules! disp_str {
        ($($ty:ty),* $(,)?) => {
            $(
                impl AsVal for $ty {
                    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
                        quote_str(self.to_string().as_ref())
                    }
                }
            )*
        }
    }

disp_str!(jiff::Timestamp, guid::Guid);

impl AsVal for bool {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        if *self { "$true" } else { "$false" }
    }
}

macro_rules! disp {
        ($($ty:ty),* $(,)?) => {
            $(
                impl AsVal for $ty {
                    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
                        self.to_string()
                    }
                }
            )*
        }
    }

disp!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

/// A raw, unquoted powershell value
///
/// The contents are emitted verbatim and are therefore trusted input. Never
/// construct one from caller-supplied strings.
pub struct RawVal<T>(T);

impl<T: AsRef<OsStr>> RawVal<T> {
    /// Create a new raw powershell value
    pub fn new(arg: T) -> Self {
        Self(arg)
    }
}

impl<T: AsRef<OsStr>> AsVal for RawVal<T> {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        &self.0
    }
}

/// A powershell variable
///
/// The name is emitted verbatim and must be trusted input.
pub struct Variable(String);

impl Variable {
    /// Create a new powershell variable
    pub fn new(name: impl AsRef<str>) -> Self {
        Self(format!("${}", name.as_ref()))
    }
}

impl AsVal for Variable {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        &self.0
    }
}

/// A powershell array
pub struct Array(OsString);

impl Array {
    /// Create a new powershell array
    pub fn new<T: AsVal>(v: impl IntoIterator<Item = T>) -> Self {
        let mut args = OsString::new();
        args.push("@(");
        let mut first = true;
        for arg in v {
            if !first {
                args.push("; ");
            }
            args.push(arg.as_val());
            first = false;
        }
        args.push(")");
        Self(args)
    }
}

impl AsVal for Array {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        &self.0
    }
}

/// A powershell hashtable
///
/// Keys are converted with [`AsVal`] just like values, so string keys are
/// quoted and integer keys stay numeric.
pub struct HashTable<K, V>(Vec<(K, V)>);

impl<K: AsVal, V: AsVal> HashTable<K, V> {
    /// Create a new powershell hash table
    pub fn new(v: impl IntoIterator<Item = (K, V)>) -> Self {
        Self(v.into_iter().collect())
    }
}

impl<K: AsVal, V: AsVal> AsVal for HashTable<K, V> {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        let mut args = OsString::new();
        args.push("@{");
        let mut first = true;
        for (k, v) in &self.0 {
            if !first {
                args.push("; ");
            }
            args.push(k.as_val());
            args.push("=");
            args.push(v.as_val());
            first = false;
        }
        args.push("}");
        args
    }
}

/// A powershell script block
///
/// The contents are emitted verbatim and are therefore trusted input. Never
/// construct one from caller-supplied strings.
pub struct Script(String);

impl Script {
    /// Create a new powershell script
    pub fn new(script: impl AsRef<str>) -> Self {
        Self(format!("{{ {} }}", script.as_ref()))
    }
}

impl AsVal for Script {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(v: impl AsVal) -> String {
        v.as_val().as_ref().to_string_lossy().into_owned()
    }

    #[test]
    fn quotes_are_literal() {
        assert_eq!(val("hello"), "'hello'");
        assert_eq!(val("with space"), "'with space'");
        assert_eq!(val("it's"), "'it''s'");
        assert_eq!(val(r"C:\path\to\file"), r"'C:\path\to\file'");
    }

    #[test]
    fn no_expansion_or_escape() {
        // `$`, `$(...)`, backticks and double quotes must all be inert.
        assert_eq!(val("$(Get-Process)"), "'$(Get-Process)'");
        assert_eq!(val("$env:PATH"), "'$env:PATH'");
        assert_eq!(val("a`nb"), "'a`nb'");
        assert_eq!(val(r#"a"b"#), r#"'a"b'"#);
    }

    #[test]
    fn cannot_break_out_of_the_literal() {
        assert_eq!(
            val("x'; Remove-Item -Recurse C:\\; '"),
            "'x''; Remove-Item -Recurse C:\\; '''"
        );
    }

    #[test]
    fn collections_quote_their_elements() {
        assert_eq!(val(Array::new(["a'b", "c"])), "@('a''b'; 'c')");
        assert_eq!(val(HashTable::new([("k", "v'w")])), "@{'k'='v''w'}");
    }

    #[test]
    fn integer_hashtable_keys_stay_numeric() {
        assert_eq!(val(HashTable::new([(0u32, "a")])), "@{0='a'}");
    }
}
