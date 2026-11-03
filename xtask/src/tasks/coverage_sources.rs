// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conservative source-role classification for local coverage reports.

use crate::Xtask;
use anyhow::Context as _;
use proc_macro2::Span;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use syn::spanned::Spanned as _;
use syn::visit::Visit as _;

#[derive(clap::Parser)]
/// Classify coverage source spans without guessing from filenames.
pub struct CoverageSources {
    #[clap(long)]
    input: PathBuf,
    #[clap(long)]
    output: PathBuf,
}

#[derive(Deserialize)]
struct Request {
    files: Vec<String>,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    src_path: PathBuf,
    kind: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Production,
    Test,
    Auxiliary,
    Unknown,
}

#[derive(Serialize)]
struct FileInfo {
    package: Option<String>,
    ranges: Vec<(usize, usize, Role)>,
}

#[derive(Serialize)]
struct SourceMap {
    files: BTreeMap<String, FileInfo>,
    issues: Vec<String>,
}

impl Xtask for CoverageSources {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let request: Request = serde_json::from_slice(&fs_err::read(self.input)?)?;
        let map = classify(&ctx.root, request)?;
        fs_err::write(self.output, serde_json::to_vec(&map)?)?;
        Ok(())
    }
}

fn classify(root: &Path, request: Request) -> anyhow::Result<SourceMap> {
    for package in &request.packages {
        anyhow::ensure!(
            package.manifest_path.is_absolute()
                && package.manifest_path.parent().is_some()
                && package
                    .targets
                    .iter()
                    .all(|target| target.src_path.is_absolute()
                        && target.src_path.parent().is_some()),
            "Cargo source paths must be absolute"
        );
    }
    let mut owners = BTreeMap::new();
    for name in &request.files {
        let path = Path::new(name);
        anyhow::ensure!(
            !path.is_absolute()
                && !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "coverage source must be repository-relative: {name}"
        );
        let owner = request
            .packages
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                root.join(path)
                    .starts_with(p.manifest_path.parent().unwrap())
            })
            .max_by_key(|(_, p)| p.manifest_path.components().count())
            .map(|(index, _)| index);
        owners.insert(name.clone(), owner);
    }
    let needed: BTreeSet<_> = owners.values().filter_map(|p| *p).collect();
    let mut scanner = Scanner::default();
    for index in needed {
        for target in &request.packages[index].targets {
            let role = if target.kind.iter().any(|k| k == "test" || k == "bench") {
                Role::Test
            } else if target
                .kind
                .iter()
                .any(|k| k == "example" || k == "custom-build")
            {
                Role::Auxiliary
            } else {
                Role::Production
            };
            scanner.scan(&target.src_path, role, target.src_path.parent().unwrap())?;
        }
    }
    let mut files = BTreeMap::new();
    for (name, owner) in owners {
        let path = root.join(&name);
        let roles = scanner.roles.remove(&path).unwrap_or_else(|| {
            scanner.issues.push(format!(
                "{name}: source not reached through supported Cargo/module roots"
            ));
            vec![Role::Unknown]
        });
        let mut ranges: Vec<(usize, usize, Role)> = Vec::new();
        for (index, role) in roles.into_iter().enumerate() {
            if let Some((_, end, previous)) = ranges.last_mut()
                && *previous == role
            {
                *end = index + 1;
            } else {
                ranges.push((index + 1, index + 1, role));
            }
        }
        files.insert(
            name,
            FileInfo {
                package: owner.map(|i| request.packages[i].name.clone()),
                ranges,
            },
        );
    }
    Ok(SourceMap {
        files,
        issues: scanner.issues,
    })
}

#[derive(Default)]
struct Scanner {
    visited: BTreeSet<(PathBuf, Role)>,
    roles: BTreeMap<PathBuf, Vec<Role>>,
    issues: Vec<String>,
}

impl Scanner {
    fn scan(&mut self, path: &Path, role: Role, module_dir: &Path) -> anyhow::Result<()> {
        if !self.visited.insert((path.to_owned(), role)) {
            return Ok(());
        }
        let text = fs_err::read_to_string(path)
            .with_context(|| format!("reading coverage source {}", path.display()))?;
        let mut roles = vec![role; text.lines().count().max(1)];
        let mut modules = Vec::new();
        match syn::parse_file(&text) {
            Ok(file) => {
                let role = attribute_role(role, &file.attrs);
                roles.fill(role);
                let mut visitor = Visitor {
                    role,
                    roles: &mut roles,
                    module_dir: module_dir.to_owned(),
                    modules: &mut modules,
                    unsupported_cfg: false,
                    painted: BTreeMap::new(),
                    mixed_lines: BTreeSet::new(),
                };
                visitor.visit_file(&file);
                for line in &visitor.mixed_lines {
                    visitor.roles[line - 1] = Role::Unknown;
                }
                if visitor.unsupported_cfg {
                    for role in &mut roles {
                        if *role == Role::Production {
                            *role = Role::Unknown;
                        }
                    }
                    self.issues.push(format!(
                        "{}: test-dependent attributes inside unsupported syntax",
                        path.display()
                    ));
                }
            }
            Err(error) => {
                roles.fill(Role::Unknown);
                self.issues.push(format!("{}: {error}", path.display()));
            }
        }
        if let Some(previous) = self.roles.get_mut(path) {
            for (previous, current) in previous.iter_mut().zip(roles) {
                if *previous != current {
                    *previous = Role::Unknown;
                }
            }
        } else {
            self.roles.insert(path.to_owned(), roles);
        }
        for (child, role) in modules {
            let module_dir = if child.file_name().is_some_and(|n| n == "mod.rs") {
                child.parent().unwrap().to_owned()
            } else {
                child.with_extension("")
            };
            self.scan(&child, role, &module_dir)?;
        }
        Ok(())
    }
}

