//! Symbol index, visibility rules and interface stubs.

use crate::config::{Config, Experiment};
use anyhow::{Context, Result, anyhow, bail, ensure};
use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use std::{collections::HashSet, ops::Range};
use verus_syn::{
    FnMode, ImplItem, Item, Publish,
    buffer::Cursor,
    parse::{Parse, ParseStream, Parser},
    spanned::Spanned,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Interface,
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum File {
    Task,
    Dependencies,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    /// `name`, `Type::method`, or `impl Trait for Type` for a trait impl, which is one symbol.
    pub name: String,
    /// For example "spec fn", "proof fn", "fn", "struct".
    pub kind: String,
    pub file: File,
    pub visibility: Visibility,
    /// 1-based inclusive line range in the real source.
    pub lines: (usize, usize),
    /// What the agent may see. Empty for hidden symbols.
    pub view: String,
}

pub struct Index {
    pub symbols: Vec<Symbol>,
    /// The agent-visible replacement for dependencies.rs.
    pub stubs: String,
}

impl Index {
    /// Fails if the config is invalid, including when anything visible mentions a hidden symbol.
    pub fn build(experiment: &Experiment) -> Result<Index> {
        let config = &experiment.config;
        let mut task = Builder::new(&experiment.task, File::Task, config);
        task.file(&parse(&experiment.task).context("src/task.rs")?)?;
        let mut deps = Builder::new(&experiment.dependencies, File::Dependencies, config);
        let stubs = deps
            .file(&parse(&experiment.dependencies).context("src/dependencies.rs")?)
            .context("src/dependencies.rs")?;

        let mut listed = HashSet::new();
        for name in config.public.iter().chain(&config.interface_only) {
            ensure!(listed.insert(name), "`{name}` is listed twice");
            ensure!(
                deps.symbols.iter().any(|s| &s.name == name),
                "`{name}` is not a symbol in dependencies.rs"
            );
        }
        let mut seen = HashSet::new();
        for symbol in &deps.symbols {
            ensure!(
                seen.insert(&symbol.name),
                "duplicate symbol `{}`",
                symbol.name
            );
        }

        let mut symbols = task.symbols;
        symbols.append(&mut deps.symbols);
        let index = Index { symbols, stubs };
        // Comments count too, so this looks at words rather than tokens.
        for name in index.hidden_names() {
            for (what, text) in [
                ("task.rs", &experiment.task),
                ("a visible part of dependencies.rs", &index.stubs),
            ] {
                ensure!(
                    !words(text).any(|w| w == name),
                    "{what} mentions hidden symbol `{name}`"
                );
            }
        }
        Ok(index)
    }

    /// Last path segments of hidden symbols, for example `method` for `Type::method`.
    pub fn hidden_names(&self) -> Vec<String> {
        self.symbols
            .iter()
            .filter(|s| s.visibility == Visibility::Hidden && !s.kind.starts_with("impl"))
            .map(|s| s.name.rsplit("::").next().unwrap().to_string())
            .collect()
    }
}

/// Parsed items with the exact tokens each came from. The tokens matter because verus_syn does not
/// print everything it parses: it drops `broadcast`, for example.
pub struct Parsed<T> {
    pub attrs: TokenStream,
    pub items: Vec<(T, TokenStream)>,
}

pub fn parse(source: &str) -> Result<Parsed<Item>> {
    split(
        source
            .parse()
            .map_err(|e: proc_macro2::LexError| anyhow!("line {}: {e}", e.span().start().line))?,
    )
}

fn split<T: Parse>(tokens: TokenStream) -> Result<Parsed<T>> {
    let parser = |input: ParseStream| {
        // Returns the tokens between `from` and the current position.
        let since = |mut from: Cursor| {
            let mut out = TokenStream::new();
            while from != input.cursor() {
                let (token, next) = from.token_tree().expect("tokens were just parsed");
                out.extend([token]);
                from = next;
            }
            out
        };
        let start = input.cursor();
        input.call(verus_syn::Attribute::parse_inner)?;
        let attrs = since(start);
        let mut items = vec![];
        while !input.is_empty() {
            let start = input.cursor();
            let item = input.parse::<T>()?;
            items.push((item, since(start)));
        }
        Ok(Parsed { attrs, items })
    };
    parser
        .parse2(tokens)
        .map_err(|e| anyhow!("line {}: {e}", e.span().start().line))
}

/// The items inside a `verus! { ... }` block, or None for any other item.
pub fn verus_block(item: &Item) -> Option<Result<Parsed<Item>>> {
    match item {
        Item::Macro(m) if m.mac.path.is_ident("verus") => Some(split(m.mac.tokens.clone())),
        _ => None,
    }
}

/// Every identifier in the tokens, with any `r#` prefix removed.
pub fn idents(tokens: TokenStream) -> HashSet<String> {
    let mut out = HashSet::new();
    for token in tokens {
        match token {
            TokenTree::Group(g) => out.extend(idents(g.stream())),
            TokenTree::Ident(i) => {
                out.insert(i.to_string().trim_start_matches("r#").to_string());
            }
            _ => {}
        }
    }
    out
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
}

fn range(node: &impl ToTokens) -> Range<usize> {
    let spans = node
        .to_token_stream()
        .into_iter()
        .map(|t| t.span().byte_range())
        .filter(|r| !r.is_empty());
    spans
        .reduce(|a, b| a.start.min(b.start)..a.end.max(b.end))
        .unwrap_or(0..0)
}

struct Builder<'a> {
    source: &'a str,
    file: File,
    config: &'a Config,
    symbols: Vec<Symbol>,
}

