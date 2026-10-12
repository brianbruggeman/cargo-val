//! The nextest `-E` filterset subset: `test() binary() package() all() none()` with `and`/`&`,
//! `or`/`|`/`+`, `not`/`!`, `-` and parentheses, matchers `=exact`, `~contains`, `#glob`. A `/regex/`
//! matcher, which nextest has, is deliberately refused at parse time: no regex engine fits the
//! `no_std`, no-heap binary.
//!
//! Parsing is a byte-fed state machine over fixed storage ([`Filterset::parse`] drives it);
//! evaluation is a pure function of a [`Candidate`]. Like nextest, `test()` defaults to
//! contains and `binary()` / `package()` to glob.

use crate::filter_glob::{GlobFault, glob_matches, glob_validate};
use memchr::memmem::Finder;

const MAX_NODES: usize = 64;
const MAX_STACK: usize = 32;
const POOL_CAPACITY: usize = 1024;
const WORD_CAPACITY: usize = 8;
const MAX_HEX_DIGITS: u8 = 6;

/// Which part of a test a predicate reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The libtest name such as `bind::tests::alpha`.
    Test,
    /// The cargo target name such as `capability_matrix`, or the crate name for unit tests.
    Binary,
    /// The package name such as `proxima-model-interop`.
    Package,
}

/// How a matcher compares its pattern with a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Contains,
    Equals,
    Glob,
}

impl Field {
    const fn default_strategy(self) -> Strategy {
        match self {
            Self::Test => Strategy::Contains,
            Self::Binary | Self::Package => Strategy::Glob,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Matcher {
    strategy: Strategy,
    start: u16,
    length: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    All,
    Nothing,
    Predicate(Field, Matcher),
    Not(u8),
    And(u8, u8),
    Or(u8, u8),
    Diff(u8, u8),
}

/// One test as the filterset sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate<'text> {
    /// package name such as `proxima-model-interop`.
    pub package: &'text str,
    /// cargo target name such as `capability_matrix`.
    pub binary: &'text str,
    /// libtest name such as `bind::tests::architecture_from_metadata_names_the_missing_key`.
    pub test: &'text str,
}

/// Why an expression was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterErrorKind {
    ExpectedOperand,
    ExpectedOperator,
    ExpectedOpenParen,
    ExpectedCloseParen,
    UnknownPredicate,
    UnmatchedClose,
    UnclosedGroup,
    UnexpectedEnd,
    EmptyMatcher,
    BadEscape,
    TooLarge,
    Glob(GlobFault),
    RegexUnsupported,
}

/// A refusal and the byte offset in the expression where it was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterError {
    pub kind: FilterErrorKind,
    pub offset: usize,
}

