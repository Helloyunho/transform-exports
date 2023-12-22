use std::collections::HashMap;

use convert_case::{Case, Casing};
use handlebars::{Context, Handlebars, Helper, HelperResult, Output, RenderContext};
use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use swc_cached::regex::CachedRegex;
use swc_ecma_ast::{ExportSpecifier, ModuleExportName, *};
use swc_ecma_visit::{noop_fold_type, Fold};

static DUP_SLASH_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"//").unwrap());

#[derive(Clone, Debug, Deserialize)]
#[serde(transparent)]
pub struct Config {
    pub packages: HashMap<String, PackageConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageConfig {
    pub transform: Transform,
    #[serde(default)]
    pub prevent_full_export: bool,
    #[serde(default)]
    pub skip_default_conversion: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Transform {
    String(String),
    Vec(Vec<(String, String)>),
}

impl From<&str> for Transform {
    fn from(s: &str) -> Self {
        Transform::String(s.to_string())
    }
}
impl From<Vec<(String, String)>> for Transform {
    fn from(v: Vec<(String, String)>) -> Self {
        Transform::Vec(v)
    }
}

struct FoldExports {
    renderer: handlebars::Handlebars<'static>,
    packages: Vec<(CachedRegex, PackageConfig)>,
}

struct Rewriter<'a> {
    renderer: &'a handlebars::Handlebars<'static>,
    key: &'a str,
    config: &'a PackageConfig,
    group: Vec<&'a str>,
}

impl<'a> Rewriter<'a> {
    fn rewrite_named(&self, old_decl: &NamedExport) -> Vec<NamedExport> {
        if old_decl.type_only || old_decl.with.is_some() {
            return vec![old_decl.clone()];
        }

        let mut out: Vec<NamedExport> = Vec::with_capacity(old_decl.specifiers.len());

        for spec in &old_decl.specifiers {
            match spec {
                ExportSpecifier::Named(named_spec) => {
                    #[derive(Serialize)]
                    #[serde(untagged)]
                    enum Data<'a> {
                        Plain(&'a str),
                        Array(&'a [&'a str]),
                    }
                    let name_str = match &named_spec.orig {
                        ModuleExportName::Ident(x) => x.as_ref(),
                        ModuleExportName::Str(x) => x.value.as_ref(),
                    };

                    let mut ctx: HashMap<&str, Data> = HashMap::new();
                    ctx.insert("matches", Data::Array(&self.group[..]));
                    ctx.insert("member", Data::Plain(name_str));

                    let new_path = match &self.config.transform {
                        Transform::String(s) => {
                            self.renderer.render_template(s, &ctx).unwrap_or_else(|e| {
                                panic!("error rendering template for '{}': {}", self.key, e);
                            })
                        }
                        Transform::Vec(v) => {
                            let mut result: Option<String> = None;

                            // We iterate over the items to find the first match
                            v.iter().any(|(k, val)| {
                                let mut key = k.to_string();
                                if !key.starts_with('^') && !key.ends_with('$') {
                                    key = format!("^{}$", key);
                                }

                                // Create a clone of the context, as we need to insert the
                                // `memberMatches` key for each key we try.
                                let mut ctx_with_member_matches: HashMap<&str, Data> =
                                    HashMap::new();
                                ctx_with_member_matches
                                    .insert("matches", Data::Array(&self.group[..]));
                                ctx_with_member_matches.insert("member", Data::Plain(name_str));

                                let regex = CachedRegex::new(&key)
                                    .expect("transform-exports: invalid regex");
                                let group = regex.captures(name_str);

                                if let Some(group) = group {
                                    let group = group
                                        .iter()
                                        .map(|x| x.map(|x| x.as_str()).unwrap_or_default())
                                        .collect::<Vec<&str>>()
                                        .clone();
                                    ctx_with_member_matches
                                        .insert("memberMatches", Data::Array(&group[..]));

                                    result = Some(
                                        self.renderer
                                            .render_template(val, &ctx_with_member_matches)
                                            .unwrap_or_else(|e| {
                                                panic!(
                                                    "error rendering template for '{}': {}",
                                                    self.key, e
                                                );
                                            }),
                                    );

                                    true
                                } else {
                                    false
                                }
                            });

                            if let Some(result) = result {
                                result
                            } else {
                                panic!(
                                    "missing transform for export '{}' of package '{}'",
                                    match named_spec.orig {
                                        ModuleExportName::Ident(ref x) => x.as_ref(),
                                        ModuleExportName::Str(ref x) => x.value.as_ref(),
                                    },
                                    self.key
                                );
                            }
                        }
                    };

                    let new_path = DUP_SLASH_REGEX.replace_all(&new_path, |_: &Captures| "/");
                    let specifier = if self.config.skip_default_conversion {
                        ExportSpecifier::Named(named_spec.clone())
                    } else {
                        ExportSpecifier::Namespace(ExportNamespaceSpecifier {
                            span: named_spec.span,
                            name: named_spec
                                .exported
                                .clone()
                                .unwrap_or(named_spec.orig.clone()),
                        })
                    };
                    out.push(NamedExport {
                        span: old_decl.span,
                        specifiers: vec![specifier],
                        src: Some(Box::new(Str::from(new_path.as_ref()))),
                        type_only: false,
                        with: None,
                    });
                }
                _ => {
                    if self.config.prevent_full_export {
                        panic!(
                            "export {:?} causes the entire module to be exported",
                            old_decl
                        );
                    } else {
                        // Give up
                        return vec![old_decl.clone()];
                    }
                }
            }
        }
        out
    }