impl<'a> Builder<'a> {
    fn new(source: &'a str, file: File, config: &'a Config) -> Self {
        Builder {
            source,
            file,
            config,
            symbols: vec![],
        }
    }

    fn text(&self, node: &impl ToTokens) -> String {
        self.source[range(node)].to_string()
    }

    /// Indexes the file and returns the part of it the agent may see.
    fn file(&mut self, file: &Parsed<Item>) -> Result<String> {
        let mut out = String::new();
        if !file.attrs.is_empty() {
            out += &format!("{}\n", self.text(&file.attrs));
        }
        for (item, tokens) in &file.items {
            if let Some(block) = verus_block(item) {
                let body: String = block?
                    .items
                    .iter()
                    .map(|(i, t)| self.item(i, t))
                    .collect::<Result<_>>()?;
                out += &format!("\nverus! {{\n\n{body}}}\n");
            } else if self.file == File::Task || matches!(item, Item::Use(_)) {
                out += &self.item(item, tokens)?;
            } else {
                bail!(
                    "line {}: only `use` may appear outside verus!",
                    item.span().start().line
                );
            }
        }
        Ok(out)
    }

    fn item(&mut self, item: &Item, tokens: &TokenStream) -> Result<String> {
        let text = self.text(tokens);
        let view = match item {
            Item::Use(_) | Item::BroadcastUse(_) => Some(text),
            Item::Fn(f) => {
                let view = self.interface(
                    &f.attrs,
                    &f.sig,
                    range(&f.vis),
                    &f.block,
                    f.semi_token.is_some(),
                    tokens,
                );
                self.add(
                    f.sig.ident.to_string(),
                    fn_kind(&f.sig),
                    tokens,
                    text,
                    Some(view),
                )?
            }
            Item::Struct(i) => self.add(i.ident.to_string(), "struct", tokens, text, None)?,
            Item::Enum(i) => self.add(i.ident.to_string(), "enum", tokens, text, None)?,
            Item::Const(i) => self.add(i.ident.to_string(), "const", tokens, text, None)?,
            Item::Type(i) => self.add(i.ident.to_string(), "type", tokens, text, None)?,
            Item::Trait(i) => self.add(i.ident.to_string(), "trait", tokens, text, None)?,
            Item::Impl(i) => self.impl_block(i, tokens, text)?,
            _ if self.file == File::Task => None,
            _ => bail!(
                "line {}: unsupported kind of item",
                item.span().start().line
            ),
        };
        Ok(view.map_or(String::new(), |v| format!("{v}\n\n")))
    }

    fn impl_block(
        &mut self,
        i: &verus_syn::ItemImpl,
        tokens: &TokenStream,
        text: String,
    ) -> Result<Option<String>> {
        // `Stack<T>` is indexed as `Stack`.
        let ty = match &*i.self_ty {
            verus_syn::Type::Path(p) => p
                .path
                .segments
                .last()
                .context("impl type")?
                .ident
                .to_string(),
            other => self.text(other),
        };
        if let Some((_, path, _)) = &i.trait_ {
            let name = format!(
                "impl {} for {ty}",
                path.segments.last().context("impl trait")?.ident
            );
            return self.add(name, "impl", tokens, text, None);
        }
        let mut methods = vec![];
        let Some(TokenTree::Group(body)) = tokens.clone().into_iter().last() else {
            bail!("impl without a body")
        };
        for (member, tokens) in &split::<ImplItem>(body.stream())?.items {
            let text = self.text(tokens);
            let added = match member {
                ImplItem::Fn(f) => {
                    let view = self.interface(
                        &f.attrs,
                        &f.sig,
                        range(&f.vis),
                        &f.block,
                        f.semi_token.is_some(),
                        tokens,
                    );
                    self.add(
                        format!("{ty}::{}", f.sig.ident),
                        fn_kind(&f.sig),
                        tokens,
                        text,
                        Some(view),
                    )?
                }
                ImplItem::Const(c) => {
                    self.add(format!("{ty}::{}", c.ident), "const", tokens, text, None)?
                }
                _ => bail!(
                    "line {}: unsupported kind of impl item",
                    member.span().start().line
                ),
            };
            methods.extend(added);
        }
        let header =
            &self.source[range(tokens).start..i.brace_token.span.open().byte_range().start];
        Ok((!methods.is_empty()).then(|| format!("{header}{{\n{}\n}}", methods.join("\n\n"))))
    }

