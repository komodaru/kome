//! 同じコードを二度書いていないかを調べる (規則 `dup`)。
//!
//! 比べる単位と、同じと見なす条件は次のとおり。どれも、名前を付けて一か所に置けば済むものである。
//!
//! | 種類 | 単位 | 同じと見なす条件 |
//! | --- | --- | --- |
//! | `exact` | 関数 (自由な関数、メソッド、trait の既定の実装、入れ子の関数) | 自分の名前と文書の注釈を除いた字句の列が同じ |
//! | `renamed` | 関数 | 局所の名前を現れた順の番号に置き換えた字句の列が同じ (変数の名前だけを変えた写し) |
//! | `table` | 配列の定数 (`const` / `static`) | 型と要素の字句の列が同じ |
//! | `run` | 一つのブロックの中の、続いた文 | 局所の名前を番号に置き換えた字句の列が同じ |
//! | `text` | 文字列リテラル | 内容が同じ |
//! | `lines` | シェルスクリプトと CI の定義 (`.sh`、`.yml`) の続いた行 | 空行、注釈、決まり文句を除いた行の列が同じ |
//!
//! 試験の中の `run` は、[`TRIAL_RUN`] 字句に届く列だけを数える。試験の最初の数行 (状態を作る、
//! プロセスとスレッドを置く) は、試験ごとに上から読めるほうがよい。関数まるごとの写しは、試験でも数える。
//!
//! 短いものは数えない ([`Kind::least`])。値を返すだけの関数や、二つのフィールドを詰めるだけの
//! `new` が似るのは偶然で、写しではない。
//!
//! 見つけた組のうち、意図して二つ持つもの (試験が実装と独立に持つ答え、など) は
//! `docs/dup-known.tsv` に理由を添えて書く。一覧は自分では増えない: 載っていない組は違反で、
//! 載っているのに見つからなくなった行も違反である。

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

/// 試験の中の続いた文を写しに数える、最小の字句の数。
pub const TRIAL_RUN: usize = 80;

/// 意図して持つ写しの一覧。
pub const KNOWN: &str = "docs/dup-known.tsv";

/// 写しの種類。並びは、報告の優先の順 (前の種類の組に含まれる後の種類の組は、報告しない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// 字句の列が同じ関数。
    Exact,
    /// 局所の名前だけが違う関数。
    Renamed,
    /// 同じ配列の定数。
    Table,
    /// 続いた文の写し。
    Run,
    /// 同じ文字列リテラル。
    Text,
    /// シェルスクリプトと CI の定義の、続いた行の写し。
    Lines,
}

impl Kind {
    /// 全ての種類。
    pub const ALL: [Kind; 6] = [Kind::Exact, Kind::Renamed, Kind::Table, Kind::Run, Kind::Text, Kind::Lines];

    /// 数える最小の大きさ。関数、配列、文は字句の数、文字列は文字の数、スクリプトは行の数。
    #[must_use]
    pub const fn least(self) -> usize {
        match self {
            Kind::Exact => 50,
            Kind::Renamed => 70,
            Kind::Table => 48,
            Kind::Run => 55,
            Kind::Text => 48,
            Kind::Lines => 4,
        }
    }

    /// 一覧に書く名前。
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Kind::Exact => "exact",
            Kind::Renamed => "renamed",
            Kind::Table => "table",
            Kind::Run => "run",
            Kind::Text => "text",
            Kind::Lines => "lines",
        }
    }

    fn of_id(id: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.id() == id)
    }
}

/// 写しの一つ: ファイルと、その中の名前 (メソッドは `型::名前`。文、文字列、行は、それを含む関数の名前)。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Member {
    /// ファイル。
    pub file: String,
    /// 名前。
    pub name: String,
    /// 始まりの行。
    pub first: usize,
    /// 終わりの行。
    pub last: usize,
}

impl Member {
    fn site(&self) -> String {
        format!("{}#{}", self.file, self.name)
    }

    fn holds(&self, other: &Member) -> bool {
        self.file == other.file && self.first <= other.first && other.last <= self.last
    }

    fn overlaps(&self, other: &Member) -> bool {
        self.file == other.file && self.first <= other.last && other.first <= self.last
    }
}

/// 同じ内容の集まり。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// 種類。
    pub kind: Kind,
    /// 写し。二つ以上で、ファイルと行の順。
    pub members: Vec<Member>,
    /// 大きさ ([`Kind::least`] と同じ単位)。
    pub size: usize,
}

