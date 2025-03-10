// Copyright 2021 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::{error, fmt};

use itertools::Itertools;
use once_cell::sync::Lazy;
use pest::iterators::{Pair, Pairs};
use pest::pratt_parser::{Assoc, Op, PrattParser};
use pest::Parser;
use pest_derive::Parser;
use thiserror::Error;

use crate::backend::{BackendError, BackendResult, CommitId};
use crate::commit::Commit;
use crate::default_index_store::{IndexEntry, IndexPosition};
use crate::op_store::WorkspaceId;
use crate::repo::Repo;
use crate::repo_path::{FsPathParseError, RepoPath};
use crate::store::Store;

#[derive(Debug, Error)]
pub enum RevsetError {
    #[error("Revision \"{0}\" doesn't exist")]
    NoSuchRevision(String),
    #[error("Commit or change id prefix \"{0}\" is ambiguous")]
    AmbiguousIdPrefix(String),
    #[error("Unexpected error from store: {0}")]
    StoreError(#[source] BackendError),
}

#[derive(Parser)]
#[grammar = "revset.pest"]
pub struct RevsetParser;

#[derive(Debug)]
pub struct RevsetParseError {
    kind: RevsetParseErrorKind,
    pest_error: Option<Box<pest::error::Error<Rule>>>,
    origin: Option<Box<RevsetParseError>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RevsetParseErrorKind {
    #[error("Syntax error")]
    SyntaxError,
    #[error("'{op}' is not a postfix operator (Did you mean '{similar_op}' for {description}?)")]
    NotPostfixOperator {
        op: String,
        similar_op: String,
        description: String,
    },
    #[error("'{op}' is not an infix operator (Did you mean '{similar_op}' for {description}?)")]
    NotInfixOperator {
        op: String,
        similar_op: String,
        description: String,
    },
    #[error("Revset function \"{0}\" doesn't exist")]
    NoSuchFunction(String),
    #[error("Invalid arguments to revset function \"{name}\": {message}")]
    InvalidFunctionArguments { name: String, message: String },
    #[error("Invalid file pattern: {0}")]
    FsPathParseError(#[source] FsPathParseError),
    #[error("Cannot resolve file pattern without workspace")]
    FsPathWithoutWorkspace,
    #[error("Redefinition of function parameter")]
    RedefinedFunctionParameter,
    #[error(r#"Alias "{0}" cannot be expanded"#)]
    BadAliasExpansion(String),
    #[error(r#"Alias "{0}" expanded recursively"#)]
    RecursiveAlias(String),
}

impl RevsetParseError {
    fn new(kind: RevsetParseErrorKind) -> Self {
        RevsetParseError {
            kind,
            pest_error: None,
            origin: None,
        }
    }

    fn with_span(kind: RevsetParseErrorKind, span: pest::Span<'_>) -> Self {
        let err = pest::error::Error::new_from_span(
            pest::error::ErrorVariant::CustomError {
                message: kind.to_string(),
            },
            span,
        );
        RevsetParseError {
            kind,
            pest_error: Some(Box::new(err)),
            origin: None,
        }
    }

    fn with_span_and_origin(
        kind: RevsetParseErrorKind,
        span: pest::Span<'_>,
        origin: Self,
    ) -> Self {
        let err = pest::error::Error::new_from_span(
            pest::error::ErrorVariant::CustomError {
                message: kind.to_string(),
            },
            span,
        );
        RevsetParseError {
            kind,
            pest_error: Some(Box::new(err)),
            origin: Some(Box::new(origin)),
        }
    }

    pub fn kind(&self) -> &RevsetParseErrorKind {
        &self.kind
    }

    /// Original parsing error which typically occurred in an alias expression.
    pub fn origin(&self) -> Option<&Self> {
        self.origin.as_deref()
    }
}

impl From<pest::error::Error<Rule>> for RevsetParseError {
    fn from(err: pest::error::Error<Rule>) -> Self {
        RevsetParseError {
            kind: RevsetParseErrorKind::SyntaxError,
            pest_error: Some(Box::new(err)),
            origin: None,
        }
    }
}

impl fmt::Display for RevsetParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(err) = &self.pest_error {
            err.fmt(f)
        } else {
            self.kind.fmt(f)
        }
    }
}

impl error::Error for RevsetParseError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        if let Some(e) = self.origin() {
            return Some(e as &dyn error::Error);
        }
        match &self.kind {
            // SyntaxError is a wrapper for pest::error::Error.
            RevsetParseErrorKind::SyntaxError => {
                self.pest_error.as_ref().map(|e| e as &dyn error::Error)
            }
            // Otherwise the kind represents this error.
            e => e.source(),
        }
    }
}

// assumes index has less than u32::MAX entries.
pub const GENERATION_RANGE_FULL: Range<u32> = 0..u32::MAX;
pub const GENERATION_RANGE_EMPTY: Range<u32> = 0..0;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RevsetFilterPredicate {
    /// Commits with number of parents in the range.
    ParentCount(Range<u32>),
    /// Commits with description containing the needle.
    Description(String),
    /// Commits with author's name or email containing the needle.
    Author(String),
    /// Commits with committer's name or email containing the needle.
    Committer(String),
    /// Commits modifying the paths specified by the pattern.
    File(Option<Vec<RepoPath>>), // TODO: embed matcher expression?
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum RevsetExpression {
    None,
    All,
    Commits(Vec<CommitId>),
    Symbol(String),
    Children(Rc<RevsetExpression>),
    Ancestors {
        heads: Rc<RevsetExpression>,
        generation: Range<u32>,
    },
    // Commits that are ancestors of "heads" but not ancestors of "roots"
    Range {
        roots: Rc<RevsetExpression>,
        heads: Rc<RevsetExpression>,
        generation: Range<u32>,
    },
    // Commits that are descendants of "roots" and ancestors of "heads"
    DagRange {
        roots: Rc<RevsetExpression>,
        heads: Rc<RevsetExpression>,
    },
    Heads(Rc<RevsetExpression>),
    Roots(Rc<RevsetExpression>),
    VisibleHeads,
    PublicHeads,
    Branches(String),
    RemoteBranches {
        branch_needle: String,
        remote_needle: String,
    },
    Tags,
    GitRefs,
    GitHead,
    Filter(RevsetFilterPredicate),
    /// Marker for subtree that should be intersected as filter.
    AsFilter(Rc<RevsetExpression>),
    Present(Rc<RevsetExpression>),
    NotIn(Rc<RevsetExpression>),
    Union(Rc<RevsetExpression>, Rc<RevsetExpression>),
    Intersection(Rc<RevsetExpression>, Rc<RevsetExpression>),
    Difference(Rc<RevsetExpression>, Rc<RevsetExpression>),
}

impl RevsetExpression {
    pub fn none() -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::None)
    }

    pub fn all() -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::All)
    }

    pub fn symbol(value: String) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Symbol(value))
    }

    pub fn commit(commit_id: CommitId) -> Rc<RevsetExpression> {
        RevsetExpression::commits(vec![commit_id])
    }

    pub fn commits(commit_ids: Vec<CommitId>) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Commits(commit_ids))
    }

    pub fn visible_heads() -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::VisibleHeads)
    }

    pub fn public_heads() -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::PublicHeads)
    }

    pub fn branches(needle: String) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Branches(needle))
    }

    pub fn remote_branches(branch_needle: String, remote_needle: String) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::RemoteBranches {
            branch_needle,
            remote_needle,
        })
    }

    pub fn tags() -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Tags)
    }

    pub fn git_refs() -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::GitRefs)
    }

    pub fn git_head() -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::GitHead)
    }

    pub fn filter(predicate: RevsetFilterPredicate) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Filter(predicate))
    }

    /// Commits in `self` that don't have descendants in `self`.
    pub fn heads(self: &Rc<RevsetExpression>) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Heads(self.clone()))
    }

    /// Commits in `self` that don't have ancestors in `self`.
    pub fn roots(self: &Rc<RevsetExpression>) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Roots(self.clone()))
    }

    /// Parents of `self`.
    pub fn parents(self: &Rc<RevsetExpression>) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Ancestors {
            heads: self.clone(),
            generation: 1..2,
        })
    }

    /// Ancestors of `self`, including `self`.
    pub fn ancestors(self: &Rc<RevsetExpression>) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Ancestors {
            heads: self.clone(),
            generation: GENERATION_RANGE_FULL,
        })
    }

    /// Children of `self`.
    pub fn children(self: &Rc<RevsetExpression>) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Children(self.clone()))
    }

    /// Descendants of `self`, including `self`.
    pub fn descendants(self: &Rc<RevsetExpression>) -> Rc<RevsetExpression> {
        self.dag_range_to(&RevsetExpression::visible_heads())
    }

    /// Commits that are descendants of `self` and ancestors of `heads`, both
    /// inclusive.
    pub fn dag_range_to(
        self: &Rc<RevsetExpression>,
        heads: &Rc<RevsetExpression>,
    ) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::DagRange {
            roots: self.clone(),
            heads: heads.clone(),
        })
    }

    /// Connects any ancestors and descendants in the set by adding the commits
    /// between them.
    pub fn connected(self: &Rc<RevsetExpression>) -> Rc<RevsetExpression> {
        self.dag_range_to(self)
    }

    /// Commits reachable from `heads` but not from `self`.
    pub fn range(
        self: &Rc<RevsetExpression>,
        heads: &Rc<RevsetExpression>,
    ) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Range {
            roots: self.clone(),
            heads: heads.clone(),
            generation: GENERATION_RANGE_FULL,
        })
    }

    /// Commits that are not in `self`, i.e. the complement of `self`.
    pub fn negated(self: &Rc<RevsetExpression>) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::NotIn(self.clone()))
    }

    /// Commits that are in `self` or in `other` (or both).
    pub fn union(
        self: &Rc<RevsetExpression>,
        other: &Rc<RevsetExpression>,
    ) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Union(self.clone(), other.clone()))
    }

    /// Commits that are in `self` and in `other`.
    pub fn intersection(
        self: &Rc<RevsetExpression>,
        other: &Rc<RevsetExpression>,
    ) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Intersection(self.clone(), other.clone()))
    }

    /// Commits that are in `self` but not in `other`.
    pub fn minus(
        self: &Rc<RevsetExpression>,
        other: &Rc<RevsetExpression>,
    ) -> Rc<RevsetExpression> {
        Rc::new(RevsetExpression::Difference(self.clone(), other.clone()))
    }

    pub fn evaluate<'index>(
        &self,
        repo: &'index dyn Repo,
        workspace_ctx: Option<&RevsetWorkspaceContext>,
    ) -> Result<Box<dyn Revset<'index> + 'index>, RevsetError> {
        repo.index().evaluate_revset(repo, self, workspace_ctx)
    }
}

#[derive(Clone, Debug, Default)]
pub struct RevsetAliasesMap {
    symbol_aliases: HashMap<String, String>,
    function_aliases: HashMap<String, (Vec<String>, String)>,
}

impl RevsetAliasesMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds new substitution rule `decl = defn`.
    ///
    /// Returns error if `decl` is invalid. The `defn` part isn't checked. A bad
    /// `defn` will be reported when the alias is substituted.
    pub fn insert(
        &mut self,
        decl: impl AsRef<str>,
        defn: impl Into<String>,
    ) -> Result<(), RevsetParseError> {
        match RevsetAliasDeclaration::parse(decl.as_ref())? {
            RevsetAliasDeclaration::Symbol(name) => {
                self.symbol_aliases.insert(name, defn.into());
            }
            RevsetAliasDeclaration::Function(name, params) => {
                self.function_aliases.insert(name, (params, defn.into()));
            }
        }
        Ok(())
    }

    fn get_symbol<'a>(&'a self, name: &str) -> Option<(RevsetAliasId<'a>, &'a str)> {
        self.symbol_aliases
            .get_key_value(name)
            .map(|(name, defn)| (RevsetAliasId::Symbol(name), defn.as_ref()))
    }

    fn get_function<'a>(
        &'a self,
        name: &str,
    ) -> Option<(RevsetAliasId<'a>, &'a [String], &'a str)> {
        self.function_aliases
            .get_key_value(name)
            .map(|(name, (params, defn))| {
                (
                    RevsetAliasId::Function(name),
                    params.as_ref(),
                    defn.as_ref(),
                )
            })
    }
}

