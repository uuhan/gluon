//! The parser is a bit more complex than it needs to be as it needs to be fully specialized to
//! avoid a recompilation every time a later part of the compiler is changed. Due to this the
//! string interner and therefore also garbage collector needs to compiled before the parser.
#![doc(html_root_url = "https://docs.rs/gluon_parser/0.12.0")] // # GLUON

extern crate codespan;
extern crate codespan_reporting;
extern crate collect_mac;
extern crate gluon_base as base;
extern crate itertools;
#[macro_use]
extern crate lalrpop_util;
#[macro_use]
extern crate log;
extern crate ordered_float;
extern crate pretty;
#[macro_use]
extern crate quick_error;

#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;

use std::{fmt, hash::Hash, sync::Arc};

use crate::base::{
    ast::{AstType, Do, Expr, IdentEnv, SpannedExpr, SpannedPattern, TypedIdent, ValueBinding},
    error::{AsDiagnostic, Errors},
    fnv::FnvMap,
    metadata::Metadata,
    pos::{self, ByteOffset, BytePos, Span, Spanned},
    symbol::Symbol,
    types::{ArcType, TypeCache},
};

use crate::{
    infix::{Fixity, OpMeta, OpTable, Reparser},
    layout::Layout,
    token::{Token, Tokenizer},
};

pub use crate::{
    infix::Error as InfixError, layout::Error as LayoutError, token::Error as TokenizeError,
};

lalrpop_mod!(
    #[cfg_attr(rustfmt, rustfmt_skip)]
    grammar
);

pub mod infix;
mod layout;
mod str_suffix;
mod token;

fn new_ident<Id>(type_cache: &TypeCache<Id, ArcType<Id>>, name: Id) -> TypedIdent<Id> {
    TypedIdent {
        name: name,
        typ: type_cache.hole(),
    }
}

type LalrpopError<'input> =
    lalrpop_util::ParseError<BytePos, Token<'input>, Spanned<Error, BytePos>>;

/// Shrink hidden spans to fit the visible expressions and flatten singleton blocks.
fn shrink_hidden_spans<Id>(mut expr: SpannedExpr<Id>) -> SpannedExpr<Id> {
    match expr.value {
        Expr::Infix { rhs: ref last, .. }
        | Expr::IfElse(_, _, ref last)
        | Expr::LetBindings(_, ref last)
        | Expr::TypeBindings(_, ref last)
        | Expr::Do(Do { body: ref last, .. }) => {
            expr.span = Span::new(expr.span.start(), last.span.end())
        }
        Expr::Lambda(ref lambda) => {
            expr.span = Span::new(expr.span.start(), lambda.body.span.end())
        }
        Expr::Block(ref mut exprs) => match exprs.len() {
            0 => (),
            1 => return exprs.pop().unwrap(),
            _ => expr.span = Span::new(expr.span.start(), exprs.last().unwrap().span.end()),
        },
        Expr::Match(_, ref alts) => {
            if let Some(last_alt) = alts.last() {
                let end = last_alt.expr.span.end();
                expr.span = Span::new(expr.span.start(), end);
            }
        }
        Expr::Annotated(..)
        | Expr::App { .. }
        | Expr::Ident(_)
        | Expr::Literal(_)
        | Expr::Projection(_, _, _)
        | Expr::Array(_)
        | Expr::Record { .. }
        | Expr::Tuple { .. }
        | Expr::MacroExpansion { .. }
        | Expr::Error(..) => (),
    }
    expr
}

fn transform_errors<'a, Iter>(
    source_span: Span<BytePos>,
    errors: Iter,
) -> Errors<Spanned<Error, BytePos>>
where
    Iter: IntoIterator<Item = LalrpopError<'a>>,
{
    errors
        .into_iter()
        .map(|err| Error::from_lalrpop(source_span, err))
        .collect()
}

struct Expected<'a>(&'a [String]);

impl<'a> fmt::Display for Expected<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0.len() {
            0 => (),
            1 => write!(f, "\nExpected ")?,
            _ => write!(f, "\nExpected one of ")?,
        }
        for (i, token) in self.0.iter().enumerate() {
            let sep = match i {
                0 => "",
                i if i + 1 < self.0.len() => ",",
                _ => " or",
            };
            write!(f, "{} {}", sep, token)?;
        }
        Ok(())
    }
}

quick_error! {
    #[derive(Debug, PartialEq)]
    pub enum Error {
        Token(err: TokenizeError) {
            description(err.description())
            display("{}", err)
            from()
        }
        Layout(err: LayoutError) {
            description(err.description())
            display("{}", err)
            from()
        }
        InvalidToken {
            description("invalid token")
            display("Invalid token")
        }
        UnexpectedToken(token: String, expected: Vec<String>) {
            description("unexpected token")
            display("Unexpected token: {}{}", token, Expected(&expected))
        }
        UnexpectedEof(expected: Vec<String>) {
            description("unexpected end of file")
            display("Unexpected end of file{}", Expected(&expected))
        }
        ExtraToken(token: String) {
            description("extra token")
            display("Extra token: {}", token)
        }
        Infix(err: InfixError) {
            description(err.description())
            display("{}", err)
            from()
        }
        Message(msg: String) {
            description(msg)
            display("{}", msg)
            from()
        }
    }
}