struct Unit {
    kind: Kind,
    member: Member,
    key: Vec<String>,
    size: usize,
}

/// `files` (パスと内容) の中の写しの組。構文として読めない Rust のファイルは見ない (コンパイラが報告する)。
#[must_use]
pub fn find<'a>(files: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<Group> {
    let mut units = Vec::new();
    for (path, text) in files {
        if path.ends_with(".rs") {
            if let Ok(file) = syn::parse_file(text) {
                Collect { file: path, owner: Vec::new(), names: Vec::new(), trial: usize::from(trial_file(path)), out: &mut units }.visit_file(&file);
            }
        } else if path.ends_with(".sh") || path.ends_with(".yml") {
            script_runs(path, text, &mut units);
        }
    }
    let mut same: BTreeMap<(Kind, &[String]), Vec<&Unit>> = BTreeMap::new();
    for unit in &units {
        same.entry((unit.kind, unit.key.as_slice())).or_default().push(unit);
    }
    let mut found: Vec<Group> = Vec::new();
    for ((kind, _), units) in same {
        let mut members: Vec<Member> = units.iter().map(|unit| unit.member.clone()).collect();
        members.sort();
        // 自分自身と重なる写し (同じ文が続いているだけ) は、一つと数える。
        let mut apart: Vec<Member> = Vec::new();
        for member in members {
            if !apart.iter().any(|kept| kept.overlaps(&member)) {
                apart.push(member);
            }
        }
        let size = units.first().map_or(0, |unit| unit.size);
        if apart.len() >= 2 {
            found.push(Group { kind, members: apart, size });
        }
    }
    // 種類の順、次に場所の順に並べ、既に報告する組に含まれる (文と行は、重なる) 組を落とす。
    found.sort_by(|a, b| (a.kind, &a.members).cmp(&(b.kind, &b.members)));
    let mut groups: Vec<Group> = Vec::new();
    for group in found {
        let covered = groups.iter().any(|kept| {
            group.members.iter().all(|member| {
                kept.members.iter().any(|wider| match group.kind {
                    Kind::Run | Kind::Lines => wider.overlaps(member),
                    Kind::Exact | Kind::Renamed | Kind::Table | Kind::Text => wider.holds(member),
                })
            })
        });
        if !covered {
            groups.push(group);
        }
    }
    groups
}

struct Collect<'a, 'u> {
    file: &'a str,
    /// 今いる `impl` / `trait`。
    owner: Vec<String>,
    /// 今いる関数 (無ければ、項目の名前)。
    names: Vec<String>,
    /// 今いる試験の入れ子の数 (試験のファイル、`#[cfg(test)]` のモジュール、試験や証明の関数)。
    trial: usize,
    out: &'u mut Vec<Unit>,
}

/// 試験だけが入るファイルか。
fn trial_file(path: &str) -> bool {
    path.contains("/tests/") || path.contains("/benches/") || path.ends_with("/tests.rs") || path.ends_with("/proofs.rs")
}

/// 試験か証明の印 (`#[test]`、`#[cfg(test)]`、`#[cfg(kani)]`、`#[kani::proof]`) が付いているか。
fn trial_marked(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        let path = squeezed(attribute.path());
        let inner = attribute.meta.require_list().map_or_else(|_| String::new(), |list| squeezed(&list.tokens));
        path == "test" || path == "kani::proof" || (path == "cfg" && (inner == "test" || inner == "kani"))
    })
}

fn squeezed(tokens: &impl ToTokens) -> String {
    tokens.to_token_stream().to_string().replace(' ', "")
}

impl Collect<'_, '_> {
    fn member(&self, name: &str, span: proc_macro2::Span) -> Member {
        Member { file: self.file.to_owned(), name: name.to_owned(), first: span.start().line, last: span.end().line }
    }

    fn named(&self, name: &syn::Ident) -> String {
        self.owner.last().map_or_else(|| name.to_string(), |owner| format!("{owner}::{name}"))
    }

    fn function(&mut self, signature: &syn::Signature, block: &syn::Block, whole: proc_macro2::Span) {
        let mut unnamed = signature.clone();
        unnamed.ident = syn::Ident::new("_", signature.ident.span());
        let mut tokens = unnamed.to_token_stream();
        block.to_tokens(&mut tokens);
        let exact = flat(tokens);
        let member = self.member(&self.named(&signature.ident), whole);
        let renamed = renamed(&exact);
        if exact.len() >= Kind::Exact.least() {
            self.out.push(Unit { kind: Kind::Exact, member: member.clone(), size: exact.len(), key: exact });
        }
        if renamed.len() >= Kind::Renamed.least() {
            self.out.push(Unit { kind: Kind::Renamed, member, size: renamed.len(), key: renamed });
        }
    }

