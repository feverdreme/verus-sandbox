//! Decides whether a proposal changes only what a proposal may change.

use crate::{
    config::Experiment,
    source::{Index, Parsed, idents, parse, verus_block},
};
use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use std::collections::HashSet;
use verus_syn::{Block, Expr, Item, Token, parse::Parser, punctuated::Punctuated};

const MAX_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 100;
const MAX_STEPS: usize = 100_000;

/// Identifiers that add trust, read files, or define something other than a function.
/// Any identifier starting with one of PREFIXES is banned as well.
const BANNED: &[&str] = &[
    "admit", "axiom", "unsafe", "exec_allows_no_decreases_clause", "may_not_terminate", "macro_rules",
    "unimplemented", "extern", "asm", "env", "option_env", "builtin", "verus_builtin", "inline_air_stmt", "verus",
    "uninterp", "global", "use", "mod", "impl", "struct", "enum", "union", "trait", "static", "const", "type",
];
const PREFIXES: &[&str] = &["assume", "external", "include"];

const MACROS: &[&str] = &[
    "assert", "assert_by_contradiction", "assert_seqs_equal", "assert_sets_equal", "assert_maps_equal",
    "assert_forall_by", "seq", "set", "map", "calc", "vec", "matches",
];
const ATTRIBUTES: &[&str] = &[
    "trigger", "auto", "all_triggers", "via_fn", "verifier::rlimit", "verifier::spinoff_prover",
    "verifier::nonlinear", "verifier::integer_ring", "verifier::loop_isolation", "verifier::opaque",
    "verifier::inline", "verifier::truncate", "verifier::memoize",
];
/// Keywords that can precede `!` and a group without being a macro call.
const KEYWORDS: &[&str] = &[
    "if", "while", "return", "match", "in", "else", "implies", "by", "break", "requires", "ensures", "recommends",
    "invariant", "invariant_except_break", "decreases", "returns", "when", "via",
];
const CLAUSES: &[&str] = &[
    "requires", "ensures", "recommends", "invariant", "invariant_except_break", "decreases", "returns",
];

const UNAVAILABLE: &str = "the proposal uses a name that is not available";

/// Returns the reasons the proposal is rejected. Empty means accepted.
pub fn check(experiment: &Experiment, index: &Index, proposal: &str) -> Vec<String> {
    if proposal.len() > MAX_BYTES {
        return vec![format!("the proposal is larger than {MAX_BYTES} bytes")];
    }
    if max_depth(proposal) > MAX_DEPTH {
        return vec!["the proposal is nested too deeply".into()];
    }
    // The parser recurses, so deep expressions need a deep stack.
    std::thread::scope(|s| {
        let check = || Checker::new(experiment, index).run(proposal);
        let thread = std::thread::Builder::new().stack_size(1 << 30).spawn_scoped(s, check);
        match thread.map(|t| t.join()) {
            Ok(Ok(reasons)) => reasons,
            _ => vec!["the proposal could not be checked".into()],
        }
    })
}

/// An upper bound on bracket nesting. Brackets in strings and comments count too.
fn max_depth(source: &str) -> usize {
    let (mut depth, mut max) = (0usize, 0);
    for c in source.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        max = max.max(depth);
    }
    max
}

/// A token tree without positions, doc comments or `r#` prefixes.
#[derive(Clone)]
enum Node {
    Group(Delimiter, Vec<Node>, TokenTree),
    Leaf(String, TokenTree),
}

impl Node {
    fn token(&self) -> &TokenTree {
        match self {
            Node::Group(.., t) | Node::Leaf(_, t) => t,
        }
    }

    fn is(&self, text: &str) -> bool {
        matches!(self, Node::Leaf(s, _) if s == text)
    }

    fn is_punct(&self, c: char) -> bool {
        matches!(self.token(), TokenTree::Punct(p) if p.as_char() == c)
    }
}