struct Visitor<'a> {
    role: Role,
    roles: &'a mut [Role],
    module_dir: PathBuf,
    modules: &'a mut Vec<(PathBuf, Role)>,
    unsupported_cfg: bool,
    painted: BTreeMap<usize, Vec<(Span, Role)>>,
    mixed_lines: BTreeSet<usize>,
}

impl Visitor<'_> {
    fn scoped(&mut self, attrs: &[syn::Attribute], span: Span, visit: impl FnOnce(&mut Self)) {
        let previous = self.role;
        self.role = attribute_role(previous, attrs);
        self.paint(span, self.role);
        visit(self);
        self.role = previous;
    }

    fn paint(&mut self, span: Span, role: Role) {
        for line in span.start().line..=span.end().line {
            let previous = self.painted.entry(line).or_default();
            if previous.iter().any(|(other, other_role)| {
                *other_role != role
                    && ((other.end().line, other.end().column)
                        <= (span.start().line, span.start().column)
                        || (span.end().line, span.end().column)
                            <= (other.start().line, other.start().column))
            }) {
                self.mixed_lines.insert(line);
            }
            previous.push((span, role));
            if let Some(slot) = line.checked_sub(1).and_then(|i| self.roles.get_mut(i)) {
                *slot = role;
            }
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for Visitor<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs = match item {
            syn::Item::Const(i) => &i.attrs,
            syn::Item::Enum(i) => &i.attrs,
            syn::Item::ExternCrate(i) => &i.attrs,
            syn::Item::Fn(i) => &i.attrs,
            syn::Item::ForeignMod(i) => &i.attrs,
            syn::Item::Impl(i) => &i.attrs,
            syn::Item::Macro(i) => &i.attrs,
            syn::Item::Mod(i) => &i.attrs,
            syn::Item::Static(i) => &i.attrs,
            syn::Item::Struct(i) => &i.attrs,
            syn::Item::Trait(i) => &i.attrs,
            syn::Item::TraitAlias(i) => &i.attrs,
            syn::Item::Type(i) => &i.attrs,
            syn::Item::Union(i) => &i.attrs,
            syn::Item::Use(i) => &i.attrs,
            _ => {
                self.paint(item.span(), Role::Unknown);
                return;
            }
        };
        self.scoped(attrs, item.span(), |this| {
            if matches!(item, syn::Item::Macro(_)) && this.role == Role::Production {
                this.role = Role::Unknown;
                this.paint(item.span(), Role::Unknown);
            }
            syn::visit::visit_item(this, item);
        });
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if item.content.is_some() {
            let previous = self.module_dir.clone();
            self.module_dir.push(item.ident.to_string());
            syn::visit::visit_item_mod(self, item);
            self.module_dir = previous;
        } else if !item
            .attrs
            .iter()
            .any(|a| a.path().is_ident("path") || a.path().is_ident("cfg_attr"))
        {
            let direct = self.module_dir.join(format!("{}.rs", item.ident));
            let nested = self.module_dir.join(item.ident.to_string()).join("mod.rs");
            if direct.is_file() != nested.is_file() {
                self.modules
                    .push((if direct.is_file() { direct } else { nested }, self.role));
            }
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.scoped(&item.attrs, item.span(), |this| {
            syn::visit::visit_impl_item_fn(this, item)
        });
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.scoped(&item.attrs, item.span(), |this| {
            syn::visit::visit_trait_item_fn(this, item)
        });
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attribute_role(self.role, std::slice::from_ref(attr)) != self.role {
            self.unsupported_cfg = true;
        }
    }
}

fn attribute_role(parent: Role, attrs: &[syn::Attribute]) -> Role {
    if parent == Role::Test {
        return parent;
    }
    let mut role = parent;
    for attr in attrs {
        if attr.path().is_ident("test") {
            return Role::Test;
        }
        if attr.path().is_ident("cfg_attr") {
            let args = attr.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            );
            let harmless = args.is_ok_and(|args| {
                args.iter().skip(1).all(|meta| {
                    [
                        "allow", "warn", "deny", "forbid", "expect", "doc", "derive", "no_std",
                        "no_main", "repr",
                    ]
                    .iter()
                    .any(|name| meta.path().is_ident(name))
                })
            });
            if !harmless {
                role = Role::Unknown;
            }
        }
        if attr.path().is_ident("cfg")
            && let Ok(meta) = attr.parse_args::<syn::Meta>()
        {
            let (has_test, value) = evaluate_without_test(&meta);
            if has_test {
                match value {
                    Some(false) => return Role::Test,
                    None => role = Role::Unknown,
                    Some(true) => {}
                }
            }
        }
    }
    role
}