/// Parsed declaration part of alias rule.
#[derive(Clone, Debug)]
enum RevsetAliasDeclaration {
    Symbol(String),
    Function(String, Vec<String>),
}

impl RevsetAliasDeclaration {
    fn parse(source: &str) -> Result<Self, RevsetParseError> {
        let mut pairs = RevsetParser::parse(Rule::alias_declaration, source)?;
        let first = pairs.next().unwrap();
        match first.as_rule() {
            Rule::identifier => Ok(RevsetAliasDeclaration::Symbol(first.as_str().to_owned())),
            Rule::function_name => {
                let name = first.as_str().to_owned();
                let params_pair = pairs.next().unwrap();
                let params_span = params_pair.as_span();
                let params = params_pair
                    .into_inner()
                    .map(|pair| match pair.as_rule() {
                        Rule::identifier => pair.as_str().to_owned(),
                        r => panic!("unexpected formal parameter rule {r:?}"),
                    })
                    .collect_vec();
                if params.iter().all_unique() {
                    Ok(RevsetAliasDeclaration::Function(name, params))
                } else {
                    Err(RevsetParseError::with_span(
                        RevsetParseErrorKind::RedefinedFunctionParameter,
                        params_span,
                    ))
                }
            }
            r => panic!("unexpected alias declaration rule {r:?}"),
        }
    }
}

/// Borrowed reference to identify alias expression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RevsetAliasId<'a> {
    Symbol(&'a str),
    Function(&'a str),
}

impl fmt::Display for RevsetAliasId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RevsetAliasId::Symbol(name) => write!(f, "{name}"),
            RevsetAliasId::Function(name) => write!(f, "{name}()"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ParseState<'a> {
    aliases_map: &'a RevsetAliasesMap,
    aliases_expanding: &'a [RevsetAliasId<'a>],
    locals: &'a HashMap<&'a str, Rc<RevsetExpression>>,
    workspace_ctx: Option<&'a RevsetWorkspaceContext<'a>>,
}

impl ParseState<'_> {
    fn with_alias_expanding<T>(
        self,
        id: RevsetAliasId<'_>,
        locals: &HashMap<&str, Rc<RevsetExpression>>,
        span: pest::Span<'_>,
        f: impl FnOnce(ParseState<'_>) -> Result<T, RevsetParseError>,
    ) -> Result<T, RevsetParseError> {
        // The stack should be short, so let's simply do linear search and duplicate.
        if self.aliases_expanding.contains(&id) {
            return Err(RevsetParseError::with_span(
                RevsetParseErrorKind::RecursiveAlias(id.to_string()),
                span,
            ));
        }
        let mut aliases_expanding = self.aliases_expanding.to_vec();
        aliases_expanding.push(id);
        let expanding_state = ParseState {
            aliases_map: self.aliases_map,
            aliases_expanding: &aliases_expanding,
            locals,
            workspace_ctx: self.workspace_ctx,
        };
        f(expanding_state).map_err(|e| {
            RevsetParseError::with_span_and_origin(
                RevsetParseErrorKind::BadAliasExpansion(id.to_string()),
                span,
                e,
            )
        })
    }
}

fn parse_program(
    revset_str: &str,
    state: ParseState,
) -> Result<Rc<RevsetExpression>, RevsetParseError> {
    let mut pairs = RevsetParser::parse(Rule::program, revset_str)?;
    let first = pairs.next().unwrap();
    parse_expression_rule(first.into_inner(), state)
}

fn parse_expression_rule(
    pairs: Pairs<Rule>,
    state: ParseState,
) -> Result<Rc<RevsetExpression>, RevsetParseError> {
    fn not_postfix_op(
        op: &Pair<Rule>,
        similar_op: impl Into<String>,
        description: impl Into<String>,
    ) -> RevsetParseError {
        RevsetParseError::with_span(
            RevsetParseErrorKind::NotPostfixOperator {
                op: op.as_str().to_owned(),
                similar_op: similar_op.into(),
                description: description.into(),
            },
            op.as_span(),
        )
    }

    fn not_infix_op(
        op: &Pair<Rule>,
        similar_op: impl Into<String>,
        description: impl Into<String>,
    ) -> RevsetParseError {
        RevsetParseError::with_span(
            RevsetParseErrorKind::NotInfixOperator {
                op: op.as_str().to_owned(),
                similar_op: similar_op.into(),
                description: description.into(),
            },
            op.as_span(),
        )
    }

    static PRATT: Lazy<PrattParser<Rule>> = Lazy::new(|| {
        PrattParser::new()
            .op(Op::infix(Rule::union_op, Assoc::Left)
                | Op::infix(Rule::compat_add_op, Assoc::Left))
            .op(Op::infix(Rule::intersection_op, Assoc::Left)
                | Op::infix(Rule::difference_op, Assoc::Left)
                | Op::infix(Rule::compat_sub_op, Assoc::Left))
            .op(Op::prefix(Rule::negate_op))
            // Ranges can't be nested without parentheses. Associativity doesn't matter.
            .op(Op::infix(Rule::dag_range_op, Assoc::Left) | Op::infix(Rule::range_op, Assoc::Left))
            .op(Op::prefix(Rule::dag_range_pre_op) | Op::prefix(Rule::range_pre_op))
            .op(Op::postfix(Rule::dag_range_post_op) | Op::postfix(Rule::range_post_op))
            // Neighbors
            .op(Op::postfix(Rule::parents_op)
                | Op::postfix(Rule::children_op)
                | Op::postfix(Rule::compat_parents_op))
    });
    PRATT
        .map_primary(|primary| parse_primary_rule(primary, state))
        .map_prefix(|op, rhs| match op.as_rule() {
            Rule::negate_op => Ok(rhs?.negated()),
            Rule::dag_range_pre_op | Rule::range_pre_op => Ok(rhs?.ancestors()),
            r => panic!("unexpected prefix operator rule {r:?}"),
        })
        .map_postfix(|lhs, op| match op.as_rule() {
            Rule::dag_range_post_op => Ok(lhs?.descendants()),
            Rule::range_post_op => Ok(lhs?.range(&RevsetExpression::visible_heads())),
            Rule::parents_op => Ok(lhs?.parents()),
            Rule::children_op => Ok(lhs?.children()),
            Rule::compat_parents_op => Err(not_postfix_op(&op, "-", "parents")),
            r => panic!("unexpected postfix operator rule {r:?}"),
        })
        .map_infix(|lhs, op, rhs| match op.as_rule() {
            Rule::union_op => Ok(lhs?.union(&rhs?)),
            Rule::compat_add_op => Err(not_infix_op(&op, "|", "union")),
            Rule::intersection_op => Ok(lhs?.intersection(&rhs?)),
            Rule::difference_op => Ok(lhs?.minus(&rhs?)),
            Rule::compat_sub_op => Err(not_infix_op(&op, "~", "difference")),
            Rule::dag_range_op => Ok(lhs?.dag_range_to(&rhs?)),
            Rule::range_op => Ok(lhs?.range(&rhs?)),
            r => panic!("unexpected infix operator rule {r:?}"),
        })
        .parse(pairs)
}

fn parse_primary_rule(
    pair: Pair<Rule>,
    state: ParseState,
) -> Result<Rc<RevsetExpression>, RevsetParseError> {
    let span = pair.as_span();
    let mut pairs = pair.into_inner();
    let first = pairs.next().unwrap();
    match first.as_rule() {
        Rule::expression => parse_expression_rule(first.into_inner(), state),
        Rule::function_name => {
            let arguments_pair = pairs.next().unwrap();
            parse_function_expression(first, arguments_pair, state, span)
        }
        Rule::symbol => parse_symbol_rule(first.into_inner(), state),
        _ => {
            panic!("unexpected revset parse rule: {:?}", first.as_str());
        }
    }
}

fn parse_symbol_rule(
    mut pairs: Pairs<Rule>,
    state: ParseState,
) -> Result<Rc<RevsetExpression>, RevsetParseError> {
    let first = pairs.next().unwrap();
    match first.as_rule() {
        Rule::identifier => {
            let name = first.as_str();
            if let Some(expr) = state.locals.get(name) {
                Ok(expr.clone())
            } else if let Some((id, defn)) = state.aliases_map.get_symbol(name) {
                let locals = HashMap::new(); // Don't spill out the current scope
                state.with_alias_expanding(id, &locals, first.as_span(), |state| {
                    parse_program(defn, state)
                })
            } else {
                Ok(RevsetExpression::symbol(name.to_owned()))
            }
        }
        Rule::literal_string => {
            return Ok(RevsetExpression::symbol(
                first
                    .as_str()
                    .strip_prefix('"')
                    .unwrap()
                    .strip_suffix('"')
                    .unwrap()
                    .to_owned(),
            ));
        }
        _ => {
            panic!("unexpected symbol parse rule: {:?}", first.as_str());
        }
    }
}

fn parse_function_expression(
    name_pair: Pair<Rule>,
    arguments_pair: Pair<Rule>,
    state: ParseState,
    primary_span: pest::Span<'_>,
) -> Result<Rc<RevsetExpression>, RevsetParseError> {
    let name = name_pair.as_str();
    if let Some((id, params, defn)) = state.aliases_map.get_function(name) {
        // Resolve arguments in the current scope, and pass them in to the alias
        // expansion scope.
        let (required, optional) =
            expect_named_arguments_vec(name, &[], arguments_pair, params.len(), params.len())?;
        assert!(optional.is_empty());
        let args: Vec<_> = required
            .into_iter()
            .map(|arg| parse_expression_rule(arg.into_inner(), state))
            .try_collect()?;
        let locals = params.iter().map(|s| s.as_str()).zip(args).collect();
        state.with_alias_expanding(id, &locals, primary_span, |state| {
            parse_program(defn, state)
        })
    } else {
        parse_builtin_function(name_pair, arguments_pair, state)
    }
}