fn nodes(tokens: TokenStream) -> Vec<Node> {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < tokens.len() {
        // Skip `#[doc = "..."]` and `#![doc = "..."]`.
        if matches!(&tokens[i], TokenTree::Punct(p) if p.as_char() == '#') {
            let bang = matches!(tokens.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!') as usize;
            if matches!(tokens.get(i + 1 + bang), Some(TokenTree::Group(g)) if is_doc(g.stream())) {
                i += 2 + bang;
                continue;
            }
        }
        out.push(match &tokens[i] {
            TokenTree::Group(g) => Node::Group(g.delimiter(), nodes(g.stream()), tokens[i].clone()),
            TokenTree::Ident(id) => Node::Leaf(id.to_string().trim_start_matches("r#").into(), tokens[i].clone()),
            TokenTree::Literal(l) => Node::Leaf(l.to_string(), tokens[i].clone()),
            TokenTree::Punct(p) => {
                let joint = p.spacing() == Spacing::Joint && matches!(tokens.get(i + 1), Some(TokenTree::Punct(_)));
                Node::Leaf(format!("{}{}", p.as_char(), if joint { "+" } else { "" }), tokens[i].clone())
            }
        });
        i += 1;
    }
    out
}

fn is_doc(tokens: TokenStream) -> bool {
    let t: Vec<TokenTree> = tokens.into_iter().collect();
    matches!(&t[..], [TokenTree::Ident(i), TokenTree::Punct(p), TokenTree::Literal(_)] if i == "doc" && p.as_char() == '=')
}

/// If a hole starts at `at`, the number of tokens it covers.
fn hole(nodes: &[Node], at: usize) -> Option<usize> {
    match &nodes[at..] {
        [name, bang, Node::Group(..), rest @ ..] if name.is("todo") && bang.is_punct('!') => {
            let statement = at == 0 || nodes[at - 1].is_punct(';') || is_brace(&nodes[at - 1]);
            Some(3 + (statement && rest.first().is_some_and(|n| n.is_punct(';'))) as usize)
        }
        _ => None,
    }
}

fn is_brace(node: &Node) -> bool {
    matches!(node, Node::Group(Delimiter::Brace, ..))
}

/// Whether `fill` may replace the hole of `width` tokens at `at`.
fn fill_parses(old: &[Node], at: usize, width: usize, outer: Option<Delimiter>, fill: &[Node]) -> bool {
    let tokens: TokenStream = fill.iter().map(|n| n.token().clone()).collect();
    let previous = at.checked_sub(1).map(|p| &old[p]);
    let next = old.get(at + width);
    let starts_statement = match previous {
        None => outer == Some(Delimiter::Brace),
        Some(p) => p.is_punct(';') || is_brace(p),
    };
    if starts_statement && (width == 4 || next.is_none()) {
        return Block::parse_within.parse2(tokens).is_ok();
    }
    // Next to an operator, a fill such as `true || true` would change how the surrounding code
    // groups, so it must be parenthesized.
    let assigned = at >= 2 && old[at - 1].is("=") && !matches!(&old[at - 2], Node::Leaf(s, _) if s.ends_with('+'));
    let free_left = previous.is_none_or(|p| {
        p.is_punct(',') || p.is_punct(';') || assigned || matches!(p.token(), TokenTree::Ident(_)) || is_brace(p)
    });
    let free_right = next.is_none_or(|n| n.is_punct(',') || n.is_punct(';') || is_brace(n));
    if !(free_left && free_right) {
        return matches!(fill, [Node::Group(Delimiter::Parenthesis, ..)]) && verus_syn::parse2::<Expr>(tokens).is_ok();
    }
    let list = previous.is_some_and(|p| p.is_punct(',') || CLAUSES.iter().any(|c| p.is(c)));
    if list {
        !fill.is_empty() && Punctuated::<Expr, Token![,]>::parse_terminated.parse2(tokens).is_ok()
    } else {
        verus_syn::parse2::<Expr>(tokens).is_ok()
    }
}

struct Checker<'a> {
    experiment: &'a Experiment,
    hidden: HashSet<String>,
    task_idents: HashSet<String>,
    dependency_idents: HashSet<String>,
    reasons: Vec<String>,
    steps: usize,
}

impl<'a> Checker<'a> {
    fn new(experiment: &'a Experiment, index: &Index) -> Self {
        let tokens = |s: &str| idents(s.parse().unwrap_or_default());
        Checker {
            experiment,
            hidden: index.hidden_names().into_iter().collect(),
            task_idents: tokens(&experiment.task),
            dependency_idents: index
                .symbols
                .iter()
                .filter_map(|s| s.name.rsplit("::").next().map(str::to_string))
                .collect(),
            reasons: vec![],
            steps: 0,
        }
    }

    fn reject(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        if !self.reasons.contains(&reason) {
            self.reasons.push(reason);
        }
    }

