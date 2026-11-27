//! Rust のソースを構文として読み、規則が使う事実を集める。

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::visit::{self, Visit};

/// 一つのファイルから集めた事実。行番号は 1 始まり。
#[derive(Debug, Default)]
pub struct Scan {
    skip_tests: bool,
    /// 現れた識別子 (型・関数・変数・パスの要素。マクロの引数の中も含む)。
    pub idents: Vec<(String, usize)>,
    /// 呼び出したマクロの名前。
    pub macros: Vec<(String, usize)>,
    /// `static` 項目。
    pub statics: Vec<usize>,
    /// 無制限に公開されたフィールド (構造体の名前)。
    pub pub_fields: Vec<(String, usize)>,
    /// `_` だけのパターンを持つ `match` の腕。
    pub wild_arms: Vec<usize>,
    /// 終わりの無い範囲 (`a..`)。
    pub open_ranges: Vec<usize>,
    /// `impl Kernel` の、公開されていて `&mut self` をとるメソッド。
    pub kernel_ops: Vec<(String, usize)>,
    /// メソッド呼び出しの名前。
    pub calls: Vec<String>,
    /// 列挙型の名前と、その変種の名前。
    pub variants: Vec<(String, String)>,
    /// `allow` / `expect` / `warn` 属性 (`cfg_attr` の中も含む)。
    pub lint_attrs: Vec<(String, usize)>,
    /// 関数の名前と、付いている属性のパス。
    pub fns: Vec<(String, Vec<String>)>,
    /// 文字列リテラルの腕を持つ `match` の対応: (パターンの最後の識別子, 文字列)。
    pub string_arms: Vec<(String, String)>,
}

/// `text` を読む。`skip_tests` なら `#[cfg(test)]` と `#[cfg(kani)]` の項目の中は見ない。
pub fn scan(text: &str, skip_tests: bool) -> Result<Scan, String> {
    let file = syn::parse_file(text).map_err(|error| error.to_string())?;
    let mut found = Scan { skip_tests, ..Scan::default() };
    found.visit_file(&file);
    Ok(found)
}

fn line_of(span: proc_macro2::Span) -> usize {
    span.start().line
}

/// `#[cfg(test)]` / `#[cfg(kani)]` (否定を含まないもの) が付いているか。
fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &attr.meta else { return false };
        let mut names = Vec::new();
        idents_in(list.tokens.clone(), &mut names);
        names.iter().any(|(name, _)| name == "test" || name == "kani") && !names.iter().any(|(name, _)| name == "not")
    })
}