fn parse_builtin_function(
    name_pair: Pair<Rule>,
    arguments_pair: Pair<Rule>,
    state: ParseState,
) -> Result<Rc<RevsetExpression>, RevsetParseError> {
    let name = name_pair.as_str();
    match name {
        "parents" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let expression = parse_expression_rule(arg.into_inner(), state)?;
            Ok(expression.parents())
        }
        "children" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let expression = parse_expression_rule(arg.into_inner(), state)?;
            Ok(expression.children())
        }
        "ancestors" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let expression = parse_expression_rule(arg.into_inner(), state)?;
            Ok(expression.ancestors())
        }
        "descendants" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let expression = parse_expression_rule(arg.into_inner(), state)?;
            Ok(expression.descendants())
        }
        "connected" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let candidates = parse_expression_rule(arg.into_inner(), state)?;
            Ok(candidates.connected())
        }
        "none" => {
            expect_no_arguments(name, arguments_pair)?;
            Ok(RevsetExpression::none())
        }
        "all" => {
            expect_no_arguments(name, arguments_pair)?;
            Ok(RevsetExpression::all())
        }
        "heads" => {
            let ([], [opt_arg]) = expect_arguments(name, arguments_pair)?;
            if let Some(arg) = opt_arg {
                let candidates = parse_expression_rule(arg.into_inner(), state)?;
                Ok(candidates.heads())
            } else {
                Ok(RevsetExpression::visible_heads())
            }
        }
        "roots" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let candidates = parse_expression_rule(arg.into_inner(), state)?;
            Ok(candidates.roots())
        }
        "public_heads" => {
            expect_no_arguments(name, arguments_pair)?;
            Ok(RevsetExpression::public_heads())
        }
        "branches" => {
            let ([], [opt_arg]) = expect_arguments(name, arguments_pair)?;
            let needle = if let Some(arg) = opt_arg {
                parse_function_argument_to_string(name, arg, state)?
            } else {
                "".to_owned()
            };
            Ok(RevsetExpression::branches(needle))
        }
        "remote_branches" => {
            let ([], [branch_opt_arg, remote_opt_arg]) =
                expect_named_arguments(name, &["", "remote"], arguments_pair)?;
            let branch_needle = if let Some(branch_arg) = branch_opt_arg {
                parse_function_argument_to_string(name, branch_arg, state)?
            } else {
                "".to_owned()
            };
            let remote_needle = if let Some(remote_arg) = remote_opt_arg {
                parse_function_argument_to_string(name, remote_arg, state)?
            } else {
                "".to_owned()
            };
            Ok(RevsetExpression::remote_branches(
                branch_needle,
                remote_needle,
            ))
        }
        "tags" => {
            expect_no_arguments(name, arguments_pair)?;
            Ok(RevsetExpression::tags())
        }
        "git_refs" => {
            expect_no_arguments(name, arguments_pair)?;
            Ok(RevsetExpression::git_refs())
        }
        "git_head" => {
            expect_no_arguments(name, arguments_pair)?;
            Ok(RevsetExpression::git_head())
        }
        "merges" => {
            expect_no_arguments(name, arguments_pair)?;
            Ok(RevsetExpression::filter(
                RevsetFilterPredicate::ParentCount(2..u32::MAX),
            ))
        }
        "description" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let needle = parse_function_argument_to_string(name, arg, state)?;
            Ok(RevsetExpression::filter(
                RevsetFilterPredicate::Description(needle),
            ))
        }
        "author" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let needle = parse_function_argument_to_string(name, arg, state)?;
            Ok(RevsetExpression::filter(RevsetFilterPredicate::Author(
                needle,
            )))
        }
        "committer" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let needle = parse_function_argument_to_string(name, arg, state)?;
            Ok(RevsetExpression::filter(RevsetFilterPredicate::Committer(
                needle,
            )))
        }
        "empty" => {
            expect_no_arguments(name, arguments_pair)?;
            Ok(RevsetExpression::filter(RevsetFilterPredicate::File(None)).negated())
        }
        "file" => {
            if let Some(ctx) = state.workspace_ctx {
                let arguments_span = arguments_pair.as_span();
                let paths: Vec<_> = arguments_pair
                    .into_inner()
                    .map(|arg| -> Result<_, RevsetParseError> {
                        let span = arg.as_span();
                        let needle = parse_function_argument_to_string(name, arg, state)?;
                        let path = RepoPath::parse_fs_path(ctx.cwd, ctx.workspace_root, &needle)
                            .map_err(|e| {
                                RevsetParseError::with_span(
                                    RevsetParseErrorKind::FsPathParseError(e),
                                    span,
                                )
                            })?;
                        Ok(path)
                    })
                    .try_collect()?;
                if paths.is_empty() {
                    Err(RevsetParseError::with_span(
                        RevsetParseErrorKind::InvalidFunctionArguments {
                            name: name.to_owned(),
                            message: "Expected at least 1 argument".to_string(),
                        },
                        arguments_span,
                    ))
                } else {
                    Ok(RevsetExpression::filter(RevsetFilterPredicate::File(Some(
                        paths,
                    ))))
                }
            } else {
                Err(RevsetParseError::new(
                    RevsetParseErrorKind::FsPathWithoutWorkspace,
                ))
            }
        }
        "present" => {
            let arg = expect_one_argument(name, arguments_pair)?;
            let expression = parse_expression_rule(arg.into_inner(), state)?;
            Ok(Rc::new(RevsetExpression::Present(expression)))
        }
        _ => Err(RevsetParseError::with_span(
            RevsetParseErrorKind::NoSuchFunction(name.to_owned()),
            name_pair.as_span(),
        )),
    }
}

type OptionalArg<'i> = Option<Pair<'i, Rule>>;

fn expect_no_arguments(
    function_name: &str,
    arguments_pair: Pair<Rule>,
) -> Result<(), RevsetParseError> {
    let ([], []) = expect_arguments(function_name, arguments_pair)?;
    Ok(())
}

fn expect_one_argument<'i>(
    function_name: &str,
    arguments_pair: Pair<'i, Rule>,
) -> Result<Pair<'i, Rule>, RevsetParseError> {
    let ([arg], []) = expect_arguments(function_name, arguments_pair)?;
    Ok(arg)
}

fn expect_arguments<'i, const N: usize, const M: usize>(
    function_name: &str,
    arguments_pair: Pair<'i, Rule>,
) -> Result<([Pair<'i, Rule>; N], [OptionalArg<'i>; M]), RevsetParseError> {
    expect_named_arguments(function_name, &[], arguments_pair)
}

/// Extracts N required arguments and M optional arguments.
///
/// `argument_names` is a list of argument names. Unnamed positional arguments
/// should be padded with `""`.
fn expect_named_arguments<'i, const N: usize, const M: usize>(
    function_name: &str,
    argument_names: &[&str],
    arguments_pair: Pair<'i, Rule>,
) -> Result<([Pair<'i, Rule>; N], [OptionalArg<'i>; M]), RevsetParseError> {
    let (required, optional) =
        expect_named_arguments_vec(function_name, argument_names, arguments_pair, N, N + M)?;
    Ok((required.try_into().unwrap(), optional.try_into().unwrap()))
}

fn expect_named_arguments_vec<'i>(
    function_name: &str,
    argument_names: &[&str],
    arguments_pair: Pair<'i, Rule>,
    min_arg_count: usize,
    max_arg_count: usize,
) -> Result<(Vec<Pair<'i, Rule>>, Vec<OptionalArg<'i>>), RevsetParseError> {
    assert!(argument_names.len() <= max_arg_count);
    let arguments_span = arguments_pair.as_span();
    let make_error = |message, span| {
        RevsetParseError::with_span(
            RevsetParseErrorKind::InvalidFunctionArguments {
                name: function_name.to_owned(),
                message,
            },
            span,
        )
    };
    let make_count_error = || {
        let message = if min_arg_count == max_arg_count {
            format!("Expected {min_arg_count} arguments")
        } else {
            format!("Expected {min_arg_count} to {max_arg_count} arguments")
        };
        make_error(message, arguments_span)
    };

    let mut pos_iter = Some(0..max_arg_count);
    let mut extracted_pairs = vec![None; max_arg_count];
    for pair in arguments_pair.into_inner() {
        let span = pair.as_span();
        match pair.as_rule() {
            Rule::expression => {
                let pos = pos_iter
                    .as_mut()
                    .ok_or_else(|| {
                        make_error(
                            "Positional argument follows keyword argument".to_owned(),
                            span,
                        )
                    })?
                    .next()
                    .ok_or_else(make_count_error)?;
                assert!(extracted_pairs[pos].is_none());
                extracted_pairs[pos] = Some(pair);
            }
            Rule::keyword_argument => {
                pos_iter = None; // No more positional arguments
                let mut pairs = pair.into_inner();
                let name = pairs.next().unwrap();
                let expr = pairs.next().unwrap();
                assert_eq!(name.as_rule(), Rule::identifier);
                assert_eq!(expr.as_rule(), Rule::expression);
                let pos = argument_names
                    .iter()
                    .position(|&n| n == name.as_str())
                    .ok_or_else(|| {
                        make_error(
                            format!(r#"Unexpected keyword argument "{}""#, name.as_str()),
                            span,
                        )
                    })?;
                if extracted_pairs[pos].is_some() {
                    return Err(make_error(
                        format!(r#"Got multiple values for keyword "{}""#, name.as_str()),
                        span,
                    ));
                }
                extracted_pairs[pos] = Some(expr);
            }
            r => panic!("unexpected argument rule {r:?}"),
        }
    }

    assert_eq!(extracted_pairs.len(), max_arg_count);
    let optional = extracted_pairs.split_off(min_arg_count);
    let required = extracted_pairs.into_iter().flatten().collect_vec();
    if required.len() != min_arg_count {
        return Err(make_count_error());
    }
    Ok((required, optional))
}

fn parse_function_argument_to_string(
    name: &str,
    pair: Pair<Rule>,
    state: ParseState,
) -> Result<String, RevsetParseError> {
    let span = pair.as_span();
    let expression = parse_expression_rule(pair.into_inner(), state)?;
    match expression.as_ref() {
        RevsetExpression::Symbol(symbol) => Ok(symbol.clone()),
        _ => Err(RevsetParseError::with_span(
            RevsetParseErrorKind::InvalidFunctionArguments {
                name: name.to_string(),
                message: "Expected function argument of type string".to_owned(),
            },
            span,
        )),
    }
}

pub fn parse(
    revset_str: &str,
    aliases_map: &RevsetAliasesMap,
    workspace_ctx: Option<&RevsetWorkspaceContext>,
) -> Result<Rc<RevsetExpression>, RevsetParseError> {
    let state = ParseState {
        aliases_map,
        aliases_expanding: &[],
        locals: &HashMap::new(),
        workspace_ctx,
    };
    parse_program(revset_str, state)
}

/// Walks `expression` tree and applies `f` recursively from leaf nodes.
///
/// If `f` returns `None`, the original expression node is reused. If no nodes
/// rewritten, returns `None`. `std::iter::successors()` could be used if
/// the transformation needs to be applied repeatedly until converged.
fn transform_expression_bottom_up(
    expression: &Rc<RevsetExpression>,
    mut f: impl FnMut(&Rc<RevsetExpression>) -> Option<Rc<RevsetExpression>>,
) -> Option<Rc<RevsetExpression>> {
    fn transform_child_rec(
        expression: &Rc<RevsetExpression>,
        f: &mut impl FnMut(&Rc<RevsetExpression>) -> Option<Rc<RevsetExpression>>,
    ) -> Option<Rc<RevsetExpression>> {
        match expression.as_ref() {
            RevsetExpression::None => None,
            RevsetExpression::All => None,
            RevsetExpression::Commits(_) => None,
            RevsetExpression::Symbol(_) => None,
            RevsetExpression::Children(roots) => {
                transform_rec(roots, f).map(RevsetExpression::Children)
            }
            RevsetExpression::Ancestors { heads, generation } => {
                transform_rec(heads, f).map(|heads| RevsetExpression::Ancestors {
                    heads,
                    generation: generation.clone(),
                })
            }
            RevsetExpression::Range {
                roots,
                heads,
                generation,
            } => transform_rec_pair((roots, heads), f).map(|(roots, heads)| {
                RevsetExpression::Range {
                    roots,
                    heads,
                    generation: generation.clone(),
                }
            }),
            RevsetExpression::DagRange { roots, heads } => transform_rec_pair((roots, heads), f)
                .map(|(roots, heads)| RevsetExpression::DagRange { roots, heads }),
            RevsetExpression::VisibleHeads => None,
            RevsetExpression::Heads(candidates) => {
                transform_rec(candidates, f).map(RevsetExpression::Heads)
            }
            RevsetExpression::Roots(candidates) => {
                transform_rec(candidates, f).map(RevsetExpression::Roots)
            }
            RevsetExpression::PublicHeads => None,
            RevsetExpression::Branches(_) => None,
            RevsetExpression::RemoteBranches { .. } => None,
            RevsetExpression::Tags => None,
            RevsetExpression::GitRefs => None,
            RevsetExpression::GitHead => None,
            RevsetExpression::Filter(_) => None,
            RevsetExpression::AsFilter(candidates) => {
                transform_rec(candidates, f).map(RevsetExpression::AsFilter)
            }
            RevsetExpression::Present(candidates) => {
                transform_rec(candidates, f).map(RevsetExpression::Present)
            }
            RevsetExpression::NotIn(complement) => {
                transform_rec(complement, f).map(RevsetExpression::NotIn)
            }
            RevsetExpression::Union(expression1, expression2) => {
                transform_rec_pair((expression1, expression2), f).map(
                    |(expression1, expression2)| RevsetExpression::Union(expression1, expression2),
                )
            }
            RevsetExpression::Intersection(expression1, expression2) => {
                transform_rec_pair((expression1, expression2), f).map(
                    |(expression1, expression2)| {
                        RevsetExpression::Intersection(expression1, expression2)
                    },
                )
            }
            RevsetExpression::Difference(expression1, expression2) => {
                transform_rec_pair((expression1, expression2), f).map(
                    |(expression1, expression2)| {
                        RevsetExpression::Difference(expression1, expression2)
                    },
                )
            }
        }
        .map(Rc::new)
    }

    fn transform_rec_pair(
        (expression1, expression2): (&Rc<RevsetExpression>, &Rc<RevsetExpression>),
        f: &mut impl FnMut(&Rc<RevsetExpression>) -> Option<Rc<RevsetExpression>>,
    ) -> Option<(Rc<RevsetExpression>, Rc<RevsetExpression>)> {
        match (transform_rec(expression1, f), transform_rec(expression2, f)) {
            (Some(new_expression1), Some(new_expression2)) => {
                Some((new_expression1, new_expression2))
            }
            (Some(new_expression1), None) => Some((new_expression1, expression2.clone())),
            (None, Some(new_expression2)) => Some((expression1.clone(), new_expression2)),
            (None, None) => None,
        }
    }

    fn transform_rec(
        expression: &Rc<RevsetExpression>,
        f: &mut impl FnMut(&Rc<RevsetExpression>) -> Option<Rc<RevsetExpression>>,
    ) -> Option<Rc<RevsetExpression>> {
        if let Some(new_expression) = transform_child_rec(expression, f) {
            // must propagate new expression tree
            Some(f(&new_expression).unwrap_or(new_expression))
        } else {
            f(expression)
        }
    }

    transform_rec(expression, &mut f)
}

/// Transforms filter expressions, by applying the following rules.
///
/// a. Moves as many sets to left of filter intersection as possible, to
///    minimize the filter inputs.
/// b. TODO: Rewrites set operations to and/or/not of predicates, to
///    help further optimization (e.g. combine `file(_)` matchers.)
/// c. Wraps union of filter and set (e.g. `author(_) | heads()`), to
///    ensure inner filter wouldn't need to evaluate all the input sets.
fn internalize_filter(expression: &Rc<RevsetExpression>) -> Option<Rc<RevsetExpression>> {
    fn is_filter(expression: &RevsetExpression) -> bool {
        matches!(
            expression,
            RevsetExpression::Filter(_) | RevsetExpression::AsFilter(_)
        )
    }

    fn is_filter_tree(expression: &RevsetExpression) -> bool {
        is_filter(expression) || as_filter_intersection(expression).is_some()
    }

    // Extracts 'c & f' from intersect_down()-ed node.
    fn as_filter_intersection(
        expression: &RevsetExpression,
    ) -> Option<(&Rc<RevsetExpression>, &Rc<RevsetExpression>)> {
        if let RevsetExpression::Intersection(expression1, expression2) = expression {
            is_filter(expression2).then(|| (expression1, expression2))
        } else {
            None
        }
    }

    // Since both sides must have already been intersect_down()-ed, we don't need to
    // apply the whole bottom-up pass to new intersection node. Instead, just push
    // new 'c & (d & g)' down-left to '(c & d) & g' while either side is
    // an intersection of filter node.
    fn intersect_down(
        expression1: &Rc<RevsetExpression>,
        expression2: &Rc<RevsetExpression>,
    ) -> Option<Rc<RevsetExpression>> {
        let recurse = |e1, e2| intersect_down(e1, e2).unwrap_or_else(|| e1.intersection(e2));
        match (expression1.as_ref(), expression2.as_ref()) {
            // Don't reorder 'f1 & f2'
            (_, e2) if is_filter(e2) => None,
            // f1 & e2 -> e2 & f1
            (e1, _) if is_filter(e1) => Some(expression2.intersection(expression1)),
            (e1, e2) => match (as_filter_intersection(e1), as_filter_intersection(e2)) {
                // e1 & (c2 & f2) -> (e1 & c2) & f2
                // (c1 & f1) & (c2 & f2) -> ((c1 & f1) & c2) & f2 -> ((c1 & c2) & f1) & f2
                (_, Some((c2, f2))) => Some(recurse(expression1, c2).intersection(f2)),
                // (c1 & f1) & e2 -> (c1 & e2) & f1
                // ((c1 & f1) & g1) & e2 -> ((c1 & f1) & e2) & g1 -> ((c1 & e2) & f1) & g1
                (Some((c1, f1)), _) => Some(recurse(c1, expression2).intersection(f1)),
                (None, None) => None,
            },
        }
    }

    // Bottom-up pass pulls up-right filter node from leaf '(c & f) & e' ->
    // '(c & e) & f', so that an intersection of filter node can be found as
    // a direct child of another intersection node. However, the rewritten
    // intersection node 'c & e' can also be a rewrite target if 'e' contains
    // a filter node. That's why intersect_down() is also recursive.
    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        RevsetExpression::Present(e) => {
            is_filter_tree(e).then(|| Rc::new(RevsetExpression::AsFilter(expression.clone())))
        }
        RevsetExpression::NotIn(e) => {
            is_filter_tree(e).then(|| Rc::new(RevsetExpression::AsFilter(expression.clone())))
        }
        RevsetExpression::Union(e1, e2) => (is_filter_tree(e1) || is_filter_tree(e2))
            .then(|| Rc::new(RevsetExpression::AsFilter(expression.clone()))),
        RevsetExpression::Intersection(expression1, expression2) => {
            intersect_down(expression1, expression2)
        }
        // Difference(e1, e2) should have been unfolded to Intersection(e1, NotIn(e2)).
        _ => None,
    })
}