    fn run(mut self, proposal: &str) -> Vec<String> {
        match (parse(&self.experiment.task), parse(proposal)) {
            (Ok(task), Ok(proposal)) => self.file(&task, &proposal),
            (Err(_), _) => self.reject("the task does not parse"),
            (_, Err(e)) => self.reject(format!("the proposal does not parse: {e}")),
        }
        self.reasons
    }

    fn file(&mut self, task: &Parsed<Item>, proposal: &Parsed<Item>) {
        if !self.same(&task.attrs, &proposal.attrs) || task.items.len() != proposal.items.len() {
            self.reject("the code outside `verus!` was changed");
            return;
        }
        for ((old, old_tokens), (new, new_tokens)) in task.items.iter().zip(&proposal.items) {
            match (verus_block(old), verus_block(new)) {
                (Some(Ok(a)), Some(Ok(b))) => {
                    if !self.same(&shell(old_tokens), &shell(new_tokens)) || !self.same(&a.attrs, &b.attrs) {
                        self.reject("the `verus!` block itself was changed");
                    }
                    self.items(&a.items, &b.items);
                }
                (_, Some(Err(e))) => self.reject(format!("the proposal does not parse: {e}")),
                _ => {
                    if !self.same(old_tokens, new_tokens) {
                        self.reject("the code outside `verus!` was changed");
                    }
                }
            }
        }
    }

    fn same(&mut self, a: &TokenStream, b: &TokenStream) -> bool {
        let mut fills = vec![];
        self.matches(&nodes(a.clone()), &nodes(b.clone()), None, &mut fills) && fills.is_empty()
    }

    /// The original items must all appear, in order. Anything else must be a new function.
    fn items(&mut self, old: &[(Item, TokenStream)], new: &[(Item, TokenStream)]) {
        let old_nodes: Vec<Vec<Node>> = old.iter().map(|(_, t)| nodes(t.clone())).collect();
        let mut at = 0;
        for (item, tokens) in new {
            let item_nodes = nodes(tokens.clone());
            let mut fills = vec![];
            // Skipping over an original item means it is missing.
            let found = (at..old.len()).find(|&i| {
                fills.clear();
                self.matches(&old_nodes[i], &item_nodes, None, &mut fills)
            });
            if let Some(found) = found {
                (at..found).for_each(|i| self.missing(&old[i].1));
                at = found + 1;
                for fill in fills {
                    self.scan(&fill);
                    if fill.iter().any(|n| count(n, "fn") > 0) {
                        self.reject("a filled hole may not define a function; add it next to the existing functions");
                    }
                }
            } else if let Item::Fn(f) = item {
                self.added(f, &item_nodes);
            } else {
                let name = item_name(tokens);
                self.reject(format!("only functions may be added, and existing items may not change: {name}"));
            }
        }
        old[at..].iter().for_each(|(_, tokens)| self.missing(tokens));
        if self.steps > MAX_STEPS {
            self.reject("the proposal is too complex to compare with the task");
        }
    }

    fn missing(&mut self, item: &TokenStream) {
        self.reject(format!("an existing item is missing or was changed outside its holes: {}", item_name(item)));
    }

    fn added(&mut self, f: &verus_syn::ItemFn, item: &[Node]) {
        let name = f.sig.ident.to_string().trim_start_matches("r#").to_string();
        if self.hidden.contains(&name) {
            self.reject(UNAVAILABLE);
        } else if self.task_idents.contains(&name) || self.dependency_idents.contains(&name) {
            self.reject(format!("`{name}` was changed outside its holes, or a new function reuses that name"));
        }
        if f.semi_token.is_some() {
            self.reject(format!("new function `{name}` has no body"));
        }
        if item.iter().map(|n| count(n, "fn")).sum::<usize>() != 1 {
            self.reject(format!("new function `{name}` contains another function"));
        }
        self.scan(item);
    }