impl AsDiagnostic for Error {
    fn as_diagnostic(&self) -> codespan_reporting::Diagnostic {
        codespan_reporting::Diagnostic::new_error(self.to_string())
    }
}

/// LALRPOP currently has an unnecessary set of `"` around each expected token
fn remove_extra_quotes(tokens: &mut [String]) {
    for token in tokens {
        if token.starts_with('"') && token.ends_with('"') {
            token.remove(0);
            token.pop();
        }
    }
}

impl Error {
    fn from_lalrpop(source_span: Span<BytePos>, err: LalrpopError) -> Spanned<Error, BytePos> {
        use lalrpop_util::ParseError::*;

        match err {
            InvalidToken { location } => pos::spanned2(location, location, Error::InvalidToken),
            UnrecognizedToken {
                token: (lpos, token, rpos),
                mut expected,
            } => {
                remove_extra_quotes(&mut expected);
                pos::spanned2(
                    lpos,
                    rpos,
                    Error::UnexpectedToken(token.to_string(), expected),
                )
            }
            UnrecognizedEOF {
                location,
                mut expected,
            } => {
                // LALRPOP will use `Default::default()` as the location if it is unable to find
                // one. This is not correct for codespan as that represents "nil" so we must grab
                // the end from the current source instead
                let location = if location == BytePos::default() {
                    source_span.end()
                } else {
                    location
                };
                remove_extra_quotes(&mut expected);
                pos::spanned2(location, location, Error::UnexpectedEof(expected))
            }
            ExtraToken {
                token: (lpos, token, rpos),
            } => pos::spanned2(lpos, rpos, Error::ExtraToken(token.to_string())),
            User { error } => error,
        }
    }
}

pub enum FieldPattern<Id> {
    Type(Spanned<Id, BytePos>, Option<Id>),
    Value(Spanned<Id, BytePos>, Option<SpannedPattern<Id>>),
}

pub enum FieldExpr<Id> {
    Type(Metadata, Spanned<Id, BytePos>, Option<ArcType<Id>>),
    Value(Metadata, Spanned<Id, BytePos>, Option<SpannedExpr<Id>>),
}

pub enum Variant<Id> {
    Gadt(Id, AstType<Id>),
    Simple(Id, Vec<AstType<Id>>),
}

// Hack around LALRPOP's limited type syntax
type MutIdentEnv<'env, Id> = &'env mut dyn IdentEnv<Ident = Id>;
type ErrorEnv<'err, 'input> = &'err mut Errors<LalrpopError<'input>>;

pub type ParseErrors = Errors<Spanned<Error, BytePos>>;

pub trait ParserSource {
    fn src(&self) -> &str;
    fn start_index(&self) -> BytePos;

    fn span(&self) -> Span<BytePos> {
        let start = self.start_index();
        Span::new(start, start + ByteOffset::from(self.src().len() as i64))
    }
}

impl<'a, S> ParserSource for &'a S
where
    S: ?Sized + ParserSource,
{
    fn src(&self) -> &str {
        (**self).src()
    }
    fn start_index(&self) -> BytePos {
        (**self).start_index()
    }
}

impl ParserSource for str {
    fn src(&self) -> &str {
        self
    }
    fn start_index(&self) -> BytePos {
        BytePos::from(1)
    }
}

impl ParserSource for codespan::FileMap {
    fn src(&self) -> &str {
        codespan::FileMap::src(self)
    }
    fn start_index(&self) -> BytePos {
        codespan::FileMap::span(self).start()
    }
}

pub fn parse_partial_expr<Id, S>(
    symbols: &mut dyn IdentEnv<Ident = Id>,
    type_cache: &TypeCache<Id, ArcType<Id>>,
    input: &S,
) -> Result<SpannedExpr<Id>, (Option<SpannedExpr<Id>>, ParseErrors)>
where
    Id: Clone + AsRef<str>,
    S: ?Sized + ParserSource,
{
    let layout = Layout::new(Tokenizer::new(input));

    let mut parse_errors = Errors::new();

    let result =
        grammar::TopExprParser::new().parse(&input, type_cache, symbols, &mut parse_errors, layout);

    match result {
        Ok(expr) => {
            if parse_errors.has_errors() {
                Err((Some(expr), transform_errors(input.span(), parse_errors)))
            } else {
                Ok(expr)
            }
        }
        Err(err) => {
            parse_errors.push(err);
            Err((None, transform_errors(input.span(), parse_errors)))
        }
    }
}

pub fn parse_expr(
    symbols: &mut dyn IdentEnv<Ident = Symbol>,
    type_cache: &TypeCache<Symbol, ArcType>,
    input: &str,
) -> Result<SpannedExpr<Symbol>, ParseErrors> {
    parse_partial_expr(symbols, type_cache, input).map_err(|t| t.1)
}