/// Eliminates redundant nodes like `x & all()`, `~~x`.
///
/// This does not rewrite 'x & none()' to 'none()' because 'x' may be an invalid
/// symbol.
fn fold_redundant_expression(expression: &Rc<RevsetExpression>) -> Option<Rc<RevsetExpression>> {
    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        RevsetExpression::NotIn(outer) => match outer.as_ref() {
            RevsetExpression::NotIn(inner) => Some(inner.clone()),
            _ => None,
        },
        RevsetExpression::Intersection(expression1, expression2) => {
            match (expression1.as_ref(), expression2.as_ref()) {
                (_, RevsetExpression::All) => Some(expression1.clone()),
                (RevsetExpression::All, _) => Some(expression2.clone()),
                _ => None,
            }
        }
        _ => None,
    })
}

/// Transforms negative intersection to difference. Redundant intersections like
/// `all() & e` should have been removed.
fn fold_difference(expression: &Rc<RevsetExpression>) -> Option<Rc<RevsetExpression>> {
    fn to_difference(
        expression: &Rc<RevsetExpression>,
        complement: &Rc<RevsetExpression>,
    ) -> Rc<RevsetExpression> {
        match (expression.as_ref(), complement.as_ref()) {
            // :heads & ~(:roots) -> roots..heads
            (
                RevsetExpression::Ancestors { heads, generation },
                RevsetExpression::Ancestors {
                    heads: roots,
                    generation: GENERATION_RANGE_FULL,
                },
            ) => Rc::new(RevsetExpression::Range {
                roots: roots.clone(),
                heads: heads.clone(),
                generation: generation.clone(),
            }),
            _ => expression.minus(complement),
        }
    }

    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        RevsetExpression::Intersection(expression1, expression2) => {
            match (expression1.as_ref(), expression2.as_ref()) {
                (_, RevsetExpression::NotIn(complement)) => {
                    Some(to_difference(expression1, complement))
                }
                (RevsetExpression::NotIn(complement), _) => {
                    Some(to_difference(expression2, complement))
                }
                _ => None,
            }
        }
        _ => None,
    })
}

/// Transforms binary difference to more primitive negative intersection.
///
/// For example, `all() ~ e` will become `all() & ~e`, which can be simplified
/// further by `fold_redundant_expression()`.
fn unfold_difference(expression: &Rc<RevsetExpression>) -> Option<Rc<RevsetExpression>> {
    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        // roots..heads -> :heads & ~(:roots)
        RevsetExpression::Range {
            roots,
            heads,
            generation,
        } => {
            let heads_ancestors = Rc::new(RevsetExpression::Ancestors {
                heads: heads.clone(),
                generation: generation.clone(),
            });
            Some(heads_ancestors.intersection(&roots.ancestors().negated()))
        }
        RevsetExpression::Difference(expression1, expression2) => {
            Some(expression1.intersection(&expression2.negated()))
        }
        _ => None,
    })
}

/// Transforms nested `ancestors()`/`parents()` like `h---`.
fn fold_ancestors(expression: &Rc<RevsetExpression>) -> Option<Rc<RevsetExpression>> {
    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        RevsetExpression::Ancestors {
            heads,
            generation: generation1,
        } => {
            match heads.as_ref() {
                // (h-)- -> ancestors(ancestors(h, 1), 1) -> ancestors(h, 2)
                // :(h-) -> ancestors(ancestors(h, 1), ..) -> ancestors(h, 1..)
                // (:h)- -> ancestors(ancestors(h, ..), 1) -> ancestors(h, 1..)
                RevsetExpression::Ancestors {
                    heads,
                    generation: generation2,
                } => {
                    // For any (g1, g2) in (generation1, generation2), g1 + g2.
                    let generation = if generation1.is_empty() || generation2.is_empty() {
                        GENERATION_RANGE_EMPTY
                    } else {
                        let start = u32::saturating_add(generation1.start, generation2.start);
                        let end = u32::saturating_add(generation1.end, generation2.end - 1);
                        start..end
                    };
                    Some(Rc::new(RevsetExpression::Ancestors {
                        heads: heads.clone(),
                        generation,
                    }))
                }
                _ => None,
            }
        }
        // Range should have been unfolded to intersection of Ancestors.
        _ => None,
    })
}

/// Rewrites the given `expression` tree to reduce evaluation cost. Returns new
/// tree.
pub fn optimize(expression: Rc<RevsetExpression>) -> Rc<RevsetExpression> {
    let expression = unfold_difference(&expression).unwrap_or(expression);
    let expression = fold_redundant_expression(&expression).unwrap_or(expression);
    let expression = fold_ancestors(&expression).unwrap_or(expression);
    let expression = internalize_filter(&expression).unwrap_or(expression);
    fold_difference(&expression).unwrap_or(expression)
}

pub trait Revset<'index> {
    // All revsets currently iterate in order of descending index position
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_>;

    fn iter_graph(
        &self,
    ) -> Box<dyn Iterator<Item = (IndexEntry<'index>, Vec<RevsetGraphEdge>)> + '_>;

    fn is_empty(&self) -> bool;
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct RevsetGraphEdge {
    pub target: IndexPosition,
    pub edge_type: RevsetGraphEdgeType,
}

impl RevsetGraphEdge {
    pub fn missing(target: IndexPosition) -> Self {
        Self {
            target,
            edge_type: RevsetGraphEdgeType::Missing,
        }
    }
    pub fn direct(target: IndexPosition) -> Self {
        Self {
            target,
            edge_type: RevsetGraphEdgeType::Direct,
        }
    }
    pub fn indirect(target: IndexPosition) -> Self {
        Self {
            target,
            edge_type: RevsetGraphEdgeType::Indirect,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum RevsetGraphEdgeType {
    Missing,
    Direct,
    Indirect,
}

pub trait RevsetIteratorExt<'index, I> {
    fn commit_ids(self) -> RevsetCommitIdIterator<I>;
    fn commits(self, store: &Arc<Store>) -> RevsetCommitIterator<I>;
    fn reversed(self) -> ReverseRevsetIterator<'index>;
}

impl<'index, I: Iterator<Item = IndexEntry<'index>>> RevsetIteratorExt<'index, I> for I {
    fn commit_ids(self) -> RevsetCommitIdIterator<I> {
        RevsetCommitIdIterator(self)
    }

    fn commits(self, store: &Arc<Store>) -> RevsetCommitIterator<I> {
        RevsetCommitIterator {
            iter: self,
            store: store.clone(),
        }
    }

    fn reversed(self) -> ReverseRevsetIterator<'index> {
        ReverseRevsetIterator {
            entries: self.into_iter().collect_vec(),
        }
    }
}

pub struct RevsetCommitIdIterator<I>(I);

impl<'index, I: Iterator<Item = IndexEntry<'index>>> Iterator for RevsetCommitIdIterator<I> {
    type Item = CommitId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|index_entry| index_entry.commit_id())
    }
}