    /// Looks for anything that agent-written code may not contain.
    fn scan(&mut self, code: &[Node]) {
        for (i, node) in code.iter().enumerate() {
            match node {
                Node::Group(_, inner, _) => self.scan(inner),
                Node::Leaf(text, TokenTree::Ident(_)) => {
                    let broadcast_use = text == "use" && i > 0 && code[i - 1].is("broadcast");
                    if text == "todo" {
                        self.reject("a hole is not filled: `todo!` remains");
                    } else if self.hidden.contains(text) {
                        self.reject(UNAVAILABLE);
                    } else if (BANNED.contains(&text.as_str()) && !broadcast_use)
                        || PREFIXES.iter().any(|p| text.starts_with(p))
                    {
                        self.reject(format!("`{text}` is not allowed"));
                    } else if let [bang, Node::Group(..), ..] = &code[i + 1..]
                        && bang.is_punct('!')
                        && !MACROS.contains(&text.as_str())
                        && !KEYWORDS.contains(&text.as_str())
                    {
                        self.reject(format!("macro `{text}!` is not allowed"));
                    }
                }
                Node::Leaf(..) if node.is_punct('#') => {
                    let bang = code.get(i + 1).is_some_and(|n| n.is_punct('!')) as usize;
                    if let Some(Node::Group(Delimiter::Bracket, attr, _)) = code.get(i + 1 + bang) {
                        let path: String = attr
                            .iter()
                            .take_while(|n| !matches!(n, Node::Group(..)) && !n.is_punct('=') && !n.is_punct(','))
                            .map(|n| n.token().to_string())
                            .collect();
                        // `#[trigger]` takes no arguments, but `#![trigger f(x)]` does.
                        let trigger = bang == 1 && attr.first().is_some_and(|n| n.is("trigger"));
                        if !trigger && !ATTRIBUTES.contains(&path.as_str()) {
                            self.reject(format!("attribute `{path}` is not allowed"));
                        }
                    }
                }
                Node::Leaf(..) => {}
            }
        }
    }

    /// Compares tokens, letting each hole in `old` match anything that parses in its place.
    /// Collects what filled the holes.
    fn matches(&mut self, old: &[Node], new: &[Node], outer: Option<Delimiter>, fills: &mut Vec<Vec<Node>>) -> bool {
        self.matches_from(old, 0, new, outer, fills)
    }

    fn matches_from(
        &mut self,
        old: &[Node],
        mut at: usize,
        mut new: &[Node],
        outer: Option<Delimiter>,
        fills: &mut Vec<Vec<Node>>,
    ) -> bool {
        while at < old.len() {
            self.steps += 1;
            if self.steps > MAX_STEPS {
                return false;
            }
            if let Some(width) = hole(old, at) {
                let before = fills.len();
                for len in 0..=new.len() {
                    fills.push(new[..len].to_vec());
                    if self.matches_from(old, at + width, &new[len..], outer, fills)
                        && fill_parses(old, at, width, outer, &new[..len])
                    {
                        return true;
                    }
                    fills.truncate(before);
                }
                return false;
            }
            let Some((first, rest)) = new.split_first() else { return false };
            let same = match (&old[at], first) {
                (Node::Group(d, a, _), Node::Group(e, b, _)) => d == e && self.matches(a, b, Some(*d), fills),
                (Node::Leaf(a, _), Node::Leaf(b, _)) => a == b,
                _ => false,
            };
            if !same {
                return false;
            }
            (at, new) = (at + 1, rest);
        }
        new.is_empty()
    }
}

fn count(node: &Node, text: &str) -> usize {
    match node {
        Node::Group(_, inner, _) => inner.iter().map(|n| count(n, text)).sum(),
        Node::Leaf(s, _) => (s == text) as usize,
    }
}

fn item_name(item: &TokenStream) -> String {
    let text: Vec<String> = nodes(item.clone()).iter().take(6).map(|n| n.token().to_string()).collect();
    text.join(" ").chars().take(60).collect()
}

/// A `verus!` item with the contents of the block removed.
fn shell(item: &TokenStream) -> TokenStream {
    let empty = |t| match t {
        TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
            TokenTree::Group(proc_macro2::Group::new(Delimiter::Brace, TokenStream::new()))
        }
        t => t,
    };
    item.clone().into_iter().map(empty).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::tests::experiment;

    const DEPS: &str = "use vstd::prelude::*; verus! { spec fn secret() -> bool { true } pub proof fn lem() {} }";
    const TASK: &str = r#"use vstd::prelude::*;