    fn rewrite_all(&self, old_decl: &ExportAll) -> Vec<ExportAll> {
        if old_decl.type_only || old_decl.with.is_some() {
            return vec![old_decl.clone()];
        }

        let mut out: Vec<ExportAll> = Vec::with_capacity(1);

        #[derive(Serialize)]
        #[serde(untagged)]
        enum Data<'a> {
            Plain(&'a str),
            Array(&'a [&'a str]),
        }

        let mut ctx: HashMap<&str, Data> = HashMap::new();
        ctx.insert("matches", Data::Array(&self.group[..]));
        ctx.insert("member", Data::Plain("*"));

        let new_path = match &self.config.transform {
            Transform::String(s) => self.renderer.render_template(s, &ctx).unwrap_or_else(|e| {
                panic!("error rendering template for '{}': {}", self.key, e);
            }),
            Transform::Vec(v) => {
                let mut result: Option<String> = None;

                // We iterate over the items to find the first match
                v.iter().any(|(k, val)| {
                    let mut key = k.to_string();
                    if !key.starts_with('^') && !key.ends_with('$') {
                        key = format!("^{}$", key);
                    }

                    // Create a clone of the context, as we need to insert the
                    // `memberMatches` key for each key we try.
                    let mut ctx_with_member_matches: HashMap<&str, Data> = HashMap::new();
                    ctx_with_member_matches.insert("matches", Data::Array(&self.group[..]));
                    ctx_with_member_matches.insert("member", Data::Plain("*"));

                    let regex = CachedRegex::new(&key).expect("transform-exports: invalid regex");
                    let group = regex.captures("*");

                    if let Some(group) = group {
                        let group = group
                            .iter()
                            .map(|x| x.map(|x| x.as_str()).unwrap_or_default())
                            .collect::<Vec<&str>>()
                            .clone();
                        ctx_with_member_matches.insert("memberMatches", Data::Array(&group[..]));

                        result = Some(
                            self.renderer
                                .render_template(val, &ctx_with_member_matches)
                                .unwrap_or_else(|e| {
                                    panic!("error rendering template for '{}': {}", self.key, e);
                                }),
                        );

                        true
                    } else {
                        false
                    }
                });

                if let Some(result) = result {
                    result
                } else {
                    panic!("missing transform for export * of package '{}'", self.key);
                }
            }
        };