    /// Records a symbol and returns what the agent may see of it.
    fn add(
        &mut self,
        name: String,
        kind: &str,
        node: &impl ToTokens,
        text: String,
        interface: Option<String>,
    ) -> Result<Option<String>> {
        let visibility = if self.file == File::Task || self.config.public.contains(&name) {
            Visibility::Public
        } else if self.config.interface_only.contains(&name) {
            Visibility::Interface
        } else {
            Visibility::Hidden
        };
        let view = match visibility {
            Visibility::Public => Some(text),
            Visibility::Interface => Some(
                interface
                    .with_context(|| format!("`{name}` is interface_only but is not a function"))?,
            ),
            Visibility::Hidden => None,
        };
        let span = range(node);
        let line = |at: usize| self.source[..at].matches('\n').count() + 1;
        self.symbols.push(Symbol {
            name,
            kind: kind.to_string(),
            file: self.file,
            visibility,
            lines: (line(span.start), line(span.end)),
            view: view.clone().unwrap_or_default(),
        });
        Ok(view)
    }

    /// The function with its body replaced, ready to be verified in the stubs.
    fn interface(
        &self,
        attrs: &[verus_syn::Attribute],
        sig: &verus_syn::Signature,
        vis: Range<usize>,
        block: &verus_syn::Block,
        bodiless: bool,
        tokens: &TokenStream,
    ) -> String {
        if bodiless {
            return self.text(tokens);
        }
        let docs = attrs
            .iter()
            .filter(|a| a.path().is_ident("doc"))
            .map(|a| self.text(a) + "\n")
            .collect::<String>();
        let vis = match &self.source[vis] {
            "" => String::new(),
            v => format!("{v} "),
        };
        // Everything after the publish modifier and before the body, minus `decreases`.
        let after_attrs = attrs
            .iter()
            .map(|a| range(a).end)
            .max()
            .unwrap_or(range(tokens).start);
        let start = after_attrs.max(range(&sig.publish).end).max(sig_start(sig));
        let end = block.brace_token.span.open().byte_range().start;
        let rest = match &sig.spec.decreases {
            Some(d) => {
                let d = range(d);
                format!(
                    "{}{}",
                    &self.source[start..d.start],
                    &self.source[d.end..end]
                )
            }
            None => self.source[start..end].to_string(),
        };
        let rest = rest.trim();
        match sig.mode {
            FnMode::Spec(_) | FnMode::SpecChecked(_) => {
                debug_assert!(!matches!(sig.publish, Publish::Uninterp(_)));
                format!("{docs}{vis}uninterp {rest};")
            }
            _ => format!("{docs}#[verifier::external_body]\n{vis}{rest}\n{{ unimplemented!() }}"),
        }
    }
}

/// Where the signature proper starts, after any visibility modifier and `open`/`closed`.
fn sig_start(sig: &verus_syn::Signature) -> usize {
    let mut sig = sig.clone();
    sig.publish = Publish::Default;
    let start = range(&sig).start;
    sig.broadcast
        .map_or(start, |b| start.min(b.span.byte_range().start))
}