pub struct RevsetCommitIterator<I> {
    store: Arc<Store>,
    iter: I,
}

impl<'index, I: Iterator<Item = IndexEntry<'index>>> Iterator for RevsetCommitIterator<I> {
    type Item = BackendResult<Commit>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()
            .map(|index_entry| self.store.get_commit(&index_entry.commit_id()))
    }
}

pub struct ReverseRevsetIterator<'index> {
    entries: Vec<IndexEntry<'index>>,
}

impl<'index> Iterator for ReverseRevsetIterator<'index> {
    type Item = IndexEntry<'index>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.pop()
    }
}

/// Workspace information needed to evaluate revset expression.
#[derive(Clone, Debug)]
pub struct RevsetWorkspaceContext<'a> {
    pub cwd: &'a Path,
    pub workspace_id: &'a WorkspaceId,
    pub workspace_root: &'a Path,
}

pub struct ReverseRevsetGraphIterator<'index> {
    items: Vec<(IndexEntry<'index>, Vec<RevsetGraphEdge>)>,
}

impl<'index> ReverseRevsetGraphIterator<'index> {
    pub fn new<'revset>(
        input: Box<dyn Iterator<Item = (IndexEntry<'index>, Vec<RevsetGraphEdge>)> + 'revset>,
    ) -> Self {
        let mut entries = vec![];
        let mut reverse_edges: HashMap<IndexPosition, Vec<RevsetGraphEdge>> = HashMap::new();
        for (entry, edges) in input {
            for RevsetGraphEdge { target, edge_type } in edges {
                reverse_edges
                    .entry(target)
                    .or_default()
                    .push(RevsetGraphEdge {
                        target: entry.position(),
                        edge_type,
                    })
            }
            entries.push(entry);
        }

        let mut items = vec![];
        for entry in entries.into_iter() {
            let edges = reverse_edges
                .get(&entry.position())
                .cloned()
                .unwrap_or_default();
            items.push((entry, edges));
        }
        Self { items }
    }
}