use crate::dependencies::*;
verus! {
spec fn even(x: int) -> bool { x % 2 == 0 }
/// Doc.
proof fn p(x: int)
    requires x > 1,
    ensures even(x + x),
{
    todo!("prove it")
}
fn count(n: u64) -> (r: u64)
    ensures r == n,
{
    let mut i: u64 = 0;
    while i < n
        invariant todo!("inv"),
        decreases n - i,
    {
        i = i + 1;
    }
    todo!("assert");
    i
}
}
"#;

    fn reasons(proposal: &str) -> Vec<String> {
        let experiment = experiment(TASK, DEPS, &["lem"], &[]);
        check(&experiment, &Index::build(&experiment).unwrap(), proposal)
    }

    /// The task with its three holes filled, and `extra` added inside `verus!`.
    fn filled(proof: &str, invariant: &str, after: &str, extra: &str) -> String {
        TASK.replace("todo!(\"prove it\")", proof)
            .replace("todo!(\"inv\")", invariant)
            .replace("todo!(\"assert\");", after)
            .replacen("verus! {", &format!("verus! {{ {extra}"), 1)
    }

    fn rejected(proposal: &str, why: &str) {
        let reasons = reasons(proposal);
        assert!(reasons.iter().any(|r| r.contains(why)), "wanted {why:?}, got {reasons:?} for {proposal}");
    }

    #[test]
    fn accepts_good_proposals() {
        for (proof, invariant, after, extra) in [
            ("", "i <= n", "", ""),
            ("lem();", "i <= n, i >= 0", "assert(i == n);", ""),
            ("assert(even(x + x)) by (nonlinear_arith);", "i <= n", "assert(true); assert(i == n);", ""),
            ("helper(x);", "i <= n", "", "proof fn helper(x: int) ensures true { lem(); }"),
            ("broadcast use vstd::seq_lib::group_seq_properties;", "i <= n", "", ""),
            ("let s = seq![1int]; if !(x > 1) { } calc! { (==) x; {} x; }", "i <= n", "", ""),
            (
                "assert forall|y: int| #![trigger even(y)] even(y) implies #[trigger] even(y + 2) by {}",
                "i <= n",
                "",
                "#[verifier::rlimit(20)] spec fn f2(x: nat) -> nat decreases x { if x == 0 { 0 } else { f2((x - 1) as nat) } }",
            ),
        ] {
            let proposal = filled(proof, invariant, after, extra);
            assert_eq!(reasons(&proposal), Vec::<String>::new(), "{proposal}");
        }
        // Comments and docs may change.
        assert_eq!(reasons(&filled("", "i <= n", "", "").replace("/// Doc.", "// hi")), Vec::<String>::new());
    }

    #[test]
    fn rejects_changes_to_the_task() {
        let good = filled("", "i <= n", "", "");
        for (from, to) in [
            ("even(x + x)", "true"),
            ("x > 1", "false"),
            ("x % 2 == 0", "true"),
            ("ensures r == n,", ""),
            ("i = i + 1;", "i = n;"),
            ("decreases n - i,", ""),
            ("proof fn p", "broadcast proof fn p"),
            ("proof fn p", "#[verifier::external_body] proof fn p"),
            ("spec fn even(x: int) -> bool { x % 2 == 0 }", ""),
            ("use crate::dependencies::*;", ""),
            ("use crate::dependencies::*;", "use crate::dependencies::*; use vstd::pervasive::*;"),
            ("verus! { \nspec", "verus! { #![allow(x)]\nspec"),
            ("verus! { \nspec", "#[cfg(any())] verus! {\nspec"),
        ] {
            assert!(good.contains(from), "{from}");
            assert!(!reasons(&good.replacen(from, to, 1)).is_empty(), "{from} -> {to}");
        }
        rejected(&format!("{good} fn evil() {{}}"), "outside `verus!`");
        rejected(&format!("{good} verus! {{ }}"), "outside `verus!`");
        rejected(&format!("#!/bin/sh\n{good}"), "parse");
        let reordered = good.replacen("spec fn even(x: int) -> bool { x % 2 == 0 }", "", 1);
        rejected(&reordered.replace("fn count", "spec fn even(x: int) -> bool { x % 2 == 0 } fn count"), "even");
    }

    #[test]
    fn rejects_fills_that_escape_their_hole() {
        rejected(&filled("", "i <= n, decreases n - i { i = i + 1; } while i < n invariant i <= n", "", ""), "");
        rejected(&filled("", "i <= n, ensures false", "", ""), "");
        rejected(&filled("", "", "", ""), "");
        rejected(&filled("fn inner() {}", "i <= n", "", ""), "define a function");
        rejected(&filled("use vstd::pervasive::*;", "i <= n", "", ""), "`use`");
        rejected(&filled("const X: u64 = 1;", "i <= n", "", ""), "`const`");
    }

    #[test]
    fn fills_cannot_regroup_their_surroundings() {
        let accepts = |task: &str, proposal: &str| {
            let experiment = experiment(task, "", &[], &[]);
            check(&experiment, &Index::build(&experiment).unwrap(), proposal).is_empty()
        };
        for task in [
            "verus! { proof fn p() ensures 1 == 2 && todo!(\"x\") { } }",
            "verus! { proof fn p() ensures todo!(\"x\") && 1 == 2 { } }",
            "verus! { spec fn s() -> bool { todo!(\"x\") && 1 == 2 } }",
            "verus! { spec fn s() -> bool { 1 == todo!(\"x\") } }",
        ] {
            assert!(!accepts(task, &task.replace("todo!(\"x\")", "true || true")), "{task}");
            assert!(accepts(task, &task.replace("todo!(\"x\")", "(true || true)")), "{task}");
        }
        let task = "verus! { fn f() { let x: u64 = todo!(\"x\"); } }";
        assert!(accepts(task, &task.replace("todo!(\"x\")", "1 + 2")));
    }

    #[test]
    fn rejects_unfilled_holes() {
        rejected(TASK, "hole is not filled");
        rejected(&filled("", "i <= n", "r#todo!();", ""), "hole is not filled");
    }

    #[test]
    fn rejects_bad_additions() {
        for (extra, why) in [
            ("spec fn lem() -> bool { true }", "reuses that name"),
            ("spec fn int() -> bool { true }", "reuses that name"),
            ("spec fn dependencies() -> bool { true }", "reuses that name"),
            ("spec fn secret() -> bool { true }", "not available"),
            ("proof fn z() ensures false;", "no body"),
            ("proof fn z() { fn y() {} }", "another function"),
            ("axiom fn z() ensures false;", "`axiom`"),
            ("uninterp spec fn z() -> int;", "`uninterp`"),
            ("struct Z;", "only functions"),
            ("use vstd::pervasive::*;", "only functions"),
            ("impl Seq { }", "only functions"),
            ("trait Z { }", "only functions"),
            ("type Z = int;", "only functions"),
            ("global size_of usize == 8;", "only functions"),
            ("macro_rules! assert { () => {} }", "only functions"),
            ("#[verifier::external_body] proof fn z() ensures false {}", "external_body"),
            ("#[verifier(external_body)] proof fn z() ensures false {}", "external_body"),
            ("#[verifier::exec_allows_no_decreases_clause] fn z() { loop {} }", "exec_allows"),
            ("#[cfg(any())] proof fn z() {}", "attribute"),
            ("#[path = \"/x\"] proof fn z() {}", "attribute"),
            ("unsafe fn z() {}", "`unsafe`"),
        ] {
            rejected(&filled("", "i <= n", "", extra), why);
        }
    }

    #[test]
    fn rejects_banned_names_anywhere() {
        for (proof, why) in [
            ("assume(false);", "`assume`"),
            ("r#assume(false);", "`assume`"),
            ("vstd::pervasive::assume(false);", "`assume`"),
            ("admit();", "`admit`"),
            ("calc! { (==) x; { assume(false); } x; }", "`assume`"),
            ("assert(false) by { admit(); }", "`admit`"),
            ("let t = Tracked::<int>::assume_new();", "`assume_new`"),
            ("assert(secret());", "not available"),
            ("assert(crate::dependencies::secret());", "not available"),
            ("seq![r#secret()];", "not available"),
            ("std::include!(\"/etc/passwd\");", "`include`"),
            ("let x = include_str!(\"/etc/passwd\");", "`include_str`"),
            ("panic!();", "macro `panic!`"),
            ("concat!(\"a\");", "macro `concat!`"),
            ("#[verifier::assume_termination] loop {}", "attribute"),
            ("verus_builtin::inline_air_stmt(\"(assume false)\");", "verus_builtin"),
        ] {
            rejected(&filled(proof, "i <= n", "", ""), why);
        }
        rejected(&filled("", "(assume(false) == ())", "", ""), "`assume`");
    }

    #[test]
    fn survives_hostile_input() {
        rejected(&"x".repeat(MAX_BYTES + 1), "larger");
        rejected(&"(".repeat(5000), "nested");
        let deep = filled(&format!("let a = {}true;", "!".repeat(3_000)), "i <= n", "", "");
        assert_eq!(reasons(&deep), Vec::<String>::new());
        rejected("verus! { fn", "parse");
    }
}