    fn table(&mut self, name: &syn::Ident, ty: &syn::Type, value: &syn::Expr, whole: proc_macro2::Span) {
        let syn::Expr::Array(array) = value else { return };
        let mut tokens = ty.to_token_stream();
        value.to_tokens(&mut tokens);
        let key = flat(tokens);
        if array.elems.len() >= 8 && key.len() >= Kind::Table.least() {
            self.out.push(Unit { kind: Kind::Table, member: self.member(&name.to_string(), whole), size: key.len(), key });
        }
    }

    fn inside<R>(&mut self, name: String, visit: impl FnOnce(&mut Self) -> R) -> R {
        self.names.push(name);
        let out = visit(self);
        self.names.pop();
        out
    }

    /// 印が付いた項目の中を、試験として見る。
    fn marked<R>(&mut self, attributes: &[syn::Attribute], visit: impl FnOnce(&mut Self) -> R) -> R {
        let trial = usize::from(trial_marked(attributes));
        self.trial = self.trial.saturating_add(trial);
        let out = visit(self);
        self.trial = self.trial.saturating_sub(trial);
        out
    }

    fn here(&self) -> String {
        self.names.last().cloned().unwrap_or_else(|| "(項目の外)".to_owned())
    }
}

impl<'a> Visit<'a> for Collect<'_, '_> {
    fn visit_item_fn(&mut self, item: &'a syn::ItemFn) {
        self.function(&item.sig, &item.block, item.span());
        self.marked(&item.attrs, |me| me.inside(me.named(&item.sig.ident), |me| visit::visit_item_fn(me, item)));
    }

    fn visit_impl_item_fn(&mut self, item: &'a syn::ImplItemFn) {
        self.function(&item.sig, &item.block, item.span());
        self.inside(self.named(&item.sig.ident), |me| visit::visit_impl_item_fn(me, item));
    }

    fn visit_trait_item_fn(&mut self, item: &'a syn::TraitItemFn) {
        if let Some(block) = &item.default {
            self.function(&item.sig, block, item.span());
        }
        self.inside(self.named(&item.sig.ident), |me| visit::visit_trait_item_fn(me, item));
    }

    fn visit_item_mod(&mut self, item: &'a syn::ItemMod) {
        self.marked(&item.attrs, |me| visit::visit_item_mod(me, item));
    }

    fn visit_item_impl(&mut self, item: &'a syn::ItemImpl) {
        let ty = squeezed(&item.self_ty);
        self.owner.push(item.trait_.as_ref().map_or_else(|| ty.clone(), |(_, path, _)| format!("<{ty}as{}>", squeezed(path))));
        visit::visit_item_impl(self, item);
        self.owner.pop();
    }

    fn visit_item_trait(&mut self, item: &'a syn::ItemTrait) {
        self.owner.push(item.ident.to_string());
        visit::visit_item_trait(self, item);
        self.owner.pop();
    }

    fn visit_item_const(&mut self, item: &'a syn::ItemConst) {
        self.table(&item.ident, &item.ty, &item.expr, item.span());
        self.inside(item.ident.to_string(), |me| visit::visit_item_const(me, item));
    }

    fn visit_item_static(&mut self, item: &'a syn::ItemStatic) {
        self.table(&item.ident, &item.ty, &item.expr, item.span());
        self.inside(item.ident.to_string(), |me| visit::visit_item_static(me, item));
    }

    fn visit_block(&mut self, block: &'a syn::Block) {
        let parts: Vec<(Vec<String>, proc_macro2::Span)> =
            block.stmts.iter().map(|stmt| (flat(stmt.to_token_stream()), stmt.span())).collect();
        let least = if self.trial > 0 { TRIAL_RUN } else { Kind::Run.least() };
        // それぞれの文から始めて、最小の大きさに届くまで後ろの文を足した列。
        for (at, (_, first)) in parts.iter().enumerate() {
            let mut tokens: Vec<String> = Vec::new();
            let mut last = *first;
            for (more, span) in parts.iter().skip(at) {
                tokens.extend(more.iter().cloned());
                last = *span;
                if tokens.len() >= least {
                    break;
                }
            }
            if tokens.len() >= least {
                let member = Member { file: self.file.to_owned(), name: self.here(), first: first.start().line, last: last.end().line };
                let key = renamed(&tokens);
                self.out.push(Unit { kind: Kind::Run, member, size: key.len(), key });
            }
        }
        visit::visit_block(self, block);
    }