fn evaluate_without_test(meta: &syn::Meta) -> (bool, Option<bool>) {
    if meta.path().is_ident("test") {
        return (true, Some(false));
    }
    let syn::Meta::List(list) = meta else {
        return (false, None);
    };
    let Ok(items) = list.parse_args_with(
        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
    ) else {
        return (false, None);
    };
    let values: Vec<_> = items.iter().map(evaluate_without_test).collect();
    let has_test = values.iter().any(|v| v.0);
    let value = if list.path.is_ident("all") {
        if values.iter().any(|v| v.1 == Some(false)) {
            Some(false)
        } else if values.iter().all(|v| v.1 == Some(true)) {
            Some(true)
        } else {
            None
        }
    } else if list.path.is_ident("any") {
        if values.iter().any(|v| v.1 == Some(true)) {
            Some(true)
        } else if values.iter().all(|v| v.1 == Some(false)) {
            Some(false)
        } else {
            None
        }
    } else if list.path.is_ident("not") && values.len() == 1 {
        values[0].1.map(|v| !v)
    } else {
        None
    };
    (has_test, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_inline_tests_and_uncertain_feature_conditions() {
        let text = concat!(
            "fn production() { let x = 1; }\n",
            "#[cfg(all(test, unix))]\n",
            "mod regression { fn helper() { let x = 2; } }\n",
            "#[cfg(any(test, feature = \"extra\"))]\n",
            "fn ambiguous() { let x = 3; }\n",
        );
        let file = syn::parse_file(text).unwrap();
        let mut roles = vec![Role::Production; text.lines().count()];
        let mut modules = Vec::new();
        let mut visitor = Visitor {
            role: Role::Production,
            roles: &mut roles,
            module_dir: PathBuf::from("src"),
            modules: &mut modules,
            unsupported_cfg: false,
            painted: BTreeMap::new(),
            mixed_lines: BTreeSet::new(),
        };
        visitor.visit_file(&file);
        assert!(roles[0] == Role::Production);
        assert!(roles[2] == Role::Test);
        assert!(roles[4] == Role::Unknown);
        let file = syn::parse_file("#![cfg_attr(not(feature = \"std\"), no_std)]\n").unwrap();
        assert!(attribute_role(Role::Production, &file.attrs) == Role::Production);
    }

    #[test]
    fn external_test_modules_are_not_classified_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs_err::write(
            root.join("lib.rs"),
            concat!(
                "mod implementation;\n#[cfg(test)] mod arbitrary_name;\n",
                "#[path = \"redirected.rs\"] mod redirected;\n",
            ),
        )
        .unwrap();
        for file in ["implementation.rs", "arbitrary_name.rs", "redirected.rs"] {
            fs_err::write(root.join(file), "fn function() {}\n").unwrap();
        }
        let map = classify(
            root,
            Request {
                files: ["implementation.rs", "arbitrary_name.rs", "redirected.rs"]
                    .map(str::to_owned)
                    .to_vec(),
                packages: vec![Package {
                    name: "fixture".into(),
                    manifest_path: root.join("Cargo.toml"),
                    targets: vec![Target {
                        src_path: root.join("lib.rs"),
                        kind: vec!["lib".into()],
                    }],
                }],
            },
        )
        .unwrap();
        assert!(map.files["implementation.rs"].ranges[0].2 == Role::Production);
        assert!(map.files["arbitrary_name.rs"].ranges[0].2 == Role::Test);
        assert!(map.files["redirected.rs"].ranges[0].2 == Role::Unknown);
        assert!(!map.issues.is_empty());
    }

    #[test]
    fn shared_roots_and_parse_errors_stay_unknown() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lib.rs");
        fs_err::write(&path, "fn shared() {}\n").unwrap();
        let mut scanner = Scanner::default();
        scanner
            .scan(&path, Role::Production, directory.path())
            .unwrap();
        scanner.scan(&path, Role::Test, directory.path()).unwrap();
        assert!(scanner.roles[&path][0] == Role::Unknown);
        let bad = directory.path().join("bad.rs");
        fs_err::write(&bad, "not valid rust!\n").unwrap();
        scanner
            .scan(&bad, Role::Production, directory.path())
            .unwrap();
        assert!(scanner.roles[&bad][0] == Role::Unknown);
        assert!(!scanner.issues.is_empty());
        fs_err::write(&path, "#[test] fn test() {} fn production() {}\n").unwrap();
        let mut scanner = Scanner::default();
        scanner
            .scan(&path, Role::Production, directory.path())
            .unwrap();
        assert!(scanner.roles[&path][0] == Role::Unknown);
    }
}