impl FilterErrorKind {
    #[must_use]
    pub const fn message(self) -> &'static [u8] {
        match self {
            Self::ExpectedOperand => b"expected a predicate, `(`, `not` or `!`",
            Self::ExpectedOperator => b"expected `and`, `or`, `&`, `|`, `+`, `-` or `)`",
            Self::ExpectedOpenParen => b"expected `(` after the predicate name",
            Self::ExpectedCloseParen => b"expected `)`",
            Self::UnknownPredicate => b"unknown predicate; this subset has test, binary, package, all, none",
            Self::UnmatchedClose => b"`)` has no matching `(`",
            Self::UnclosedGroup => b"`(` is never closed",
            Self::UnexpectedEnd => b"expression ends before it is complete",
            Self::EmptyMatcher => b"matcher is empty",
            Self::BadEscape => b"escape must be one of \\n \\r \\t \\\\ \\/ \\) \\, \\u{...}",
            Self::TooLarge => b"expression exceeds the fixed storage (64 nodes, 32 nesting, 1024 matcher bytes)",
            Self::Glob(GlobFault::UnclosedClass { .. }) => b"glob class `[` is not closed",
            Self::Glob(GlobFault::Alternation { .. }) => b"glob `{a,b}` alternation is not supported",
            Self::RegexUnsupported => b"regex matchers are not supported; use contains (bare or ~text), exact (=text) or glob (#pattern)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperatorKind {
    Group,
    Not,
    And,
    Or,
    Diff,
}

impl OperatorKind {
    const fn precedence(self) -> u8 {
        match self {
            Self::Group => 0,
            Self::Or => 1,
            Self::And | Self::Diff => 2,
            Self::Not => 3,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Operator {
    kind: OperatorKind,
    offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordContext {
    Operand,
    Operator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    Backslash,
    Unicode { value: u32, digits: u8, opened: bool },
}

#[derive(Debug, Clone, Copy)]
enum State {
    Operand,
    Operator,
    Word {
        context: WordContext,
        buffer: [u8; WORD_CAPACITY],
        length: usize,
        start: usize,
    },
    NoArguments { all: bool },
    ArgumentStart { field: Field },
    ArgumentText(TextState),
}

#[derive(Debug, Clone, Copy)]
struct TextState {
    field: Field,
    strategy: Strategy,
    start: usize,
    trimmed: usize,
    origin: usize,
    escape: Escape,
}

enum Step {
    Consumed,
    Again,
}

/// A parsed expression: a small arena of nodes and the matcher text.
#[derive(Debug, Clone)]
pub struct Filterset {
    nodes: [Node; MAX_NODES],
    root: u8,
    pool: [u8; POOL_CAPACITY],
}

struct Parser {
    nodes: [Node; MAX_NODES],
    node_count: usize,
    pool: [u8; POOL_CAPACITY],
    pool_length: usize,
    operands: [u8; MAX_STACK + 1],
    operand_count: usize,
    operators: [Operator; MAX_STACK],
    operator_count: usize,
    state: State,
}

fn error<T>(kind: FilterErrorKind, offset: usize) -> Result<T, FilterError> {
    Err(FilterError { kind, offset })
}

fn hex_digit(byte: u8) -> Option<u32> {
    char::from(byte).to_digit(16)
}

impl Parser {
    fn new() -> Self {
        Self {
            nodes: [Node::All; MAX_NODES],
            node_count: 0,
            pool: [0; POOL_CAPACITY],
            pool_length: 0,
            operands: [0; MAX_STACK + 1],
            operand_count: 0,
            operators: [Operator {
                kind: OperatorKind::Group,
                offset: 0,
            }; MAX_STACK],
            operator_count: 0,
            state: State::Operand,
        }
    }

    fn push(&mut self, offset: usize, byte: u8) -> Result<(), FilterError> {
        while matches!(self.step(offset, byte)?, Step::Again) {}
        Ok(())
    }

    fn step(&mut self, offset: usize, byte: u8) -> Result<Step, FilterError> {
        match self.state {
            State::Operand => self.operand(offset, byte),
            State::Operator => self.operator(offset, byte),
            State::Word {
                context,
                buffer,
                length,
                start,
            } => self.word(context, buffer, length, start, offset, byte),
            State::NoArguments { all } => self.no_arguments(all, offset, byte),
            State::ArgumentStart { field } => self.argument_start(field, offset, byte),
            State::ArgumentText(text) => self.argument_text(text, offset, byte),
        }
    }

    fn operand(&mut self, offset: usize, byte: u8) -> Result<Step, FilterError> {
        match byte {
            _ if byte.is_ascii_whitespace() => {}
            b'(' => self.push_operator(OperatorKind::Group, offset)?,
            b'!' => self.push_operator(OperatorKind::Not, offset)?,
            _ if byte.is_ascii_alphabetic() => self.begin_word(WordContext::Operand, offset, byte),
            _ => return error(FilterErrorKind::ExpectedOperand, offset),
        }
        Ok(Step::Consumed)
    }

    fn operator(&mut self, offset: usize, byte: u8) -> Result<Step, FilterError> {
        match byte {
            _ if byte.is_ascii_whitespace() => {}
            b')' => self.close_group(offset)?,
            b'&' => self.binary(OperatorKind::And, offset)?,
            b'|' | b'+' => self.binary(OperatorKind::Or, offset)?,
            b'-' => self.binary(OperatorKind::Diff, offset)?,
            _ if byte.is_ascii_alphabetic() => self.begin_word(WordContext::Operator, offset, byte),
            _ => return error(FilterErrorKind::ExpectedOperator, offset),
        }
        Ok(Step::Consumed)
    }

    fn begin_word(&mut self, context: WordContext, start: usize, byte: u8) {
        let mut buffer = [0; WORD_CAPACITY];
        buffer[0] = byte;
        self.state = State::Word {
            context,
            buffer,
            length: 1,
            start,
        };
    }

    fn word(
        &mut self,
        context: WordContext,
        mut buffer: [u8; WORD_CAPACITY],
        length: usize,
        start: usize,
        offset: usize,
        byte: u8,
    ) -> Result<Step, FilterError> {
        if byte.is_ascii_alphabetic() || byte == b'_' {
            if length < WORD_CAPACITY {
                buffer[length] = byte;
            }
            self.state = State::Word {
                context,
                buffer,
                length: length + 1,
                start,
            };
            return Ok(Step::Consumed);
        }
        let word = buffer.get(..length).unwrap_or(&[]);
        match (context, word) {
            (WordContext::Operand, b"not") => {
                self.push_operator(OperatorKind::Not, start)?;
                self.state = State::Operand;
                Ok(Step::Again)
            }
            (WordContext::Operator, b"and") => {
                self.binary(OperatorKind::And, start)?;
                Ok(Step::Again)
            }
            (WordContext::Operator, b"or") => {
                self.binary(OperatorKind::Or, start)?;
                Ok(Step::Again)
            }
            (WordContext::Operator, _) => error(FilterErrorKind::ExpectedOperator, start),
            (WordContext::Operand, name) => self.predicate(name, start, offset, byte),
        }
    }

    fn predicate(
        &mut self,
        name: &[u8],
        start: usize,
        offset: usize,
        byte: u8,
    ) -> Result<Step, FilterError> {
        let next = match name {
            b"all" => State::NoArguments { all: true },
            b"none" => State::NoArguments { all: false },
            b"test" => State::ArgumentStart { field: Field::Test },
            b"binary" => State::ArgumentStart {
                field: Field::Binary,
            },
            b"package" => State::ArgumentStart {
                field: Field::Package,
            },
            _ => return error(FilterErrorKind::UnknownPredicate, start),
        };
        if byte != b'(' {
            return error(FilterErrorKind::ExpectedOpenParen, offset);
        }
        self.state = next;
        Ok(Step::Consumed)
    }

    fn no_arguments(&mut self, all: bool, offset: usize, byte: u8) -> Result<Step, FilterError> {
        if byte.is_ascii_whitespace() {
            return Ok(Step::Consumed);
        }
        if byte != b')' {
            return error(FilterErrorKind::ExpectedCloseParen, offset);
        }
        let node = if all { Node::All } else { Node::Nothing };
        self.finish_operand(node, offset)?;
        Ok(Step::Consumed)
    }

    fn argument_start(
        &mut self,
        field: Field,
        offset: usize,
        byte: u8,
    ) -> Result<Step, FilterError> {
        let strategy = match byte {
            _ if byte.is_ascii_whitespace() => return Ok(Step::Consumed),
            b')' => return error(FilterErrorKind::EmptyMatcher, offset),
            b'/' => return error(FilterErrorKind::RegexUnsupported, offset),
            b'=' => Strategy::Equals,
            b'~' => Strategy::Contains,
            b'#' => Strategy::Glob,
            _ => {
                self.begin_text(field, field.default_strategy(), offset);
                return Ok(Step::Again);
            }
        };
        self.begin_text(field, strategy, offset + 1);
        Ok(Step::Consumed)
    }

    fn begin_text(&mut self, field: Field, strategy: Strategy, origin: usize) {
        self.state = State::ArgumentText(TextState {
            field,
            strategy,
            start: self.pool_length,
            trimmed: self.pool_length,
            origin,
            escape: Escape::None,
        });
    }

    fn append(&mut self, offset: usize, byte: u8) -> Result<(), FilterError> {
        if self.pool_length == POOL_CAPACITY {
            return error(FilterErrorKind::TooLarge, offset);
        }
        self.pool[self.pool_length] = byte;
        self.pool_length += 1;
        Ok(())
    }

    fn argument_text(
        &mut self,
        text: TextState,
        offset: usize,
        byte: u8,
    ) -> Result<Step, FilterError> {
        let mut next = text;
        next.escape = match text.escape {
            Escape::None => match byte {
                b'\\' => Escape::Backslash,
                b')' => return self.finish_text(text, offset),
                _ if byte.is_ascii_whitespace() && self.pool_length == text.start => Escape::None,
                _ => {
                    self.append(offset, byte)?;
                    if !byte.is_ascii_whitespace() {
                        next.trimmed = self.pool_length;
                    }
                    Escape::None
                }
            },
            Escape::Backslash => self.simple_escape(offset, byte, &mut next.trimmed)?,
            Escape::Unicode {
                value,
                digits,
                opened,
            } => self.unicode_escape(value, digits, opened, offset, byte, &mut next.trimmed)?,
        };
        self.state = State::ArgumentText(next);
        Ok(Step::Consumed)
    }

    fn simple_escape(
        &mut self,
        offset: usize,
        byte: u8,
        trimmed: &mut usize,
    ) -> Result<Escape, FilterError> {
        let decoded = match byte {
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'\\' | b'/' | b')' | b',' => byte,
            b'u' => {
                return Ok(Escape::Unicode {
                    value: 0,
                    digits: 0,
                    opened: false,
                });
            }
            _ => return error(FilterErrorKind::BadEscape, offset),
        };
        self.append(offset, decoded)?;
        *trimmed = self.pool_length;
        Ok(Escape::None)
    }

    fn unicode_escape(
        &mut self,
        value: u32,
        digits: u8,
        opened: bool,
        offset: usize,
        byte: u8,
        trimmed: &mut usize,
    ) -> Result<Escape, FilterError> {
        if !opened {
            return if byte == b'{' {
                Ok(Escape::Unicode {
                    value,
                    digits,
                    opened: true,
                })
            } else {
                error(FilterErrorKind::BadEscape, offset)
            };
        }
        if let Some(digit) = hex_digit(byte) {
            if digits == MAX_HEX_DIGITS {
                return error(FilterErrorKind::BadEscape, offset);
            }
            return Ok(Escape::Unicode {
                value: value * 16 + digit,
                digits: digits + 1,
                opened,
            });
        }
        let Some(character) = char::from_u32(value).filter(|_| byte == b'}' && digits > 0) else {
            return error(FilterErrorKind::BadEscape, offset);
        };
        let mut encoded = [0; 4];
        for unit in character.encode_utf8(&mut encoded).bytes() {
            self.append(offset, unit)?;
        }
        *trimmed = self.pool_length;
        Ok(Escape::None)
    }

    fn finish_text(&mut self, text: TextState, offset: usize) -> Result<Step, FilterError> {
        self.pool_length = text.trimmed;
        if text.trimmed == text.start {
            return error(FilterErrorKind::EmptyMatcher, offset);
        }
        if text.strategy == Strategy::Glob
            && let Err(fault) = glob_validate(&self.pool[text.start..text.trimmed])
        {
            let (GlobFault::UnclosedClass { offset: inside } | GlobFault::Alternation { offset: inside }) = fault;
            return error(FilterErrorKind::Glob(fault), text.origin + inside);
        }
        let matcher = Matcher {
            strategy: text.strategy,
            start: u16::try_from(text.start).unwrap_or(u16::MAX),
            length: u16::try_from(text.trimmed - text.start).unwrap_or(u16::MAX),
        };
        self.finish_operand(Node::Predicate(text.field, matcher), offset)?;
        Ok(Step::Consumed)
    }

    fn add_node(&mut self, node: Node, offset: usize) -> Result<u8, FilterError> {
        if self.node_count == MAX_NODES {
            return error(FilterErrorKind::TooLarge, offset);
        }
        self.nodes[self.node_count] = node;
        self.node_count += 1;
        Ok(u8::try_from(self.node_count - 1).unwrap_or(u8::MAX))
    }

    fn finish_operand(&mut self, node: Node, offset: usize) -> Result<(), FilterError> {
        let index = self.add_node(node, offset)?;
        self.operands[self.operand_count] = index;
        self.operand_count += 1;
        self.state = State::Operator;
        Ok(())
    }

    fn push_operator(&mut self, kind: OperatorKind, offset: usize) -> Result<(), FilterError> {
        if self.operator_count == MAX_STACK || self.operand_count == MAX_STACK {
            return error(FilterErrorKind::TooLarge, offset);
        }
        self.operators[self.operator_count] = Operator { kind, offset };
        self.operator_count += 1;
        Ok(())
    }

    fn binary(&mut self, kind: OperatorKind, offset: usize) -> Result<(), FilterError> {
        while let Some(top) = self.top_operator()
            && top.kind != OperatorKind::Group
            && top.kind.precedence() >= kind.precedence()
        {
            self.reduce(top)?;
        }
        self.push_operator(kind, offset)?;
        self.state = State::Operand;
        Ok(())
    }

    fn top_operator(&self) -> Option<Operator> {
        self.operator_count
            .checked_sub(1)
            .map(|index| self.operators[index])
    }

    fn pop_operand(&mut self, offset: usize) -> Result<u8, FilterError> {
        self.operand_count = self
            .operand_count
            .checked_sub(1)
            .ok_or(FilterError {
                kind: FilterErrorKind::UnexpectedEnd,
                offset,
            })?;
        Ok(self.operands[self.operand_count])
    }

    fn reduce(&mut self, operator: Operator) -> Result<(), FilterError> {
        self.operator_count -= 1;
        let offset = operator.offset;
        let node = if operator.kind == OperatorKind::Not {
            Node::Not(self.pop_operand(offset)?)
        } else {
            let right = self.pop_operand(offset)?;
            let left = self.pop_operand(offset)?;
            match operator.kind {
                OperatorKind::And => Node::And(left, right),
                OperatorKind::Or => Node::Or(left, right),
                _ => Node::Diff(left, right),
            }
        };
        let index = self.add_node(node, offset)?;
        self.operands[self.operand_count] = index;
        self.operand_count += 1;
        Ok(())
    }

    fn close_group(&mut self, offset: usize) -> Result<(), FilterError> {
        loop {
            let Some(top) = self.top_operator() else {
                return error(FilterErrorKind::UnmatchedClose, offset);
            };
            if top.kind == OperatorKind::Group {
                self.operator_count -= 1;
                return Ok(());
            }
            self.reduce(top)?;
        }
    }

    fn finish(mut self, length: usize) -> Result<Filterset, FilterError> {
        if matches!(self.state, State::Word { .. }) {
            self.push(length, b' ')?;
        }
        if !matches!(self.state, State::Operator) {
            return error(FilterErrorKind::UnexpectedEnd, length);
        }
        while let Some(top) = self.top_operator() {
            if top.kind == OperatorKind::Group {
                return error(FilterErrorKind::UnclosedGroup, top.offset);
            }
            self.reduce(top)?;
        }
        Ok(Filterset {
            nodes: self.nodes,
            root: self.operands[0],
            pool: self.pool,
        })
    }
}

impl Filterset {
    /// Parses one expression such as `package(proxima-*) and not test(/^bind::real_/)`.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] with the byte offset of the first malformed token.
    pub fn parse(text: &[u8]) -> Result<Self, FilterError> {
        let mut parser = Parser::new();
        for (offset, byte) in text.iter().enumerate() {
            parser.push(offset, *byte)?;
        }
        parser.finish(text.len())
    }

    /// Builds one substring [`Finder`] per contains matcher; do this once per run, not per name.
    #[must_use]
    pub fn prepare(&self) -> Prepared<'_> {
        let mut finders: [Option<Finder<'_>>; MAX_NODES] = core::array::from_fn(|_| None);
        for (index, node) in self.nodes.iter().enumerate() {
            if let Node::Predicate(_, matcher) = node
                && matcher.strategy == Strategy::Contains
            {
                finders[index] = Some(Finder::new(self.needle(*matcher)));
            }
        }
        Prepared {
            filterset: self,
            finders,
        }
    }

    /// Whether `candidate` is in the set the expression selects; prefer [`Self::prepare`] for many names.
    #[must_use]
    pub fn matches(&self, candidate: &Candidate<'_>) -> bool {
        self.prepare().matches(candidate)
    }

    fn needle(&self, matcher: Matcher) -> &[u8] {
        let start = usize::from(matcher.start);
        &self.pool[start..start + usize::from(matcher.length)]
    }
}

/// A [`Filterset`] with its contains finders built; matching a name does not allocate.
pub struct Prepared<'filterset> {
    filterset: &'filterset Filterset,
    finders: [Option<Finder<'filterset>>; MAX_NODES],
}

impl core::fmt::Debug for Prepared<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("Prepared").finish_non_exhaustive()
    }
}

impl Prepared<'_> {
    /// Whether `candidate` is in the set the expression selects.
    #[must_use]
    pub fn matches(&self, candidate: &Candidate<'_>) -> bool {
        self.evaluate(self.filterset.root, candidate)
    }

    fn evaluate(&self, index: u8, candidate: &Candidate<'_>) -> bool {
        match self.filterset.nodes[usize::from(index)] {
            Node::All => true,
            Node::Nothing => false,
            Node::Predicate(field, matcher) => self.hit(index, field, matcher, candidate),
            Node::Not(inner) => !self.evaluate(inner, candidate),
            Node::And(left, right) => {
                self.evaluate(left, candidate) && self.evaluate(right, candidate)
            }
            Node::Or(left, right) => {
                self.evaluate(left, candidate) || self.evaluate(right, candidate)
            }
            Node::Diff(left, right) => {
                self.evaluate(left, candidate) && !self.evaluate(right, candidate)
            }
        }
    }

    fn hit(&self, index: u8, field: Field, matcher: Matcher, candidate: &Candidate<'_>) -> bool {
        let text = match field {
            Field::Test => candidate.test,
            Field::Binary => candidate.binary,
            Field::Package => candidate.package,
        }
        .as_bytes();
        let needle = self.filterset.needle(matcher);
        match matcher.strategy {
            Strategy::Equals => text == needle,
            Strategy::Contains => self.finders[usize::from(index)]
                .as_ref()
                .is_some_and(|finder| finder.find(text).is_some()),
            Strategy::Glob => glob_matches(needle, text),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    use super::{Candidate, FilterErrorKind, Filterset};

    const CORPUS: &str = include_str!("../tests/fixtures/filterset/interop-corpus.tsv");

    struct Row {
        package: String,
        binary: String,
        test: String,
    }

    fn corpus() -> Vec<Row> {
        CORPUS
            .lines()
            .map(|line| {
                let (identifier, test) = line.split_once('\t').expect("corpus row has a tab");
                let (package, binary) = match identifier.split_once("::") {
                    Some((package, binary)) => (package, String::from(binary)),
                    None => (identifier, identifier.replace('-', "_")),
                };
                Row {
                    package: String::from(package),
                    binary,
                    test: String::from(test),
                }
            })
            .collect()
    }

    fn selected(expression: &str, rows: &[Row]) -> Vec<usize> {
        let filterset = Filterset::parse(expression.as_bytes()).expect(expression);
        let prepared = filterset.prepare();
        rows.iter()
            .enumerate()
            .filter(|(_, row)| {
                prepared.matches(&Candidate {
                    package: &row.package,
                    binary: &row.binary,
                    test: &row.test,
                })
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn reference(rows: &[Row], keep: impl Fn(&Row) -> bool) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, row)| keep(row))
            .map(|(index, _)| index)
            .collect()
    }

    #[test]
    fn corpus_is_the_real_interop_selection() {
        assert_eq!(corpus().len(), 737);
    }

    #[test]
    fn each_matcher_selects_what_a_direct_scan_of_the_names_selects() {
        let rows = corpus();
        let cases: [(&str, Vec<usize>); 12] = [
            ("all()", reference(&rows, |_| true)),
            ("none()", Vec::new()),
            ("test(draft_acceptance)", reference(&rows, |row| row.test.contains("draft_acceptance"))),
            ("test(~draft_acceptance)", reference(&rows, |row| row.test.contains("draft_acceptance"))),
            (
                "test(=bind::tests::bf8_refuses_ggml_layout_and_cpu_packed_decode)",
                reference(&rows, |row| row.test == "bind::tests::bf8_refuses_ggml_layout_and_cpu_packed_decode"),
            ),
            ("test(#bind::moe_*)", reference(&rows, |row| row.test.starts_with("bind::moe_"))),
            ("test(#bind::moe_*::*)", reference(&rows, |row| row.test.starts_with("bind::moe_") && row.test["bind::moe_".len()..].contains("::"))),
            ("binary(capability_matrix)", reference(&rows, |row| row.binary == "capability_matrix")),
            ("binary(cap*)", reference(&rows, |row| row.binary.starts_with("cap"))),
            ("binary(=capability_matrix)", reference(&rows, |row| row.binary == "capability_matrix")),
            ("package(proxima-model-interop)", reference(&rows, |_| true)),
            ("package(proxima)", Vec::new()),
        ];
        let mut checked = 0;
        for (expression, expected) in cases {
            assert_eq!(selected(expression, &rows), expected, "{expression}");
            checked += 1;
        }
        assert_eq!(checked, 12);
        for expression in ["binary(capability_matrix)", "test(#bind::moe_*)", "test(#bind::moe_*::*)"] {
            assert_ne!(selected(expression, &rows), Vec::<usize>::new(), "{expression}");
        }
    }

    #[test]
    fn operators_and_symbols_agree_and_precedence_follows_nextest() {
        let rows = corpus();
        let draft = "test(draft_acceptance)";
        let matrix = "binary(capability_matrix)";
        let capability = "test(capability)";
        let pairs: [(String, String); 8] = [
            (format!("{draft} and {matrix}"), format!("{draft} & {matrix}")),
            (format!("{draft} or {matrix}"), format!("{draft} | {matrix}")),
            (format!("{draft} or {matrix}"), format!("{draft} + {matrix}")),
            (format!("not {draft}"), format!("!{draft}")),
            (format!("{capability} - {draft}"), format!("{capability} and not {draft}")),
            (format!("{draft} | {matrix} & {capability}"), format!("{draft} | ({matrix} & {capability})")),
            (format!("not {draft} and {capability}"), format!("(not {draft}) and {capability}")),
            (format!("!!{draft}"), String::from(draft)),
        ];
        for (left, right) in &pairs {
            assert_eq!(selected(left, &rows), selected(right, &rows), "{left} vs {right}");
        }
        assert_ne!(
            selected(&format!("({draft} | {matrix}) & {capability}"), &rows),
            selected(&format!("{draft} | {matrix} & {capability}"), &rows)
        );
    }

    #[test]
    fn escapes_decode_before_matching() {
        let filterset = Filterset::parse(br"test(=a\)b\,c\\d\u{3a}\u{3a}e)").expect("escapes parse");
        let candidate = |test| Candidate {
            package: "p",
            binary: "b",
            test,
        };
        assert!(filterset.matches(&candidate("a)b,c\\d::e")));
        assert!(!filterset.matches(&candidate("a)b,c\\d:e")));
    }

    fn expression_corpus() -> Vec<String> {
        let atoms = [
            "test(draft_acceptance)",
            "test(#bind::*)",
            "test(#*::tests::*)",
            "binary(capability_matrix)",
            "binary(proxima_*)",
            "package(proxima-*)",
            "all()",
            "none()",
        ];
        let mut expressions = Vec::new();
        for left in atoms {
            expressions.push(String::from(left));
            for right in atoms {
                expressions.push(format!("{left} | {right}"));
                expressions.push(format!("{left} & !{right}"));
                expressions.push(format!("({left} - {right}) + {right}"));
            }
        }
        expressions
    }

    fn match_set(expression: &str, rows: &[Row]) -> Vec<bool> {
        let filterset = Filterset::parse(expression.as_bytes()).expect(expression);
        let prepared = filterset.prepare();
        rows.iter()
            .map(|row| {
                prepared.matches(&Candidate {
                    package: &row.package,
                    binary: &row.binary,
                    test: &row.test,
                })
            })
            .collect()
    }

    #[test]
    fn not_partitions_the_set_with_the_expression() {
        let rows = corpus();
        let mut checked = 0;
        for expression in expression_corpus() {
            let inside = match_set(&expression, &rows);
            let outside = match_set(&format!("not ({expression})"), &rows);
            let disjoint_and_covering = inside.iter().zip(&outside).all(|(is_inside, is_outside)| is_inside != is_outside);
            assert!(disjoint_and_covering, "{expression}");
            checked += 1;
        }
        assert_eq!(checked, 8 + 8 * 8 * 3);
    }

    #[test]
    fn a_and_b_is_a_subset_of_a_and_of_b() {
        let rows = corpus();
        let atoms = expression_corpus();
        let lefts: Vec<(&String, Vec<bool>)> = atoms.iter().step_by(7).map(|left| (left, match_set(left, &rows))).collect();
        let rights: Vec<(&String, Vec<bool>)> = atoms.iter().step_by(11).map(|right| (right, match_set(right, &rows))).collect();
        let mut checked = 0;
        for (left, only_left) in &lefts {
            for (right, only_right) in &rights {
                let both = match_set(&format!("({left}) and ({right})"), &rows);
                let intersection: Vec<bool> = only_left.iter().zip(only_right).map(|(in_left, in_right)| *in_left && *in_right).collect();
                assert_eq!(both, intersection, "{left} and {right}");
                checked += 1;
            }
        }
        assert!(checked > 100, "{checked}");
    }

    #[test]
    fn malformed_expressions_fail_with_the_kind_and_offset_of_the_first_bad_token() {
        let cases: [(&str, FilterErrorKind, usize); 17] = [
            ("", FilterErrorKind::UnexpectedEnd, 0),
            ("test(a", FilterErrorKind::UnexpectedEnd, 6),
            ("test(a) and", FilterErrorKind::UnexpectedEnd, 11),
            ("test(a) b", FilterErrorKind::ExpectedOperator, 8),
            ("test a", FilterErrorKind::ExpectedOpenParen, 4),
            ("tests(a)", FilterErrorKind::UnknownPredicate, 0),
            ("test()", FilterErrorKind::EmptyMatcher, 5),
            ("(test(a)", FilterErrorKind::UnclosedGroup, 0),
            ("test(a))", FilterErrorKind::UnmatchedClose, 7),
            ("test(a) & & test(b)", FilterErrorKind::ExpectedOperand, 10),
            ("all(x)", FilterErrorKind::ExpectedCloseParen, 4),
            (r"test(a\q)", FilterErrorKind::BadEscape, 7),
            ("test(/ab/)", FilterErrorKind::RegexUnsupported, 5),
            ("binary(/a/)", FilterErrorKind::RegexUnsupported, 7),
            ("binary(a[b)", FilterErrorKind::Glob(crate::filter_glob::GlobFault::UnclosedClass { offset: 1 }), 8),
            ("binary(#a{b,c})", FilterErrorKind::Glob(crate::filter_glob::GlobFault::Alternation { offset: 1 }), 9),
            ("!", FilterErrorKind::UnexpectedEnd, 1),
        ];
        let mut checked = 0;
        for (text, kind, offset) in cases {
            let failure = Filterset::parse(text.as_bytes()).expect_err(text);
            assert_eq!((failure.kind, failure.offset), (kind, offset), "{text}");
            checked += 1;
        }
        assert_eq!(checked, cases.len());
    }
}