    /// `///` と `//!` は注釈で、コードではない。説明の文が似るのは写しに数えない。
    fn visit_attribute(&mut self, attribute: &'a syn::Attribute) {
        if !attribute.path().is_ident("doc") {
            visit::visit_attribute(self, attribute);
        }
    }

    fn visit_lit_str(&mut self, text: &'a syn::LitStr) {
        let value = text.value();
        let size = value.chars().count();
        if size >= Kind::Text.least() {
            let member = self.member(&self.here(), text.span());
            self.out.push(Unit { kind: Kind::Text, member, size, key: vec![value] });
        }
    }

    fn visit_macro(&mut self, call: &'a syn::Macro) {
        // マクロの引数は構文として読まれない。文字列リテラルだけを字句から拾う。
        literals(call.tokens.clone(), &mut |text| self.visit_lit_str(&text));
        visit::visit_macro(self, call);
    }
}

fn literals(tokens: TokenStream, found: &mut impl FnMut(syn::LitStr)) {
    for token in tokens {
        match token {
            TokenTree::Group(group) => literals(group.stream(), found),
            TokenTree::Literal(literal) => {
                if let Ok(text) = syn::parse2::<syn::LitStr>(TokenTree::Literal(literal).into()) {
                    found(text);
                }
            }
            TokenTree::Ident(_) | TokenTree::Punct(_) => {}
        }
    }
}

/// シェルスクリプトと CI の定義の、続いた行の列。空行、注釈、決まり文句は行に数えない。
/// 決まり文句は、殻の構文 (`set -euo pipefail`、`fi` など) と、ジョブごとに書くほかない
/// ワークフローの枠 (`steps:`、`runs-on:`、`timeout-minutes:`、checkout) である。
fn script_runs(path: &str, text: &str, out: &mut Vec<Unit>) {
    let whole = ["set -euo pipefail", "cd \"$(dirname \"$0\")/../..\"", "fi", "done", "else", "then", "}", "{", "esac", ";;", "steps:", "with:"];
    let starts = ["runs-on: ", "timeout-minutes: ", "- uses: actions/checkout@"];
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(at, line)| (at.saturating_add(1), line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .filter(|(_, line)| !whole.contains(line) && !starts.iter().any(|start| line.starts_with(start)))
        .collect();
    for window in lines.windows(Kind::Lines.least()) {
        let (Some((first, opening)), Some((last, _))) = (window.first(), window.last()) else { continue };
        // 一覧では、列の最初の行で場所を指す。
        let member = Member { file: path.to_owned(), name: (*opening).to_owned(), first: *first, last: *last };
        let key: Vec<String> = window.iter().map(|(_, line)| (*line).to_owned()).collect();
        out.push(Unit { kind: Kind::Lines, member, size: key.len(), key });
    }
}

/// 字句を一つずつ並べる (かっこは、開きと閉じの字句にする)。文書の注釈は除く。
#[must_use]
pub fn flat(tokens: TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    flat_into(tokens, &mut out);
    out
}

fn flat_into(tokens: TokenStream, out: &mut Vec<String>) {
    let all: Vec<TokenTree> = tokens.into_iter().collect();
    let mut at = 0usize;
    while let Some(token) = all.get(at) {
        at = at.saturating_add(1);
        match token {
            TokenTree::Punct(punct) if punct.as_char() == '#' => {
                // `#[doc = ".."]` と `#![doc = ".."]` は注釈で、コードではない。
                let (inner, group_at) = match all.get(at) {
                    Some(TokenTree::Punct(bang)) if bang.as_char() == '!' => (true, at.saturating_add(1)),
                    Some(TokenTree::Punct(_) | TokenTree::Group(_) | TokenTree::Ident(_) | TokenTree::Literal(_)) | None => (false, at),
                };
                let doc = match all.get(group_at) {
                    Some(TokenTree::Group(group)) => {
                        group.delimiter() == Delimiter::Bracket
                            && group.stream().into_iter().next().is_some_and(|first| first.to_string() == "doc")
                    }
                    Some(TokenTree::Punct(_) | TokenTree::Ident(_) | TokenTree::Literal(_)) | None => false,
                };
                if doc {
                    at = group_at.saturating_add(1);
                    continue;
                }
                out.push("#".to_owned());
                if inner {
                    out.push("!".to_owned());
                    at = group_at;
                }
            }
            TokenTree::Group(group) => {
                let (open, close) = match group.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::None => ("", ""),
                };
                if !open.is_empty() {
                    out.push(open.to_owned());
                }
                flat_into(group.stream(), out);
                if !close.is_empty() {
                    out.push(close.to_owned());
                }
            }
            TokenTree::Ident(ident) => out.push(ident.to_string()),
            TokenTree::Punct(punct) => out.push(punct.as_char().to_string()),
            TokenTree::Literal(literal) => out.push(literal.to_string()),
        }
    }
}