impl<'index> Iterator for ReverseRevsetGraphIterator<'index> {
    type Item = (IndexEntry<'index>, Vec<RevsetGraphEdge>);

    fn next(&mut self) -> Option<Self::Item> {
        self.items.pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(revset_str: &str) -> Result<Rc<RevsetExpression>, RevsetParseErrorKind> {
        parse_with_aliases(revset_str, [] as [(&str, &str); 0])
    }

    fn parse_with_aliases(
        revset_str: &str,
        aliases: impl IntoIterator<Item = (impl AsRef<str>, impl Into<String>)>,
    ) -> Result<Rc<RevsetExpression>, RevsetParseErrorKind> {
        let mut aliases_map = RevsetAliasesMap::new();
        for (decl, defn) in aliases {
            aliases_map.insert(decl, defn).unwrap();
        }
        // Set up pseudo context to resolve file(path)
        let workspace_ctx = RevsetWorkspaceContext {
            cwd: Path::new("/"),
            workspace_id: &WorkspaceId::default(),
            workspace_root: Path::new("/"),
        };
        // Map error to comparable object
        super::parse(revset_str, &aliases_map, Some(&workspace_ctx)).map_err(|e| e.kind)
    }

    #[test]
    fn test_revset_expression_building() {
        let wc_symbol = RevsetExpression::symbol("@".to_string());
        let foo_symbol = RevsetExpression::symbol("foo".to_string());
        assert_eq!(
            wc_symbol,
            Rc::new(RevsetExpression::Symbol("@".to_string()))
        );
        assert_eq!(
            wc_symbol.heads(),
            Rc::new(RevsetExpression::Heads(wc_symbol.clone()))
        );
        assert_eq!(
            wc_symbol.roots(),
            Rc::new(RevsetExpression::Roots(wc_symbol.clone()))
        );
        assert_eq!(
            wc_symbol.parents(),
            Rc::new(RevsetExpression::Ancestors {
                heads: wc_symbol.clone(),
                generation: 1..2,
            })
        );
        assert_eq!(
            wc_symbol.ancestors(),
            Rc::new(RevsetExpression::Ancestors {
                heads: wc_symbol.clone(),
                generation: GENERATION_RANGE_FULL,
            })
        );
        assert_eq!(
            foo_symbol.children(),
            Rc::new(RevsetExpression::Children(foo_symbol.clone()))
        );
        assert_eq!(
            foo_symbol.descendants(),
            Rc::new(RevsetExpression::DagRange {
                roots: foo_symbol.clone(),
                heads: RevsetExpression::visible_heads(),
            })
        );
        assert_eq!(
            foo_symbol.dag_range_to(&wc_symbol),
            Rc::new(RevsetExpression::DagRange {
                roots: foo_symbol.clone(),
                heads: wc_symbol.clone(),
            })
        );
        assert_eq!(
            foo_symbol.connected(),
            Rc::new(RevsetExpression::DagRange {
                roots: foo_symbol.clone(),
                heads: foo_symbol.clone(),
            })
        );
        assert_eq!(
            foo_symbol.range(&wc_symbol),
            Rc::new(RevsetExpression::Range {
                roots: foo_symbol.clone(),
                heads: wc_symbol.clone(),
                generation: GENERATION_RANGE_FULL,
            })
        );
        assert_eq!(
            foo_symbol.negated(),
            Rc::new(RevsetExpression::NotIn(foo_symbol.clone()))
        );
        assert_eq!(
            foo_symbol.union(&wc_symbol),
            Rc::new(RevsetExpression::Union(
                foo_symbol.clone(),
                wc_symbol.clone()
            ))
        );
        assert_eq!(
            foo_symbol.intersection(&wc_symbol),
            Rc::new(RevsetExpression::Intersection(
                foo_symbol.clone(),
                wc_symbol.clone()
            ))
        );
        assert_eq!(
            foo_symbol.minus(&wc_symbol),
            Rc::new(RevsetExpression::Difference(foo_symbol, wc_symbol.clone()))
        );
    }

    #[test]
    fn test_parse_revset() {
        let wc_symbol = RevsetExpression::symbol("@".to_string());
        let foo_symbol = RevsetExpression::symbol("foo".to_string());
        let bar_symbol = RevsetExpression::symbol("bar".to_string());
        // Parse a single symbol (specifically the "checkout" symbol)
        assert_eq!(parse("@"), Ok(wc_symbol.clone()));
        // Parse a single symbol
        assert_eq!(parse("foo"), Ok(foo_symbol.clone()));
        // Internal '.', '-', and '+' are allowed
        assert_eq!(
            parse("foo.bar-v1+7"),
            Ok(RevsetExpression::symbol("foo.bar-v1+7".to_string()))
        );
        assert_eq!(
            parse("foo.bar-v1+7-"),
            Ok(RevsetExpression::symbol("foo.bar-v1+7".to_string()).parents())
        );
        // Default arguments for *branches() are all ""
        assert_eq!(parse("branches()"), parse(r#"branches("")"#));
        assert_eq!(parse("remote_branches()"), parse(r#"remote_branches("")"#));
        assert_eq!(
            parse("remote_branches()"),
            parse(r#"remote_branches("", "")"#)
        );
        // '.' is not allowed at the beginning or end
        assert_eq!(parse(".foo"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("foo."), Err(RevsetParseErrorKind::SyntaxError));
        // Multiple '.', '-', '+' are not allowed
        assert_eq!(parse("foo.+bar"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("foo--bar"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("foo+-bar"), Err(RevsetParseErrorKind::SyntaxError));
        // Parse a parenthesized symbol
        assert_eq!(parse("(foo)"), Ok(foo_symbol.clone()));
        // Parse a quoted symbol
        assert_eq!(parse("\"foo\""), Ok(foo_symbol.clone()));
        // Parse the "parents" operator
        assert_eq!(parse("@-"), Ok(wc_symbol.parents()));
        // Parse the "children" operator
        assert_eq!(parse("@+"), Ok(wc_symbol.children()));
        // Parse the "ancestors" operator
        assert_eq!(parse(":@"), Ok(wc_symbol.ancestors()));
        // Parse the "descendants" operator
        assert_eq!(parse("@:"), Ok(wc_symbol.descendants()));
        // Parse the "dag range" operator
        assert_eq!(parse("foo:bar"), Ok(foo_symbol.dag_range_to(&bar_symbol)));
        // Parse the "range" prefix operator
        assert_eq!(parse("..@"), Ok(wc_symbol.ancestors()));
        assert_eq!(
            parse("@.."),
            Ok(wc_symbol.range(&RevsetExpression::visible_heads()))
        );
        assert_eq!(parse("foo..bar"), Ok(foo_symbol.range(&bar_symbol)));
        // Parse the "negate" operator
        assert_eq!(parse("~ foo"), Ok(foo_symbol.negated()));
        assert_eq!(
            parse("~ ~~ foo"),
            Ok(foo_symbol.negated().negated().negated())
        );
        // Parse the "intersection" operator
        assert_eq!(parse("foo & bar"), Ok(foo_symbol.intersection(&bar_symbol)));
        // Parse the "union" operator
        assert_eq!(parse("foo | bar"), Ok(foo_symbol.union(&bar_symbol)));
        // Parse the "difference" operator
        assert_eq!(parse("foo ~ bar"), Ok(foo_symbol.minus(&bar_symbol)));
        // Parentheses are allowed before suffix operators
        assert_eq!(parse("(@)-"), Ok(wc_symbol.parents()));
        // Space is allowed around expressions
        assert_eq!(parse(" :@ "), Ok(wc_symbol.ancestors()));
        assert_eq!(parse("( :@ )"), Ok(wc_symbol.ancestors()));
        // Space is not allowed around prefix operators
        assert_eq!(parse(" : @ "), Err(RevsetParseErrorKind::SyntaxError));
        // Incomplete parse
        assert_eq!(parse("foo | -"), Err(RevsetParseErrorKind::SyntaxError));
        // Space is allowed around infix operators and function arguments
        assert_eq!(
            parse("   description(  arg1 ) ~    file(  arg1 ,   arg2 )  ~ heads(  )  "),
            Ok(
                RevsetExpression::filter(RevsetFilterPredicate::Description("arg1".to_string()))
                    .minus(&RevsetExpression::filter(RevsetFilterPredicate::File(
                        Some(vec![
                            RepoPath::from_internal_string("arg1"),
                            RepoPath::from_internal_string("arg2"),
                        ])
                    )))
                    .minus(&RevsetExpression::visible_heads())
            )
        );
        // Space is allowed around keyword arguments
        assert_eq!(
            parse("remote_branches( remote  =   foo  )").unwrap(),
            parse("remote_branches(remote=foo)").unwrap(),
        );

        // Trailing comma isn't allowed for empty argument
        assert!(parse("branches(,)").is_err());
        // Trailing comma is allowed for the last argument
        assert!(parse("branches(a,)").is_ok());
        assert!(parse("branches(a ,  )").is_ok());
        assert!(parse("branches(,a)").is_err());
        assert!(parse("branches(a,,)").is_err());
        assert!(parse("branches(a  , , )").is_err());
        assert!(parse("file(a,b,)").is_ok());
        assert!(parse("file(a,,b)").is_err());
        assert!(parse("remote_branches(a,remote=b  , )").is_ok());
        assert!(parse("remote_branches(a,,remote=b)").is_err());
    }

    #[test]
    fn test_parse_whitespace() {
        let ascii_whitespaces: String = ('\x00'..='\x7f')
            .filter(char::is_ascii_whitespace)
            .collect();
        assert_eq!(
            parse(&format!("{ascii_whitespaces}all()")).unwrap(),
            parse("all()").unwrap(),
        );
    }

    #[test]
    fn test_parse_revset_alias_formal_parameter() {
        let mut aliases_map = RevsetAliasesMap::new();
        // Trailing comma isn't allowed for empty parameter
        assert!(aliases_map.insert("f(,)", "none()").is_err());
        // Trailing comma is allowed for the last parameter
        assert!(aliases_map.insert("g(a,)", "none()").is_ok());
        assert!(aliases_map.insert("h(a ,  )", "none()").is_ok());
        assert!(aliases_map.insert("i(,a)", "none()").is_err());
        assert!(aliases_map.insert("j(a,,)", "none()").is_err());
        assert!(aliases_map.insert("k(a  , , )", "none()").is_err());
        assert!(aliases_map.insert("l(a,b,)", "none()").is_ok());
        assert!(aliases_map.insert("m(a,,b)", "none()").is_err());
    }

    #[test]
    fn test_parse_revset_compat_operator() {
        assert_eq!(
            parse("foo^"),
            Err(RevsetParseErrorKind::NotPostfixOperator {
                op: "^".to_owned(),
                similar_op: "-".to_owned(),
                description: "parents".to_owned(),
            })
        );
        assert_eq!(
            parse("foo + bar"),
            Err(RevsetParseErrorKind::NotInfixOperator {
                op: "+".to_owned(),
                similar_op: "|".to_owned(),
                description: "union".to_owned(),
            })
        );
        assert_eq!(
            parse("foo - bar"),
            Err(RevsetParseErrorKind::NotInfixOperator {
                op: "-".to_owned(),
                similar_op: "~".to_owned(),
                description: "difference".to_owned(),
            })
        );
    }

    #[test]
    fn test_parse_revset_operator_combinations() {
        let foo_symbol = RevsetExpression::symbol("foo".to_string());
        // Parse repeated "parents" operator
        assert_eq!(
            parse("foo---"),
            Ok(foo_symbol.parents().parents().parents())
        );
        // Parse repeated "children" operator
        assert_eq!(
            parse("foo+++"),
            Ok(foo_symbol.children().children().children())
        );
        // Set operator associativity/precedence
        assert_eq!(parse("~x|y").unwrap(), parse("(~x)|y").unwrap());
        assert_eq!(parse("x&~y").unwrap(), parse("x&(~y)").unwrap());
        assert_eq!(parse("x~~y").unwrap(), parse("x~(~y)").unwrap());
        assert_eq!(parse("x~~~y").unwrap(), parse("x~(~(~y))").unwrap());
        assert_eq!(parse("~x:y").unwrap(), parse("~(x:y)").unwrap());
        assert_eq!(parse("x|y|z").unwrap(), parse("(x|y)|z").unwrap());
        assert_eq!(parse("x&y|z").unwrap(), parse("(x&y)|z").unwrap());
        assert_eq!(parse("x|y&z").unwrap(), parse("x|(y&z)").unwrap());
        assert_eq!(parse("x|y~z").unwrap(), parse("x|(y~z)").unwrap());
        // Parse repeated "ancestors"/"descendants"/"dag range"/"range" operators
        assert_eq!(parse(":foo:"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("::foo"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("foo::"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("foo::bar"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse(":foo:bar"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("foo:bar:"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("....foo"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("foo...."), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("foo.....bar"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("..foo..bar"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(parse("foo..bar.."), Err(RevsetParseErrorKind::SyntaxError));
        // Parse combinations of "parents"/"children" operators and the range operators.
        // The former bind more strongly.
        assert_eq!(parse("foo-+"), Ok(foo_symbol.parents().children()));
        assert_eq!(parse("foo-:"), Ok(foo_symbol.parents().descendants()));
        assert_eq!(parse(":foo+"), Ok(foo_symbol.children().ancestors()));
    }

    #[test]
    fn test_parse_revset_function() {
        let wc_symbol = RevsetExpression::symbol("@".to_string());
        assert_eq!(parse("parents(@)"), Ok(wc_symbol.parents()));
        assert_eq!(parse("parents((@))"), Ok(wc_symbol.parents()));
        assert_eq!(parse("parents(\"@\")"), Ok(wc_symbol.parents()));
        assert_eq!(
            parse("ancestors(parents(@))"),
            Ok(wc_symbol.parents().ancestors())
        );
        assert_eq!(parse("parents(@"), Err(RevsetParseErrorKind::SyntaxError));
        assert_eq!(
            parse("parents(@,@)"),
            Err(RevsetParseErrorKind::InvalidFunctionArguments {
                name: "parents".to_string(),
                message: "Expected 1 arguments".to_string()
            })
        );
        assert_eq!(
            parse(r#"description("")"#),
            Ok(RevsetExpression::filter(
                RevsetFilterPredicate::Description("".to_string())
            ))
        );
        assert_eq!(
            parse("description(foo)"),
            Ok(RevsetExpression::filter(
                RevsetFilterPredicate::Description("foo".to_string())
            ))
        );
        assert_eq!(
            parse("description(heads())"),
            Err(RevsetParseErrorKind::InvalidFunctionArguments {
                name: "description".to_string(),
                message: "Expected function argument of type string".to_string()
            })
        );
        assert_eq!(
            parse("description((foo))"),
            Ok(RevsetExpression::filter(
                RevsetFilterPredicate::Description("foo".to_string())
            ))
        );
        assert_eq!(
            parse("description(\"(foo)\")"),
            Ok(RevsetExpression::filter(
                RevsetFilterPredicate::Description("(foo)".to_string())
            ))
        );
        assert_eq!(
            parse("empty()"),
            Ok(RevsetExpression::filter(RevsetFilterPredicate::File(None)).negated())
        );
        assert!(parse("empty(foo)").is_err());
        assert!(parse("file()").is_err());
        assert_eq!(
            parse("file(foo)"),
            Ok(RevsetExpression::filter(RevsetFilterPredicate::File(Some(
                vec![RepoPath::from_internal_string("foo")]
            ))))
        );
        assert_eq!(
            parse("file(foo, bar, baz)"),
            Ok(RevsetExpression::filter(RevsetFilterPredicate::File(Some(
                vec![
                    RepoPath::from_internal_string("foo"),
                    RepoPath::from_internal_string("bar"),
                    RepoPath::from_internal_string("baz"),
                ]
            ))))
        );
    }

    #[test]
    fn test_parse_revset_keyword_arguments() {
        assert_eq!(
            parse("remote_branches(remote=foo)").unwrap(),
            parse(r#"remote_branches("", foo)"#).unwrap(),
        );
        assert_eq!(
            parse("remote_branches(foo, remote=bar)").unwrap(),
            parse(r#"remote_branches(foo, bar)"#).unwrap(),
        );
        insta::assert_debug_snapshot!(
            parse(r#"remote_branches(remote=foo, bar)"#).unwrap_err(),
            @r###"
        InvalidFunctionArguments {
            name: "remote_branches",
            message: "Positional argument follows keyword argument",
        }
        "###);
        insta::assert_debug_snapshot!(
            parse(r#"remote_branches("", foo, remote=bar)"#).unwrap_err(),
            @r###"
        InvalidFunctionArguments {
            name: "remote_branches",
            message: "Got multiple values for keyword \"remote\"",
        }
        "###);
        insta::assert_debug_snapshot!(
            parse(r#"remote_branches(remote=bar, remote=bar)"#).unwrap_err(),
            @r###"
        InvalidFunctionArguments {
            name: "remote_branches",
            message: "Got multiple values for keyword \"remote\"",
        }
        "###);
        insta::assert_debug_snapshot!(
            parse(r#"remote_branches(unknown=bar)"#).unwrap_err(),
            @r###"
        InvalidFunctionArguments {
            name: "remote_branches",
            message: "Unexpected keyword argument \"unknown\"",
        }
        "###);
    }

    #[test]
    fn test_expand_symbol_alias() {
        assert_eq!(
            parse_with_aliases("AB|c", [("AB", "a|b")]).unwrap(),
            parse("(a|b)|c").unwrap()
        );
        assert_eq!(
            parse_with_aliases("AB:heads(AB)", [("AB", "a|b")]).unwrap(),
            parse("(a|b):heads(a|b)").unwrap()
        );

        // Not string substitution 'a&b|c', but tree substitution.
        assert_eq!(
            parse_with_aliases("a&BC", [("BC", "b|c")]).unwrap(),
            parse("a&(b|c)").unwrap()
        );

        // String literal should not be substituted with alias.
        assert_eq!(
            parse_with_aliases(r#"A|"A""#, [("A", "a")]).unwrap(),
            parse("a|A").unwrap()
        );

        // Alias can be substituted to string literal.
        assert_eq!(
            parse_with_aliases("author(A)", [("A", "a")]).unwrap(),
            parse("author(a)").unwrap()
        );

        // Multi-level substitution.
        assert_eq!(
            parse_with_aliases("A", [("A", "BC"), ("BC", "b|C"), ("C", "c")]).unwrap(),
            parse("b|c").unwrap()
        );

        // Infinite recursion, where the top-level error isn't of RecursiveAlias kind.
        assert_eq!(
            parse_with_aliases("A", [("A", "A")]),
            Err(RevsetParseErrorKind::BadAliasExpansion("A".to_owned()))
        );
        assert_eq!(
            parse_with_aliases("A", [("A", "B"), ("B", "b|C"), ("C", "c|B")]),
            Err(RevsetParseErrorKind::BadAliasExpansion("A".to_owned()))
        );

        // Error in alias definition.
        assert_eq!(
            parse_with_aliases("A", [("A", "a(")]),
            Err(RevsetParseErrorKind::BadAliasExpansion("A".to_owned()))
        );
    }

    #[test]
    fn test_expand_function_alias() {
        assert_eq!(
            parse_with_aliases("F()", [("F(  )", "a")]).unwrap(),
            parse("a").unwrap()
        );
        assert_eq!(
            parse_with_aliases("F(a)", [("F( x  )", "x")]).unwrap(),
            parse("a").unwrap()
        );
        assert_eq!(
            parse_with_aliases("F(a, b)", [("F( x,  y )", "x|y")]).unwrap(),
            parse("a|b").unwrap()
        );

        // Arguments should be resolved in the current scope.
        assert_eq!(
            parse_with_aliases("F(a:y,b:x)", [("F(x,y)", "x|y")]).unwrap(),
            parse("(a:y)|(b:x)").unwrap()
        );
        // F(a) -> G(a)&y -> (x|a)&y
        assert_eq!(
            parse_with_aliases("F(a)", [("F(x)", "G(x)&y"), ("G(y)", "x|y")]).unwrap(),
            parse("(x|a)&y").unwrap()
        );
        // F(G(a)) -> F(x|a) -> G(x|a)&y -> (x|(x|a))&y
        assert_eq!(
            parse_with_aliases("F(G(a))", [("F(x)", "G(x)&y"), ("G(y)", "x|y")]).unwrap(),
            parse("(x|(x|a))&y").unwrap()
        );

        // Function parameter should precede the symbol alias.
        assert_eq!(
            parse_with_aliases("F(a)|X", [("F(X)", "X"), ("X", "x")]).unwrap(),
            parse("a|x").unwrap()
        );

        // Function parameter shouldn't be expanded in symbol alias.
        assert_eq!(
            parse_with_aliases("F(a)", [("F(x)", "x|A"), ("A", "x")]).unwrap(),
            parse("a|x").unwrap()
        );

        // String literal should not be substituted with function parameter.
        assert_eq!(
            parse_with_aliases("F(a)", [("F(x)", r#"x|"x""#)]).unwrap(),
            parse("a|x").unwrap()
        );

        // Pass string literal as parameter.
        assert_eq!(
            parse_with_aliases("F(a)", [("F(x)", "author(x)|committer(x)")]).unwrap(),
            parse("author(a)|committer(a)").unwrap()
        );

        // Function and symbol aliases reside in separate namespaces.
        assert_eq!(
            parse_with_aliases("A()", [("A()", "A"), ("A", "a")]).unwrap(),
            parse("a").unwrap()
        );

        // Invalid number of arguments.
        assert_eq!(
            parse_with_aliases("F(a)", [("F()", "x")]),
            Err(RevsetParseErrorKind::InvalidFunctionArguments {
                name: "F".to_owned(),
                message: "Expected 0 arguments".to_owned()
            })
        );
        assert_eq!(
            parse_with_aliases("F()", [("F(x)", "x")]),
            Err(RevsetParseErrorKind::InvalidFunctionArguments {
                name: "F".to_owned(),
                message: "Expected 1 arguments".to_owned()
            })
        );
        assert_eq!(
            parse_with_aliases("F(a,b,c)", [("F(x,y)", "x|y")]),
            Err(RevsetParseErrorKind::InvalidFunctionArguments {
                name: "F".to_owned(),
                message: "Expected 2 arguments".to_owned()
            })
        );

        // Keyword argument isn't supported for now.
        assert_eq!(
            parse_with_aliases("F(x=y)", [("F(x)", "x")]),
            Err(RevsetParseErrorKind::InvalidFunctionArguments {
                name: "F".to_owned(),
                message: r#"Unexpected keyword argument "x""#.to_owned()
            })
        );

        // Infinite recursion, where the top-level error isn't of RecursiveAlias kind.
        assert_eq!(
            parse_with_aliases(
                "F(a)",
                [("F(x)", "G(x)"), ("G(x)", "H(x)"), ("H(x)", "F(x)")]
            ),
            Err(RevsetParseErrorKind::BadAliasExpansion("F()".to_owned()))
        );
    }

    #[test]
    fn test_optimize_subtree() {
        // Check that transform_expression_bottom_up() never rewrites enum variant
        // (e.g. Range -> DagRange) nor reorders arguments unintentionally.

        assert_eq!(
            optimize(parse("parents(branches() & all())").unwrap()),
            RevsetExpression::branches("".to_owned()).parents()
        );
        assert_eq!(
            optimize(parse("children(branches() & all())").unwrap()),
            RevsetExpression::branches("".to_owned()).children()
        );
        assert_eq!(
            optimize(parse("ancestors(branches() & all())").unwrap()),
            RevsetExpression::branches("".to_owned()).ancestors()
        );
        assert_eq!(
            optimize(parse("descendants(branches() & all())").unwrap()),
            RevsetExpression::branches("".to_owned()).descendants()
        );

        assert_eq!(
            optimize(parse("(branches() & all())..(all() & tags())").unwrap()),
            RevsetExpression::branches("".to_owned()).range(&RevsetExpression::tags())
        );
        assert_eq!(
            optimize(parse("(branches() & all()):(all() & tags())").unwrap()),
            RevsetExpression::branches("".to_owned()).dag_range_to(&RevsetExpression::tags())
        );

        assert_eq!(
            optimize(parse("heads(branches() & all())").unwrap()),
            RevsetExpression::branches("".to_owned()).heads()
        );
        assert_eq!(
            optimize(parse("roots(branches() & all())").unwrap()),
            RevsetExpression::branches("".to_owned()).roots()
        );

        assert_eq!(
            optimize(parse("present(author(foo) ~ bar)").unwrap()),
            Rc::new(RevsetExpression::AsFilter(Rc::new(
                RevsetExpression::Present(
                    RevsetExpression::filter(RevsetFilterPredicate::Author("foo".to_owned()))
                        .minus(&RevsetExpression::symbol("bar".to_owned()))
                )
            )))
        );
        assert_eq!(
            optimize(parse("present(branches() & all())").unwrap()),
            Rc::new(RevsetExpression::Present(RevsetExpression::branches(
                "".to_owned()
            )))
        );

        assert_eq!(
            optimize(parse("~branches() & all()").unwrap()),
            RevsetExpression::branches("".to_owned()).negated()
        );
        assert_eq!(
            optimize(parse("(branches() & all()) | (all() & tags())").unwrap()),
            RevsetExpression::branches("".to_owned()).union(&RevsetExpression::tags())
        );
        assert_eq!(
            optimize(parse("(branches() & all()) & (all() & tags())").unwrap()),
            RevsetExpression::branches("".to_owned()).intersection(&RevsetExpression::tags())
        );
        assert_eq!(
            optimize(parse("(branches() & all()) ~ (all() & tags())").unwrap()),
            RevsetExpression::branches("".to_owned()).minus(&RevsetExpression::tags())
        );
    }

    #[test]
    fn test_optimize_unchanged_subtree() {
        fn unwrap_union(
            expression: &RevsetExpression,
        ) -> (&Rc<RevsetExpression>, &Rc<RevsetExpression>) {
            match expression {
                RevsetExpression::Union(left, right) => (left, right),
                _ => panic!("unexpected expression: {expression:?}"),
            }
        }

        // transform_expression_bottom_up() should not recreate tree unnecessarily.
        let parsed = parse("foo-").unwrap();
        let optimized = optimize(parsed.clone());
        assert!(Rc::ptr_eq(&parsed, &optimized));

        let parsed = parse("branches() | tags()").unwrap();
        let optimized = optimize(parsed.clone());
        assert!(Rc::ptr_eq(&parsed, &optimized));

        let parsed = parse("branches() & tags()").unwrap();
        let optimized = optimize(parsed.clone());
        assert!(Rc::ptr_eq(&parsed, &optimized));

        // Only left subtree should be rewritten.
        let parsed = parse("(branches() & all()) | tags()").unwrap();
        let optimized = optimize(parsed.clone());
        assert_eq!(
            unwrap_union(&optimized).0.as_ref(),
            &RevsetExpression::Branches("".to_owned())
        );
        assert!(Rc::ptr_eq(
            unwrap_union(&parsed).1,
            unwrap_union(&optimized).1
        ));

        // Only right subtree should be rewritten.
        let parsed = parse("branches() | (all() & tags())").unwrap();
        let optimized = optimize(parsed.clone());
        assert!(Rc::ptr_eq(
            unwrap_union(&parsed).0,
            unwrap_union(&optimized).0
        ));
        assert_eq!(unwrap_union(&optimized).1.as_ref(), &RevsetExpression::Tags);
    }

    #[test]
    fn test_optimize_difference() {
        insta::assert_debug_snapshot!(optimize(parse("foo & ~bar").unwrap()), @r###"
        Difference(
            Symbol(
                "foo",
            ),
            Symbol(
                "bar",
            ),
        )
        "###);
        insta::assert_debug_snapshot!(optimize(parse("~foo & bar").unwrap()), @r###"
        Difference(
            Symbol(
                "bar",
            ),
            Symbol(
                "foo",
            ),
        )
        "###);
        insta::assert_debug_snapshot!(optimize(parse("~foo & bar & ~baz").unwrap()), @r###"
        Difference(
            Difference(
                Symbol(
                    "bar",
                ),
                Symbol(
                    "foo",
                ),
            ),
            Symbol(
                "baz",
            ),
        )
        "###);
        insta::assert_debug_snapshot!(optimize(parse("(all() & ~foo) & bar").unwrap()), @r###"
        Difference(
            Symbol(
                "bar",
            ),
            Symbol(
                "foo",
            ),
        )
        "###);

        // Binary difference operation should go through the same optimization passes.
        insta::assert_debug_snapshot!(optimize(parse("all() ~ foo").unwrap()), @r###"
        NotIn(
            Symbol(
                "foo",
            ),
        )
        "###);
        insta::assert_debug_snapshot!(optimize(parse("foo ~ bar").unwrap()), @r###"
        Difference(
            Symbol(
                "foo",
            ),
            Symbol(
                "bar",
            ),
        )
        "###);
        insta::assert_debug_snapshot!(optimize(parse("(all() ~ foo) & bar").unwrap()), @r###"
        Difference(
            Symbol(
                "bar",
            ),
            Symbol(
                "foo",
            ),
        )
        "###);

        // Range expression.
        insta::assert_debug_snapshot!(optimize(parse(":foo & ~:bar").unwrap()), @r###"
        Range {
            roots: Symbol(
                "bar",
            ),
            heads: Symbol(
                "foo",
            ),
            generation: 0..4294967295,
        }
        "###);
        insta::assert_debug_snapshot!(optimize(parse("~:foo & :bar").unwrap()), @r###"
        Range {
            roots: Symbol(
                "foo",
            ),
            heads: Symbol(
                "bar",
            ),
            generation: 0..4294967295,
        }
        "###);
        insta::assert_debug_snapshot!(optimize(parse("foo..").unwrap()), @r###"
        Range {
            roots: Symbol(
                "foo",
            ),
            heads: VisibleHeads,
            generation: 0..4294967295,
        }
        "###);
        insta::assert_debug_snapshot!(optimize(parse("foo..bar").unwrap()), @r###"
        Range {
            roots: Symbol(
                "foo",
            ),
            heads: Symbol(
                "bar",
            ),
            generation: 0..4294967295,
        }
        "###);

        // Double/triple negates.
        insta::assert_debug_snapshot!(optimize(parse("foo & ~~bar").unwrap()), @r###"
        Intersection(
            Symbol(
                "foo",
            ),
            Symbol(
                "bar",
            ),
        )
        "###);
        insta::assert_debug_snapshot!(optimize(parse("foo & ~~~bar").unwrap()), @r###"
        Difference(
            Symbol(
                "foo",
            ),
            Symbol(
                "bar",
            ),
        )
        "###);
        insta::assert_debug_snapshot!(optimize(parse("~(all() & ~foo) & bar").unwrap()), @r###"
        Intersection(
            Symbol(
                "foo",
            ),
            Symbol(
                "bar",
            ),
        )
        "###);

        // Should be better than '(all() & ~foo) & (all() & ~bar)'.
        insta::assert_debug_snapshot!(optimize(parse("~foo & ~bar").unwrap()), @r###"
        Difference(
            NotIn(
                Symbol(
                    "foo",
                ),
            ),
            Symbol(
                "bar",
            ),
        )
        "###);
    }

    #[test]
    fn test_optimize_filter_difference() {
        // '~empty()' -> '~~file(*)' -> 'file(*)'
        insta::assert_debug_snapshot!(optimize(parse("~empty()").unwrap()), @r###"
        Filter(
            File(
                None,
            ),
        )
        "###);

        // '& baz' can be moved into the filter node, and form a difference node.
        insta::assert_debug_snapshot!(
            optimize(parse("(author(foo) & ~bar) & baz").unwrap()), @r###"
        Intersection(
            Difference(
                Symbol(
                    "baz",
                ),
                Symbol(
                    "bar",
                ),
            ),
            Filter(
                Author(
                    "foo",
                ),
            ),
        )
        "###)
    }

    #[test]
    fn test_optimize_filter_intersection() {
        insta::assert_debug_snapshot!(optimize(parse("author(foo)").unwrap()), @r###"
        Filter(
            Author(
                "foo",
            ),
        )
        "###);

        insta::assert_debug_snapshot!(optimize(parse("foo & description(bar)").unwrap()), @r###"
        Intersection(
            Symbol(
                "foo",
            ),
            Filter(
                Description(
                    "bar",
                ),
            ),
        )
        "###);
        insta::assert_debug_snapshot!(optimize(parse("author(foo) & bar").unwrap()), @r###"
        Intersection(
            Symbol(
                "bar",
            ),
            Filter(
                Author(
                    "foo",
                ),
            ),
        )
        "###);
        insta::assert_debug_snapshot!(
            optimize(parse("author(foo) & committer(bar)").unwrap()), @r###"
        Intersection(
            Filter(
                Author(
                    "foo",
                ),
            ),
            Filter(
                Committer(
                    "bar",
                ),
            ),
        )
        "###);

        insta::assert_debug_snapshot!(
            optimize(parse("foo & description(bar) & author(baz)").unwrap()), @r###"
        Intersection(
            Intersection(
                Symbol(
                    "foo",
                ),
                Filter(
                    Description(
                        "bar",
                    ),
                ),
            ),
            Filter(
                Author(
                    "baz",
                ),
            ),
        )
        "###);
        insta::assert_debug_snapshot!(
            optimize(parse("committer(foo) & bar & author(baz)").unwrap()), @r###"
        Intersection(
            Intersection(
                Symbol(
                    "bar",
                ),
                Filter(
                    Committer(
                        "foo",
                    ),
                ),
            ),
            Filter(
                Author(
                    "baz",
                ),
            ),
        )
        "###);
        insta::assert_debug_snapshot!(
            optimize(parse("committer(foo) & file(bar) & baz").unwrap()), @r###"
        Intersection(
            Intersection(
                Symbol(
                    "baz",
                ),
                Filter(
                    Committer(
                        "foo",
                    ),
                ),
            ),
            Filter(
                File(
                    Some(
                        [
                            "bar",
                        ],
                    ),
                ),
            ),
        )
        "###);
        insta::assert_debug_snapshot!(
            optimize(parse("committer(foo) & file(bar) & author(baz)").unwrap()), @r###"
        Intersection(
            Intersection(
                Filter(
                    Committer(
                        "foo",
                    ),
                ),
                Filter(
                    File(
                        Some(
                            [
                                "bar",
                            ],
                        ),
                    ),
                ),
            ),
            Filter(
                Author(
                    "baz",
                ),
            ),
        )
        "###);
        insta::assert_debug_snapshot!(optimize(parse("foo & file(bar) & baz").unwrap()), @r###"
        Intersection(
            Intersection(
                Symbol(
                    "foo",
                ),
                Symbol(
                    "baz",
                ),
            ),
            Filter(
                File(
                    Some(
                        [
                            "bar",
                        ],
                    ),
                ),
            ),
        )
        "###);

        insta::assert_debug_snapshot!(
            optimize(parse("foo & description(bar) & author(baz) & qux").unwrap()), @r###"
        Intersection(
            Intersection(
                Intersection(
                    Symbol(
                        "foo",
                    ),
                    Symbol(
                        "qux",
                    ),
                ),
                Filter(
                    Description(
                        "bar",
                    ),
                ),
            ),
            Filter(
                Author(
                    "baz",
                ),
            ),
        )
        "###);
        insta::assert_debug_snapshot!(
            optimize(parse("foo & description(bar) & parents(author(baz)) & qux").unwrap()), @r###"
        Intersection(
            Intersection(
                Intersection(
                    Symbol(
                        "foo",
                    ),
                    Ancestors {
                        heads: Filter(
                            Author(
                                "baz",
                            ),
                        ),
                        generation: 1..2,
                    },
                ),
                Symbol(
                    "qux",
                ),
            ),
            Filter(
                Description(
                    "bar",
                ),
            ),
        )
        "###);
        insta::assert_debug_snapshot!(
            optimize(parse("foo & description(bar) & parents(author(baz) & qux)").unwrap()), @r###"
        Intersection(
            Intersection(
                Symbol(
                    "foo",
                ),
                Ancestors {
                    heads: Intersection(
                        Symbol(
                            "qux",
                        ),
                        Filter(
                            Author(
                                "baz",
                            ),
                        ),
                    ),
                    generation: 1..2,
                },
            ),
            Filter(
                Description(
                    "bar",
                ),
            ),
        )
        "###);

        // Symbols have to be pushed down to the innermost filter node.
        insta::assert_debug_snapshot!(
            optimize(parse("(a & author(A)) & (b & author(B)) & (c & author(C))").unwrap()), @r###"
        Intersection(
            Intersection(
                Intersection(
                    Intersection(
                        Intersection(
                            Symbol(
                                "a",
                            ),
                            Symbol(
                                "b",
                            ),
                        ),
                        Symbol(
                            "c",
                        ),
                    ),
                    Filter(
                        Author(
                            "A",
                        ),
                    ),
                ),
                Filter(
                    Author(
                        "B",
                    ),
                ),
            ),
            Filter(
                Author(
                    "C",
                ),
            ),
        )
        "###);
        insta::assert_debug_snapshot!(
            optimize(parse("(a & author(A)) & ((b & author(B)) & (c & author(C))) & d").unwrap()),
            @r###"
        Intersection(
            Intersection(
                Intersection(
                    Intersection(
                        Intersection(
                            Symbol(
                                "a",
                            ),
                            Intersection(
                                Symbol(
                                    "b",
                                ),
                                Symbol(
                                    "c",
                                ),
                            ),
                        ),
                        Symbol(
                            "d",
                        ),
                    ),
                    Filter(
                        Author(
                            "A",
                        ),
                    ),
                ),
                Filter(
                    Author(
                        "B",
                    ),
                ),
            ),
            Filter(
                Author(
                    "C",
                ),
            ),
        )
        "###);

        // 'all()' moves in to 'filter()' first, so 'A & filter()' can be found.
        insta::assert_debug_snapshot!(
            optimize(parse("foo & (all() & description(bar)) & (author(baz) & all())").unwrap()),
            @r###"
        Intersection(
            Intersection(
                Symbol(
                    "foo",
                ),
                Filter(
                    Description(
                        "bar",
                    ),
                ),
            ),
            Filter(
                Author(
                    "baz",
                ),
            ),
        )
        "###);
    }

    #[test]
    fn test_optimize_filter_subtree() {
        insta::assert_debug_snapshot!(
            optimize(parse("(author(foo) | bar) & baz").unwrap()), @r###"
        Intersection(
            Symbol(
                "baz",
            ),
            AsFilter(
                Union(
                    Filter(
                        Author(
                            "foo",
                        ),
                    ),
                    Symbol(
                        "bar",
                    ),
                ),
            ),
        )
        "###);

        insta::assert_debug_snapshot!(
            optimize(parse("(foo | committer(bar)) & description(baz) & qux").unwrap()), @r###"
        Intersection(
            Intersection(
                Symbol(
                    "qux",
                ),
                AsFilter(
                    Union(
                        Symbol(
                            "foo",
                        ),
                        Filter(
                            Committer(
                                "bar",
                            ),
                        ),
                    ),
                ),
            ),
            Filter(
                Description(
                    "baz",
                ),
            ),
        )
        "###);

        insta::assert_debug_snapshot!(
            optimize(parse("(~present(author(foo) & bar) | baz) & qux").unwrap()), @r###"
        Intersection(
            Symbol(
                "qux",
            ),
            AsFilter(
                Union(
                    AsFilter(
                        NotIn(
                            AsFilter(
                                Present(
                                    Intersection(
                                        Symbol(
                                            "bar",
                                        ),
                                        Filter(
                                            Author(
                                                "foo",
                                            ),
                                        ),
                                    ),
                                ),
                            ),
                        ),
                    ),
                    Symbol(
                        "baz",
                    ),
                ),
            ),
        )
        "###);

        // Symbols have to be pushed down to the innermost filter node.
        insta::assert_debug_snapshot!(
            optimize(parse(
                "(a & (author(A) | 0)) & (b & (author(B) | 1)) & (c & (author(C) | 2))").unwrap()),
            @r###"
        Intersection(
            Intersection(
                Intersection(
                    Intersection(
                        Intersection(
                            Symbol(
                                "a",
                            ),
                            Symbol(
                                "b",
                            ),
                        ),
                        Symbol(
                            "c",
                        ),
                    ),
                    AsFilter(
                        Union(
                            Filter(
                                Author(
                                    "A",
                                ),
                            ),
                            Symbol(
                                "0",
                            ),
                        ),
                    ),
                ),
                AsFilter(
                    Union(
                        Filter(
                            Author(
                                "B",
                            ),
                        ),
                        Symbol(
                            "1",
                        ),
                    ),
                ),
            ),
            AsFilter(
                Union(
                    Filter(
                        Author(
                            "C",
                        ),
                    ),
                    Symbol(
                        "2",
                    ),
                ),
            ),
        )
        "###);
    }

    #[test]
    fn test_optimize_ancestors() {
        // Typical scenario: fold nested parents()
        insta::assert_debug_snapshot!(optimize(parse("foo--").unwrap()), @r###"
        Ancestors {
            heads: Symbol(
                "foo",
            ),
            generation: 2..3,
        }
        "###);
        insta::assert_debug_snapshot!(optimize(parse(":(foo---)").unwrap()), @r###"
        Ancestors {
            heads: Symbol(
                "foo",
            ),
            generation: 3..4294967295,
        }
        "###);
        insta::assert_debug_snapshot!(optimize(parse("(:foo)---").unwrap()), @r###"
        Ancestors {
            heads: Symbol(
                "foo",
            ),
            generation: 3..4294967295,
        }
        "###);

        // 'foo-+' is not 'foo'.
        insta::assert_debug_snapshot!(optimize(parse("foo---+").unwrap()), @r###"
        Children(
            Ancestors {
                heads: Symbol(
                    "foo",
                ),
                generation: 3..4,
            },
        )
        "###);

        // For 'roots..heads', heads can be folded.
        insta::assert_debug_snapshot!(optimize(parse("foo..(bar--)").unwrap()), @r###"
        Range {
            roots: Symbol(
                "foo",
            ),
            heads: Symbol(
                "bar",
            ),
            generation: 2..4294967295,
        }
        "###);
        // roots can also be folded, but range expression cannot be reconstructed.
        // No idea if this is better than the original range expression.
        insta::assert_debug_snapshot!(optimize(parse("(foo--)..(bar---)").unwrap()), @r###"
        Difference(
            Ancestors {
                heads: Symbol(
                    "bar",
                ),
                generation: 3..4294967295,
            },
            Ancestors {
                heads: Symbol(
                    "foo",
                ),
                generation: 2..4294967295,
            },
        )
        "###);

        // If inner range is bounded by roots, it cannot be merged.
        // e.g. '..(foo..foo)' is equivalent to '..none()', not to '..foo'
        insta::assert_debug_snapshot!(optimize(parse("(foo..bar)--").unwrap()), @r###"
        Ancestors {
            heads: Range {
                roots: Symbol(
                    "foo",
                ),
                heads: Symbol(
                    "bar",
                ),
                generation: 0..4294967295,
            },
            generation: 2..3,
        }
        "###);
        insta::assert_debug_snapshot!(optimize(parse("foo..(bar..baz)").unwrap()), @r###"
        Range {
            roots: Symbol(
                "foo",
            ),
            heads: Range {
                roots: Symbol(
                    "bar",
                ),
                heads: Symbol(
                    "baz",
                ),
                generation: 0..4294967295,
            },
            generation: 0..4294967295,
        }
        "###);

        // Ancestors of empty generation range should be empty.
        // TODO: rewrite these tests if we added syntax for arbitrary generation
        // ancestors
        let empty_generation_ancestors = |heads| {
            Rc::new(RevsetExpression::Ancestors {
                heads,
                generation: GENERATION_RANGE_EMPTY,
            })
        };
        insta::assert_debug_snapshot!(
            optimize(empty_generation_ancestors(
                RevsetExpression::symbol("foo".to_owned()).ancestors()
            )),
            @r###"
        Ancestors {
            heads: Symbol(
                "foo",
            ),
            generation: 0..0,
        }
        "###
        );
        insta::assert_debug_snapshot!(
            optimize(
                empty_generation_ancestors(RevsetExpression::symbol("foo".to_owned())).ancestors()
            ),
            @r###"
        Ancestors {
            heads: Symbol(
                "foo",
            ),
            generation: 0..0,
        }
        "###
        );
    }
}
