// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PowerShell Command Builder
//!
//! Provides a builder for constructing PowerShell commands with various
//! argument data types and pipelining.

#![cfg(windows)]
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// A PowerShell script builder
pub struct PowerShellBuilder(Command);

impl PowerShellBuilder {
    /// Create a new PowerShell command
    pub fn new() -> Self {
        PowerShellCmdletBuilder(Command::new("powershell.exe"))
            .flag("NoProfile")
            .finish()
    }

    /// Start a new Cmdlet
    pub fn cmdlet<S: AsRef<str>>(self, cmdlet: S) -> PowerShellCmdletBuilder {
        PowerShellCmdletBuilder(self.0).positional(RawVal::new(cmdlet.as_ref()))
    }

    /// Assign the output of the cmdlet to a variable
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

/// wrap a string in quotes
pub fn quote_str(s: &OsStr) -> OsString {
    let mut quoted = OsString::new();
    quoted.push("\"");
    // TODO: escape this properly.
    quoted.push(s);
    quoted.push("\"");
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

/// A raw powershell value
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
pub struct HashTable<K, V>(Vec<(K, V)>);

impl<K: AsRef<str>, V: AsVal> HashTable<K, V> {
    /// Create a new powershell hash table
    pub fn new(v: impl IntoIterator<Item = (K, V)>) -> Self {
        Self(v.into_iter().collect())
    }
}

impl<K: AsRef<str>, V: AsVal> AsVal for HashTable<K, V> {
    fn as_val(&self) -> impl '_ + AsRef<OsStr> {
        let mut args = OsString::new();
        args.push("@{");
        let mut first = true;
        for (k, v) in &self.0 {
            if !first {
                args.push("; ");
            }
            args.push(k.as_ref());
            args.push("=");
            args.push(v.as_val());
            first = false;
        }
        args.push("}");
        args
    }
}

/// A powershell script
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