/// 局所の名前として数えない語 (予約語と、組み込みの型)。
const WORDS: [&str; 53] = [
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
    "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "bool", "char",
    "str", "_",
];

/// 局所の名前を、最初に現れた順の番号に置き換える。局所の名前とは、小文字で始まる識別子のうち、
/// 予約語でなく、呼び出されておらず (`f(`、`m!`)、`.` の後 (フィールド、メソッド) でも `::` の隣でもないもの。
#[must_use]
pub fn renamed(tokens: &[String]) -> Vec<String> {
    let mut numbers: BTreeMap<&str, usize> = BTreeMap::new();
    let token = |at: Option<usize>| at.and_then(|at| tokens.get(at)).map(String::as_str);
    let mut out = Vec::with_capacity(tokens.len());
    for (at, word) in tokens.iter().enumerate() {
        let (before, before2) = (token(at.checked_sub(1)), token(at.checked_sub(2)));
        let (after, after2) = (token(at.checked_add(1)), token(at.checked_add(2)));
        let local = word.chars().next().is_some_and(|first| first.is_ascii_lowercase() || first == '_')
            && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !WORDS.contains(&word.as_str())
            && !matches!(after, Some("(" | "!"))
            && before != Some(".")
            && !(before == Some(":") && before2 == Some(":"))
            && !(after == Some(":") && after2 == Some(":"));
        if local {
            let next = numbers.len();
            let number = *numbers.entry(word.as_str()).or_insert(next);
            out.push(format!("${number}"));
        } else {
            out.push(word.clone());
        }
    }
    out
}

/// 一覧の一行: 種類、写しの場所 (`ファイル#名前`)、理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    /// 一覧の中の行 (1 始まり)。
    pub line: usize,
    /// 種類。
    pub kind: Kind,
    /// 写しの場所。
    pub sites: BTreeSet<String>,
    /// 理由。
    pub why: String,
}

/// 一覧を読む。行は `種類<TAB>ファイル#名前<TAB>ファイル#名前…<TAB># 理由`。`#` で始まる行と空行は注釈。
/// 決まりに合わない行は、その行番号と理由を返す。
pub fn known(text: &str) -> Result<Vec<Known>, (usize, String)> {
    let mut out = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let number = at.saturating_add(1);
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cells = line.split('\t');
        let kind = cells.next().and_then(Kind::of_id).ok_or((number, "種類が exact / renamed / table / run / text / lines のどれでもない".to_owned()))?;
        let mut sites = BTreeSet::new();
        let mut why = String::new();
        for cell in cells {
            match cell.strip_prefix("# ") {
                Some(reason) => why = reason.trim().to_owned(),
                None if cell.contains('#') => {
                    sites.insert(cell.to_owned());
                }
                None => return Err((number, format!("場所は `ファイル#名前` と書く: {cell}"))),
            }
        }
        if sites.len() < 2 {
            return Err((number, "場所が二つ以上無い".to_owned()));
        }
        if why.chars().count() < 12 {
            return Err((number, "二つ持つ理由が書かれていない (`# 理由`)".to_owned()));
        }
        out.push(Known { line: number, kind, sites, why });
    }
    Ok(out)
}

/// 組が、一覧の行に載っているか (種類が同じで、全ての写しの場所がその行にある)。
#[must_use]
pub fn listed(group: &Group, entry: &Known) -> bool {
    entry.kind == group.kind && group.members.iter().all(|member| entry.sites.contains(&member.site()))
}

/// 組を一覧に書くときの行 (理由は書く人が埋める)。
#[must_use]
pub fn entry_line(group: &Group) -> String {
    let sites: BTreeSet<String> = group.members.iter().map(Member::site).collect();
    format!("{}\t{}\t# 理由", group.kind.id(), sites.into_iter().collect::<Vec<_>>().join("\t"))
}