fn idents_in(tokens: TokenStream, out: &mut Vec<(String, usize)>) {
    for tree in tokens {
        match tree {
            TokenTree::Ident(ident) => out.push((ident.to_string(), line_of(ident.span()))),
            TokenTree::Group(group) => idents_in(group.stream(), out),
            TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
    }
}

/// トークン列の中の `#[allow(..)]` の類を探す (マクロの定義の中は構文木にならない)。
fn lint_attrs_in(tokens: TokenStream, out: &mut Vec<(String, usize)>) {
    let mut after_hash = false;
    for tree in tokens {
        match tree {
            TokenTree::Punct(punct) if punct.as_char() == '#' || (after_hash && punct.as_char() == '!') => {
                after_hash = true;
            }
            TokenTree::Group(group) => {
                if after_hash && group.delimiter() == Delimiter::Bracket {
                    let mut names = Vec::new();
                    idents_in(group.stream(), &mut names);
                    let head = names.first().map(|(name, _)| name.as_str());
                    let through_cfg = head == Some("cfg_attr");
                    for (name, line) in &names {
                        let is_level = matches!(name.as_str(), "allow" | "expect" | "warn");
                        if is_level && (through_cfg || head == Some(name.as_str())) {
                            out.push((name.clone(), *line));
                        }
                    }
                }
                lint_attrs_in(group.stream(), out);
                after_hash = false;
            }
            TokenTree::Punct(_) | TokenTree::Ident(_) | TokenTree::Literal(_) => after_hash = false,
        }
    }
}

fn last_ident(tokens: TokenStream) -> Option<String> {
    let mut names = Vec::new();
    idents_in(tokens, &mut names);
    names.pop().map(|(name, _)| name)
}

macro_rules! skipping {
    ($($method:ident($node:ty) => $walk:path;)*) => {$(
        fn $method(&mut self, node: &'ast $node) {
            if self.skip_tests && test_only(&node.attrs) {
                return;
            }
            $walk(self, node);
        }
    )*};
}

impl<'ast> Visit<'ast> for Scan {
    skipping! {
        visit_item_mod(syn::ItemMod) => visit::visit_item_mod;
        visit_item_const(syn::ItemConst) => visit::visit_item_const;
        visit_item_use(syn::ItemUse) => visit::visit_item_use;
        visit_item_type(syn::ItemType) => visit::visit_item_type;
        visit_item_trait(syn::ItemTrait) => visit::visit_item_trait;
        visit_item_macro(syn::ItemMacro) => visit::visit_item_macro;
        visit_impl_item_const(syn::ImplItemConst) => visit::visit_impl_item_const;
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if self.skip_tests && test_only(&node.attrs) {
            return;
        }
        self.note_fn(&node.sig.ident, &node.attrs);
        visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if self.skip_tests && test_only(&node.attrs) {
            return;
        }
        self.note_fn(&node.sig.ident, &node.attrs);
        visit::visit_impl_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if self.skip_tests && test_only(&node.attrs) {
            return;
        }
        let is_kernel = matches!(&*node.self_ty, syn::Type::Path(path) if path.path.is_ident("Kernel"));
        if is_kernel && node.trait_.is_none() {
            for item in &node.items {
                let syn::ImplItem::Fn(method) = item else { continue };
                if self.skip_tests && test_only(&method.attrs) {
                    continue;
                }
                let public = matches!(method.vis, syn::Visibility::Public(_));
                let mutates = method.sig.receiver().is_some_and(|receiver| receiver.mutability.is_some());
                if public && mutates {
                    self.kernel_ops.push((method.sig.ident.to_string(), line_of(method.sig.ident.span())));
                }
            }
        }
        visit::visit_item_impl(self, node);
    }

    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        if self.skip_tests && test_only(&node.attrs) {
            return;
        }
        self.statics.push(line_of(node.ident.span()));
        visit::visit_item_static(self, node);
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        if self.skip_tests && test_only(&node.attrs) {
            return;
        }
        for field in &node.fields {
            if matches!(field.vis, syn::Visibility::Public(_)) {
                let line = field.ident.as_ref().map_or_else(|| line_of(node.ident.span()), |ident| line_of(ident.span()));
                self.pub_fields.push((node.ident.to_string(), line));
            }
        }
        visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if self.skip_tests && test_only(&node.attrs) {
            return;
        }
        for variant in &node.variants {
            self.variants.push((node.ident.to_string(), variant.ident.to_string()));
        }
        visit::visit_item_enum(self, node);
    }

    fn visit_ident(&mut self, node: &'ast proc_macro2::Ident) {
        self.idents.push((node.to_string(), line_of(node.span())));
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if let Some(last) = node.path.segments.last() {
            self.macros.push((last.ident.to_string(), line_of(last.ident.span())));
        }
        idents_in(node.tokens.clone(), &mut self.idents);
        lint_attrs_in(node.tokens.clone(), &mut self.lint_attrs);
        visit::visit_macro(self, node);
    }

    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        let line = node.path().segments.first().map_or(1, |segment| line_of(segment.ident.span()));
        for level in ["allow", "expect", "warn"] {
            if node.path().is_ident(level) {
                self.lint_attrs.push((level.to_owned(), line));
            }
        }
        if node.path().is_ident("cfg_attr")
            && let syn::Meta::List(list) = &node.meta
        {
            let mut names = Vec::new();
            idents_in(list.tokens.clone(), &mut names);
            for (name, line) in names {
                if matches!(name.as_str(), "allow" | "expect" | "warn") {
                    self.lint_attrs.push((name, line));
                }
            }
        }
        visit::visit_attribute(self, node);
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if let syn::Pat::Wild(wild) = &node.pat {
            self.wild_arms.push(line_of(wild.underscore_token.span));
        }
        if let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(text), .. }) = &*node.body
            && let Some(name) = last_ident(quote_pat(&node.pat))
        {
            self.string_arms.push((name, text.value()));
        }
        visit::visit_arm(self, node);
    }

    fn visit_expr_range(&mut self, node: &'ast syn::ExprRange) {
        if node.start.is_some() && node.end.is_none() {
            let span = match &node.limits {
                syn::RangeLimits::HalfOpen(dots) => dots.spans[0],
                syn::RangeLimits::Closed(dots) => dots.spans[0],
            };
            self.open_ranges.push(line_of(span));
        }
        visit::visit_expr_range(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.calls.push(node.method.to_string());
        visit::visit_expr_method_call(self, node);
    }
}

/// パターンが単純なパス (`Invariant::RunQueue`) なら、そのトークン列。
fn quote_pat(pat: &syn::Pat) -> TokenStream {
    let mut out = TokenStream::new();
    if let syn::Pat::Path(path) = pat {
        for segment in &path.path.segments {
            out.extend([TokenTree::Ident(segment.ident.clone())]);
        }
    }
    out
}

impl Scan {
    fn note_fn(&mut self, name: &proc_macro2::Ident, attrs: &[syn::Attribute]) {
        let paths = attrs
            .iter()
            .map(|attr| attr.path().segments.iter().map(|s| s.ident.to_string()).collect::<Vec<_>>().join("::"))
            .collect();
        self.fns.push((name.to_string(), paths));
    }
}