        let new_path = DUP_SLASH_REGEX.replace_all(&new_path, |_: &Captures| "/");
        out.push(ExportAll {
            span: old_decl.span,
            src: Box::new(Str::from(new_path.as_ref())),
            type_only: false,
            with: None,
        });
        out
    }
}

impl FoldExports {
    fn should_rewrite<'a>(&'a self, name: Option<&'a str>) -> Option<Rewriter<'a>> {
        match name {
            None => None,
            Some(name) => {
                for (regex, config) in &self.packages {
                    let group = regex.captures(name);
                    if let Some(group) = group {
                        let group = group
                            .iter()
                            .map(|x| x.map(|x| x.as_str()).unwrap_or_default())
                            .collect::<Vec<&str>>();
                        return Some(Rewriter {
                            renderer: &self.renderer,
                            key: name,
                            config,
                            group,
                        });
                    }
                }
                None
            }
        }
    }
}

impl Fold for FoldExports {
    noop_fold_type!();

    fn fold_module(&mut self, mut module: Module) -> Module {
        let mut new_items: Vec<ModuleItem> = vec![];
        for item in module.body {
            match item {
                ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(decl)) => {
                    match self.should_rewrite(match decl.src {
                        None => None,
                        Some(ref x) => Some(&x.value),
                    }) {
                        Some(rewriter) => {
                            let rewritten = rewriter.rewrite_named(&decl);
                            new_items.extend(
                                rewritten
                                    .into_iter()
                                    .map(|x| ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(x))),
                            );
                        }
                        None => {
                            new_items.push(ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(decl)))
                        }
                    }
                }
                ModuleItem::ModuleDecl(ModuleDecl::ExportAll(decl)) => {
                    match self.should_rewrite(Some(&decl.src.value)) {
                        Some(rewriter) => {
                            let rewritten = rewriter.rewrite_all(&decl);
                            new_items.extend(
                                rewritten
                                    .into_iter()
                                    .map(|x| ModuleItem::ModuleDecl(ModuleDecl::ExportAll(x))),
                            );
                        }
                        None => new_items.push(ModuleItem::ModuleDecl(ModuleDecl::ExportAll(decl))),
                    }
                }
                x => {
                    new_items.push(x);
                }
            }
        }
        module.body = new_items;
        module
    }
}

pub fn modularize_exports(config: Config) -> impl Fold {
    let mut folder = FoldExports {
        renderer: handlebars::Handlebars::new(),
        packages: vec![],
    };
    folder
        .renderer
        .register_helper("lowerCase", Box::new(helper_lower_case));
    folder
        .renderer
        .register_helper("upperCase", Box::new(helper_upper_case));
    folder
        .renderer
        .register_helper("camelCase", Box::new(helper_camel_case));
    folder
        .renderer
        .register_helper("kebabCase", Box::new(helper_kebab_case));
    for (mut k, v) in config.packages {
        // XXX: Should we keep this hack?
        if !k.starts_with('^') && !k.ends_with('$') {
            k = format!("^{}$", k);
        }
        folder.packages.push((
            CachedRegex::new(&k).expect("transform-exports: invalid regex"),
            v,
        ));
    }
    folder
}

fn helper_lower_case(
    h: &Helper<'_, '_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    // get parameter from helper or throw an error
    let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");
    out.write(param.to_lowercase().as_ref())?;
    Ok(())
}

fn helper_upper_case(
    h: &Helper<'_, '_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    // get parameter from helper or throw an error
    let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");
    out.write(param.to_uppercase().as_ref())?;
    Ok(())
}

fn helper_camel_case(
    h: &Helper<'_, '_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    // get parameter from helper or throw an error
    let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");

    out.write(param.to_case(Case::Camel).as_ref())?;
    Ok(())
}

fn helper_kebab_case(
    h: &Helper<'_, '_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    // get parameter from helper or throw an error
    let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");

    out.write(param.to_case(Case::Kebab).as_ref())?;
    Ok(())
}