#[derive(Debug, PartialEq)]
pub enum ReplLine<Id> {
    Expr(SpannedExpr<Id>),
    Let(ValueBinding<Id>),
}

pub fn parse_partial_repl_line<Id, S>(
    symbols: &mut dyn IdentEnv<Ident = Id>,
    input: &S,
) -> Result<Option<ReplLine<Id>>, (Option<ReplLine<Id>>, ParseErrors)>
where
    Id: Clone + Eq + Hash + AsRef<str> + ::std::fmt::Debug,
    S: ?Sized + ParserSource,
{
    let layout = Layout::new(Tokenizer::new(input));

    let mut parse_errors = Errors::new();

    let type_cache = TypeCache::default();

    let result = grammar::ReplLineParser::new().parse(
        &input,
        &type_cache,
        symbols,
        &mut parse_errors,
        layout,
    );

    match result {
        Ok(repl_line) => {
            let repl_line = repl_line.map(|b| *b);
            if parse_errors.has_errors() {
                Err((repl_line, transform_errors(input.span(), parse_errors)))
            } else {
                Ok(repl_line)
            }
        }
        Err(err) => {
            parse_errors.push(err);
            Err((None, transform_errors(input.span(), parse_errors)))
        }
    }
}

pub fn reparse_infix<Id>(
    metadata: &FnvMap<Id, Arc<Metadata>>,
    symbols: &dyn IdentEnv<Ident = Id>,
    expr: &mut SpannedExpr<Id>,
) -> Result<(), ParseErrors>
where
    Id: Clone + Eq + Hash + AsRef<str> + ::std::fmt::Debug,
{
    use crate::base::ast::{is_operator_char, walk_pattern, Pattern, Visitor};

    let mut errors = Errors::new();

    struct CheckInfix<'b, Id>
    where
        Id: 'b,
    {
        metadata: &'b FnvMap<Id, Arc<Metadata>>,
        errors: &'b mut Errors<Spanned<Error, BytePos>>,
        op_table: &'b mut OpTable<Id>,
    }

    impl<'b, Id> CheckInfix<'b, Id>
    where
        Id: Clone + Eq + Hash + AsRef<str>,
    {
        fn insert_infix(&mut self, id: &Id, span: Span<BytePos>) {
            match self
                .metadata
                .get(id)
                .and_then(|meta| meta.get_attribute("infix"))
            {
                Some(infix_attribute) => {
                    fn parse_infix(s: &str) -> Result<OpMeta, InfixError> {
                        let mut iter = s.splitn(2, ",");
                        let fixity = match iter.next().ok_or(InfixError::InvalidFixity)?.trim() {
                            "left" => Fixity::Left,
                            "right" => Fixity::Right,
                            _ => {
                                return Err(InfixError::InvalidFixity);
                            }
                        };
                        let precedence = iter
                            .next()
                            .and_then(|s| s.trim().parse().ok())
                            .and_then(|precedence| {
                                if precedence >= 0 {
                                    Some(precedence)
                                } else {
                                    None
                                }
                            })
                            .ok_or(InfixError::InvalidPrecedence)?;
                        Ok(OpMeta { fixity, precedence })
                    }

                    match parse_infix(infix_attribute) {
                        Ok(op_meta) => {
                            self.op_table.operators.insert(id.clone(), op_meta);
                        }
                        Err(err) => {
                            self.errors.push(pos::spanned(span, err.into()));
                        }
                    }
                }

                None => {
                    if id.as_ref().starts_with(is_operator_char) {
                        self.errors.push(pos::spanned(
                            span,
                            InfixError::UndefinedFixity(id.as_ref().into()).into(),
                        ))
                    }
                }
            }
        }
    }
    impl<'a, 'b, Id> Visitor<'a> for CheckInfix<'b, Id>
    where
        Id: Clone + Eq + Hash + AsRef<str> + 'a,
    {
        type Ident = Id;

        fn visit_pattern(&mut self, pattern: &'a SpannedPattern<Id>) {
            match pattern.value {
                Pattern::Ident(ref id) => {
                    self.insert_infix(&id.name, pattern.span);
                }
                Pattern::Record { ref fields, .. } => {
                    for field in fields.iter().filter(|field| field.value.is_none()) {
                        self.insert_infix(&field.name.value, field.name.span);
                    }
                }
                _ => (),
            }
            walk_pattern(self, &pattern.value);
        }
    }

    let mut op_table = OpTable::new(None);
    CheckInfix {
        metadata,
        errors: &mut errors,
        op_table: &mut op_table,
    }
    .visit_expr(expr);

    let mut reparser = Reparser::new(op_table, symbols);
    match reparser.reparse(expr) {
        Err(reparse_errors) => {
            errors.extend(reparse_errors.into_iter().map(|err| err.map(Error::from)));
        }
        Ok(_) => {}
    }

    if errors.has_errors() {
        Err(errors)
    } else {
        Ok(())
    }
}