fn fn_kind(sig: &verus_syn::Signature) -> &'static str {
    match sig.mode {
        FnMode::Spec(_) | FnMode::SpecChecked(_) => "spec fn",
        FnMode::Proof(_) | FnMode::ProofAxiom(_) => "proof fn",
        FnMode::Exec(_) | FnMode::Default => "fn",
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    const DEPS: &str = r#"// SECRET file comment: é
use vstd::prelude::*;
verus! {
// SECRET comment
/// Public doc.
pub open spec fn shown(x: int) -> bool { x > 0 }
#[verifier::opaque]
pub open spec fn iface(x: nat) -> nat
    decreases x,
{ if x == 0 { 41 } else { iface((x - 1) as nat) } }
pub proof fn lemma(x: int)
    requires shown(x),
    ensures x >= 1,
    decreases x,
{ assert(x >= 1); }
pub broadcast proof fn blemma(x: int)
    ensures #[trigger] shown(x) == shown(x),
{ }
pub proof fn rlemma(x: int) -> (r: int) ensures r == x { x }
pub fn exec(x: u64) -> (r: u64) ensures r == x { x }
spec fn secret(x: int) -> bool { x == 42 }
pub struct S { pub a: u64 }
impl S {
    pub open spec fn m(self) -> bool { self.a > 1 }
    pub closed spec fn n(&self) -> bool { self.a > 7 }
    spec fn hid(self) -> bool { true }
}
struct T { b: u64 }
impl T { spec fn gone(self) -> bool { true } }
}
"#;

    pub fn experiment(task: &str, deps: &str, public: &[&str], interface: &[&str]) -> Experiment {
        let own = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let config = serde_json::json!({"name": "t", "description": "", "public": own(public), "interface_only": own(interface)});
        Experiment {
            config: serde_json::from_value(config).unwrap(),
            task: task.into(),
            dependencies: deps.into(),
        }
    }

    const PUBLIC: &[&str] = &["shown", "S", "S::m"];
    const INTERFACE: &[&str] = &["iface", "lemma", "blemma", "rlemma", "exec", "S::n"];

    fn error(task: &str, deps: &str, public: &[&str], interface: &[&str]) -> String {
        format!(
            "{:#}",
            Index::build(&experiment(task, deps, public, interface))
                .err()
                .expect("should fail")
        )
    }

    #[test]
    fn stubs_show_only_what_is_allowed() {
        let index = Index::build(&experiment("", DEPS, PUBLIC, INTERFACE)).unwrap();
        for hidden in [
            "SECRET",
            "secret",
            "41",
            "hid",
            "gone",
            "struct T",
            "impl T",
            "> 7",
            "decreases",
            "opaque",
        ] {
            assert!(!index.stubs.contains(hidden), "{hidden} in {}", index.stubs);
        }
        for shown in [
            "/// Public doc.",
            "x > 0",
            "self.a > 1",
            "pub broadcast proof fn blemma",
            "pub uninterp spec fn iface",
            "ensures x >= 1",
            "pub uninterp spec fn n",
        ] {
            assert!(
                index.stubs.contains(shown),
                "{shown} not in {}",
                index.stubs
            );
        }
        assert_eq!(index.hidden_names(), ["secret", "hid", "T", "gone"]);
        let shown = index.symbols.iter().find(|s| s.name == "shown").unwrap();
        assert_eq!((shown.lines, shown.kind.as_str()), ((5, 6), "spec fn"));
        // `VSB_DUMP_STUBS=path cargo test` writes the stubs so they can be run through Verus.
        if let Ok(path) = std::env::var("VSB_DUMP_STUBS") {
            std::fs::write(path, &index.stubs).unwrap();
        }
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(error("", DEPS, &["nope"], &[]).contains("not a symbol"));
        assert!(error("", DEPS, &["shown"], &["shown"]).contains("listed twice"));
        assert!(error("", DEPS, &[], &["S"]).contains("not a function"));
        let twice = "verus! { spec fn a() -> bool { true } spec fn a() -> bool { true } }";
        assert!(error("", twice, &[], &[]).contains("duplicate"));
        assert!(error("", "macro_rules! m { () => {} }", &[], &[]).contains("only `use`"));
        assert!(
            error("", "verus! { macro_rules! m { () => {} } }", &[], &[]).contains("unsupported")
        );
    }

    #[test]
    fn rejects_visible_mentions_of_hidden_symbols() {
        let deps = "verus! { spec fn h() -> bool { true } spec fn b() -> bool { h() } proof fn l() ensures h() {} }";
        assert!(error("", deps, &["b"], &[]).contains("mentions hidden"));
        assert!(error("", deps, &[], &["l"]).contains("mentions hidden"));
        assert!(
            error("verus! { proof fn p() { assert(h()); } }", deps, &[], &[])
                .contains("task.rs mentions")
        );
        Index::build(&experiment("", deps, &[], &["b"])).unwrap();
        // A visible method on a hidden type.
        let deps = "verus! { struct T; impl T { spec fn m() -> bool { true } } }";
        assert!(error("", deps, &["T::m"], &[]).contains("mentions hidden"));
    }
}
