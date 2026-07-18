//! The branch-condition expression language: a small, TOTAL, non-Turing-complete
//! language that gives meaning to the opaque string inside a
//! [`crate::document::BranchCondition::Expression`].
//!
//! # Why deliberately weak
//!
//! A branch condition is evaluated inside a durable, replayed run: the same
//! expression is re-evaluated against the same routed value during replay and
//! again during a fork, so its result must be identical every time and forever.
//! Two properties follow, and they shape every decision here:
//!
//! - **Total.** Every syntactically valid expression produces a `bool` for
//!   *every* possible JSON value, with no panic, no error, and no undefined
//!   case. There is no runtime failure mode, so a valid condition can never
//!   turn a replay into an error.
//! - **Non-Turing-complete.** No loops, no function calls, no arithmetic, no
//!   regex. Evaluation is a single linear walk of an abstract syntax tree whose
//!   size is bounded by the [`MAX_EXPRESSION_LEN`]-character source cap, so eval
//!   time is bounded. A Turing-complete language in a durable log would make
//!   replay time unbounded.
//!
//! # The grammar
//!
//! ```text
//! or         := and ( "||" and )*
//! and        := unary ( "&&" unary )*
//! unary      := "!" unary | atom
//! atom       := "(" or ")" | comparison
//! comparison := operand ( cmp_op operand )?
//! cmp_op     := "==" | "!=" | "<" | "<=" | ">" | ">="
//! operand    := literal | path
//! literal    := string | number | "true" | "false" | "null"
//! path       := ident ( "." segment )*
//! segment    := ident | integer
//! ```
//!
//! Precedence, loosest to tightest: `||`, then `&&`, then `!`, then a
//! comparison. A comparison therefore binds tighter than any boolean operator
//! (`!score > 0.8` parses as `!(score > 0.8)`), and comparisons do not chain
//! (`a < b < c` is a parse error). Parentheses group a whole boolean
//! sub-expression; they are not part of a comparison operand, so `(a > b) > c`
//! is a parse error rather than a comparison of a boolean.
//!
//! A bare operand used where a boolean is expected (`ready`, or `ready && ok`)
//! is true only when it resolves to the JSON boolean `true`; every other value,
//! and a missing path, is false.
//!
//! # Paths and array indexing
//!
//! A path is dot-separated segments walked from the root of the routed value:
//! `score`, `output.score`, `items.0.score`. Array indexing IS supported
//! because routed values routinely carry lists (a tool that returns an array, a
//! map join), and without it a branch could not reach into one at all. The
//! container type decides how a segment is read: on a JSON object a segment is a
//! key, and on a JSON array a purely numeric segment is a zero-based index. A
//! numeric segment therefore never indexes an object, so an object key spelled
//! with digits (`{"0": ...}`) is not reachable; that ambiguity is traded away
//! deliberately for a single, deterministic rule.
//!
//! # Total semantics (these are forever: replay and fork re-run them verbatim)
//!
//! - **Missing path.** A path that does not resolve (an absent key, an index
//!   past the end, or a descent into a non-container) is MISSING, which is
//!   distinct from JSON `null`. Any comparison with a missing operand is
//!   `false`, and a missing operand in boolean position is `false`. Rationale:
//!   JSON `null` is a value an author chose, so conflating it with "absent"
//!   would let `x == null` fire on a field that simply is not there; keeping
//!   them distinct means a branch never silently fires on absent data. `!` still
//!   lets an author test for absence deliberately (`!(score > 0.8)` is true when
//!   `score` is missing).
//! - **Type mismatch.** Equality (`==`, `!=`) is defined across all types:
//!   values of different types are never equal, so `"5" == 5` is false. Ordering
//!   (`<`, `<=`, `>`, `>=`) is defined only for two numbers or two strings; any
//!   other ordered comparison (a number against a string, anything against a
//!   bool/null/object/array) is `false`. Rationale: ordering has an obvious
//!   total meaning only for numbers and strings, and yielding `false` everywhere
//!   else means a mismatched type can never satisfy an ordering branch.
//! - **Number comparison.** Two numbers compare by mathematical value. When both
//!   are integers they compare exactly through `i128`, so two distinct large
//!   integers never collide; when either is a floating-point number both are
//!   compared as `f64` (so `1 == 1.0` is true). Rationale: exact integer
//!   comparison is the safe default, and dropping to `f64` only when a fractional
//!   value is actually involved confines `f64`'s precision limit to the case
//!   that inherently needs it. JSON numbers are always finite (JSON cannot encode
//!   `NaN` or infinity), so an unordered `f64` result cannot arise; the code
//!   treats it as `false` regardless, keeping eval total for any constructed
//!   value.

use std::cmp::Ordering;

use serde_json::{Number, Value};

/// The maximum length, in Unicode scalar values, of a condition expression.
///
/// Enforced at [`parse`] before any work, so an over-long string is rejected
/// with a precise error and never lexed. The cap is what bounds the AST size,
/// which in turn bounds eval time.
pub const MAX_EXPRESSION_LEN: usize = 512;

/// A failure to parse an expression. Its [`Display`](std::fmt::Display) message
/// is the human-readable diagnostic, and names the offending character position
/// where one is known.
///
/// `Clone`, `PartialEq`, and `Eq` are derived so a caller (the graph validator)
/// can carry it inside its own comparable error type.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ExprError {
    message: String,
}

impl ExprError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The human-readable diagnostic message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// A parsed, ready-to-evaluate condition expression.
///
/// Produced by [`parse`] and evaluated by [`Expr::eval`]. It owns its whole AST,
/// borrows nothing, and holds no IO, clock, or randomness, so it is safe to keep
/// and re-evaluate for the life of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    root: Bool,
}

impl Expr {
    /// Evaluates the expression against a routed value, always returning a
    /// `bool`.
    ///
    /// This is TOTAL: it never panics and never errors for any `value`. The
    /// missing-path, type-mismatch, and number-comparison semantics are those
    /// documented on the [module](self). Eval is a single linear walk of the
    /// AST, whose size is bounded by [`MAX_EXPRESSION_LEN`].
    #[must_use]
    pub fn eval(&self, value: &Value) -> bool {
        eval_bool(&self.root, value)
    }
}

/// A parsed reference into a routed value: a dot-separated path with array
/// indexing, the same `path` grammar the expression language uses for an
/// operand, standing alone as a value reference rather than inside a boolean.
///
/// This is how a `map` node's `over` field names the list it fans out over: the
/// reference is resolved against the node's routed value at fan-out time, and the
/// engine reuses the identical missing-path semantics the branch expressions use,
/// so references resolve one consistent way across the whole document. It owns
/// its whole path, borrows nothing, and holds no IO, clock, or randomness, so it
/// is safe to keep and re-resolve for the life of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Reference {
    segments: Vec<Segment>,
}

impl Reference {
    /// Resolves the reference against a routed value, returning the value it
    /// names or `None` when the path is missing (an absent key, an index past the
    /// end, or a descent into a non-container).
    ///
    /// This is TOTAL: it never panics for any `value`, and its missing-path
    /// semantics are exactly those [`Expr::eval`] uses for a path operand.
    #[must_use]
    pub fn resolve<'a>(&self, value: &'a Value) -> Option<&'a Value> {
        resolve_path(&self.segments, value)
    }
}

/// Parses a bare reference path (`items`, `output.items`, `results.0.items`), the
/// standalone counterpart of a `path` operand in the expression grammar.
///
/// The [`MAX_EXPRESSION_LEN`]-character cap is enforced first, exactly as [`parse`]
/// does. Only a path is accepted: a literal (`5`, `"x"`, `true`) is rejected, so a
/// reference always names a location in the routed value rather than a constant.
///
/// # Errors
///
/// Returns an [`ExprError`] when the input exceeds the length cap, is not a
/// well-formed path, or is a bare literal rather than a path.
pub fn parse_reference(input: &str) -> Result<Reference, ExprError> {
    let chars: Vec<char> = input.chars().collect();
    if chars.len() > MAX_EXPRESSION_LEN {
        return Err(ExprError::new(format!(
            "reference is {} characters long, which exceeds the {MAX_EXPRESSION_LEN}-character limit",
            chars.len()
        )));
    }

    let tokens = lex(&chars)?;
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        end: chars.len(),
    };
    let operand = parser.parse_operand()?;
    parser.expect_end()?;
    match operand {
        Operand::Path(segments) => Ok(Reference { segments }),
        Operand::Literal(_) => Err(ExprError::new(
            "a reference must be a path into the routed value, not a literal",
        )),
    }
}

/// A boolean-valued node of the AST.
#[derive(Clone, Debug, PartialEq)]
enum Bool {
    Or(Box<Bool>, Box<Bool>),
    And(Box<Bool>, Box<Bool>),
    Not(Box<Bool>),
    Compare {
        left: Operand,
        op: CmpOp,
        right: Operand,
    },
    /// A bare operand used in boolean position: true only if it resolves to the
    /// JSON boolean `true`.
    Truthy(Operand),
}

/// One side of a comparison, or a bare boolean operand: a literal value or a
/// path into the routed value.
#[derive(Clone, Debug, PartialEq)]
enum Operand {
    Literal(Value),
    Path(Vec<Segment>),
}

/// One step of a path: an object key or an array index.
#[derive(Clone, Debug, PartialEq)]
enum Segment {
    Key(String),
    Index(usize),
}

/// A comparison operator.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parses a condition expression, or reports why it is not well-formed.
///
/// The [`MAX_EXPRESSION_LEN`]-character cap is enforced first, so an over-long
/// input is rejected before any lexing. On success the returned [`Expr`] is
/// guaranteed to evaluate totally against any JSON value.
///
/// # Errors
///
/// Returns an [`ExprError`] whose message names the problem (and, where known,
/// the character position) when the input exceeds the length cap or does not
/// match the grammar.
pub fn parse(input: &str) -> Result<Expr, ExprError> {
    let chars: Vec<char> = input.chars().collect();
    if chars.len() > MAX_EXPRESSION_LEN {
        return Err(ExprError::new(format!(
            "expression is {} characters long, which exceeds the {MAX_EXPRESSION_LEN}-character limit",
            chars.len()
        )));
    }

    let tokens = lex(&chars)?;
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        end: chars.len(),
    };
    let root = parser.parse_or()?;
    parser.expect_end()?;
    Ok(Expr { root })
}

/// A lexed token together with the character position it started at.
#[derive(Clone, Debug, PartialEq)]
struct Spanned {
    token: Token,
    at: usize,
}

/// A lexical token.
#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Number(Number),
    Str(String),
    True,
    False,
    Null,
    Dot,
    LParen,
    RParen,
    Bang,
    AndAnd,
    OrOr,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Turns the character slice into a token stream, or reports the first bad
/// character.
fn lex(chars: &[char]) -> Result<Vec<Spanned>, ExprError> {
    let mut tokens = Vec::new();
    let mut i = 0;
    let len = chars.len();

    while i < len {
        let c = chars[i];
        let at = i;

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        match c {
            '(' => {
                tokens.push(Spanned {
                    token: Token::LParen,
                    at,
                });
                i += 1;
            }
            ')' => {
                tokens.push(Spanned {
                    token: Token::RParen,
                    at,
                });
                i += 1;
            }
            '.' => {
                tokens.push(Spanned {
                    token: Token::Dot,
                    at,
                });
                i += 1;
            }
            '!' => {
                if i + 1 < len && chars[i + 1] == '=' {
                    tokens.push(Spanned {
                        token: Token::NotEq,
                        at,
                    });
                    i += 2;
                } else {
                    tokens.push(Spanned {
                        token: Token::Bang,
                        at,
                    });
                    i += 1;
                }
            }
            '=' => {
                if i + 1 < len && chars[i + 1] == '=' {
                    tokens.push(Spanned {
                        token: Token::EqEq,
                        at,
                    });
                    i += 2;
                } else {
                    return Err(at_char(at, "a lone `=`; did you mean `==`?"));
                }
            }
            '<' => {
                if i + 1 < len && chars[i + 1] == '=' {
                    tokens.push(Spanned {
                        token: Token::Le,
                        at,
                    });
                    i += 2;
                } else {
                    tokens.push(Spanned {
                        token: Token::Lt,
                        at,
                    });
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < len && chars[i + 1] == '=' {
                    tokens.push(Spanned {
                        token: Token::Ge,
                        at,
                    });
                    i += 2;
                } else {
                    tokens.push(Spanned {
                        token: Token::Gt,
                        at,
                    });
                    i += 1;
                }
            }
            '&' => {
                if i + 1 < len && chars[i + 1] == '&' {
                    tokens.push(Spanned {
                        token: Token::AndAnd,
                        at,
                    });
                    i += 2;
                } else {
                    return Err(at_char(at, "a lone `&`; did you mean `&&`?"));
                }
            }
            '|' => {
                if i + 1 < len && chars[i + 1] == '|' {
                    tokens.push(Spanned {
                        token: Token::OrOr,
                        at,
                    });
                    i += 2;
                } else {
                    return Err(at_char(at, "a lone `|`; did you mean `||`?"));
                }
            }
            '"' => {
                let (token, next) = lex_string(chars, i)?;
                tokens.push(Spanned { token, at });
                i = next;
            }
            _ if c.is_ascii_digit()
                || (c == '-' && i + 1 < len && chars[i + 1].is_ascii_digit()) =>
            {
                let (token, next) = lex_number(chars, i)?;
                tokens.push(Spanned { token, at });
                i = next;
            }
            _ if is_ident_start(c) => {
                let (token, next) = lex_ident(chars, i);
                tokens.push(Spanned { token, at });
                i = next;
            }
            _ => {
                return Err(at_char(at, format!("an unexpected character `{c}`")));
            }
        }
    }

    Ok(tokens)
}

/// Lexes a double-quoted string starting at the opening quote. Supports the
/// escapes `\"`, `\\`, `\/`, `\n`, `\t`, and `\r`; any other escape, or an
/// unterminated string, is an error.
fn lex_string(chars: &[char], start: usize) -> Result<(Token, usize), ExprError> {
    let len = chars.len();
    let mut i = start + 1;
    let mut value = String::new();

    while i < len {
        let c = chars[i];
        if c == '"' {
            return Ok((Token::Str(value), i + 1));
        }
        if c == '\\' {
            i += 1;
            if i >= len {
                break;
            }
            match chars[i] {
                '"' => value.push('"'),
                '\\' => value.push('\\'),
                '/' => value.push('/'),
                'n' => value.push('\n'),
                't' => value.push('\t'),
                'r' => value.push('\r'),
                other => {
                    return Err(at_char(
                        i,
                        format!("an unsupported string escape `\\{other}`"),
                    ));
                }
            }
            i += 1;
        } else {
            value.push(c);
            i += 1;
        }
    }

    Err(at_char(start, "an unterminated string literal"))
}

/// Lexes an integer or decimal number (optionally negative). Exponents are not
/// supported.
fn lex_number(chars: &[char], start: usize) -> Result<(Token, usize), ExprError> {
    let len = chars.len();
    let mut i = start;
    if chars[i] == '-' {
        i += 1;
    }
    while i < len && chars[i].is_ascii_digit() {
        i += 1;
    }
    // A fractional part is consumed only when a digit actually follows the dot,
    // so `items.0` lexes as the number `0` then a `.`, not as `0.` waiting for a
    // fraction.
    if i + 1 < len && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
        i += 1;
        while i < len && chars[i].is_ascii_digit() {
            i += 1;
        }
    }

    let text: String = chars[start..i].iter().collect();
    let number: Number = serde_json::from_str(&text)
        .map_err(|_| at_char(start, format!("a malformed number `{text}`")))?;
    Ok((Token::Number(number), i))
}

/// Lexes an identifier, promoting the three reserved words to their keyword
/// tokens.
fn lex_ident(chars: &[char], start: usize) -> (Token, usize) {
    let len = chars.len();
    let mut i = start;
    while i < len && is_ident_continue(chars[i]) {
        i += 1;
    }
    let text: String = chars[start..i].iter().collect();
    let token = match text.as_str() {
        "true" => Token::True,
        "false" => Token::False,
        "null" => Token::Null,
        _ => Token::Ident(text),
    };
    (token, i)
}

fn at_char(position: usize, what: impl std::fmt::Display) -> ExprError {
    ExprError::new(format!("found {what} at character {position}"))
}

/// A recursive-descent parser over the token stream. Each grammar rule is one
/// method, and they call one another top-down, so the source reads in the same
/// order as the grammar in the module doc.
struct Parser<'a> {
    tokens: &'a [Spanned],
    pos: usize,
    /// The character length of the source, used to position an
    /// unexpected-end-of-input error.
    end: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|s| &s.token)
    }

    fn position(&self) -> usize {
        self.tokens.get(self.pos).map_or(self.end, |s| s.at)
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn expect_end(&self) -> Result<(), ExprError> {
        match self.peek() {
            None => Ok(()),
            Some(_) => Err(at_char(
                self.position(),
                "an unexpected trailing token; the expression already ended",
            )),
        }
    }

    fn parse_or(&mut self) -> Result<Bool, ExprError> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Token::OrOr)) {
            self.advance();
            let right = self.parse_and()?;
            left = Bool::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Bool, ExprError> {
        let mut left = self.parse_unary()?;
        while matches!(self.peek(), Some(Token::AndAnd)) {
            self.advance();
            let right = self.parse_unary()?;
            left = Bool::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Bool, ExprError> {
        if matches!(self.peek(), Some(Token::Bang)) {
            self.advance();
            let inner = self.parse_unary()?;
            Ok(Bool::Not(Box::new(inner)))
        } else {
            self.parse_atom()
        }
    }

    fn parse_atom(&mut self) -> Result<Bool, ExprError> {
        if matches!(self.peek(), Some(Token::LParen)) {
            self.advance();
            let inner = self.parse_or()?;
            match self.peek() {
                Some(Token::RParen) => {
                    self.advance();
                    Ok(inner)
                }
                _ => Err(at_char(self.position(), "a missing closing `)`")),
            }
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Bool, ExprError> {
        let left = self.parse_operand()?;
        let op = match self.peek() {
            Some(Token::EqEq) => CmpOp::Eq,
            Some(Token::NotEq) => CmpOp::Ne,
            Some(Token::Lt) => CmpOp::Lt,
            Some(Token::Le) => CmpOp::Le,
            Some(Token::Gt) => CmpOp::Gt,
            Some(Token::Ge) => CmpOp::Ge,
            _ => return Ok(Bool::Truthy(left)),
        };
        self.advance();
        let right = self.parse_operand()?;
        Ok(Bool::Compare { left, op, right })
    }

    fn parse_operand(&mut self) -> Result<Operand, ExprError> {
        match self.peek() {
            Some(Token::Number(n)) => {
                let value = Operand::Literal(Value::Number(n.clone()));
                self.advance();
                Ok(value)
            }
            Some(Token::Str(s)) => {
                let value = Operand::Literal(Value::String(s.clone()));
                self.advance();
                Ok(value)
            }
            Some(Token::True) => {
                self.advance();
                Ok(Operand::Literal(Value::Bool(true)))
            }
            Some(Token::False) => {
                self.advance();
                Ok(Operand::Literal(Value::Bool(false)))
            }
            Some(Token::Null) => {
                self.advance();
                Ok(Operand::Literal(Value::Null))
            }
            Some(Token::Ident(name)) => {
                let name = name.clone();
                self.advance();
                self.parse_path(name)
            }
            _ => Err(at_char(
                self.position(),
                "a value or path where one was required",
            )),
        }
    }

    fn parse_path(&mut self, first: String) -> Result<Operand, ExprError> {
        let mut segments = vec![Segment::Key(first)];
        while matches!(self.peek(), Some(Token::Dot)) {
            self.advance();
            match self.peek() {
                Some(Token::Ident(name)) => {
                    segments.push(Segment::Key(name.clone()));
                    self.advance();
                }
                Some(Token::Number(n)) => {
                    let index = n
                        .as_u64()
                        .and_then(|v| usize::try_from(v).ok())
                        .ok_or_else(|| {
                            at_char(
                                self.position(),
                                "a path segment that is not a non-negative integer index",
                            )
                        })?;
                    segments.push(Segment::Index(index));
                    self.advance();
                }
                _ => {
                    return Err(at_char(self.position(), "a missing path segment after `.`"));
                }
            }
        }
        Ok(Operand::Path(segments))
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

fn eval_bool(node: &Bool, root: &Value) -> bool {
    match node {
        Bool::Or(a, b) => eval_bool(a, root) || eval_bool(b, root),
        Bool::And(a, b) => eval_bool(a, root) && eval_bool(b, root),
        Bool::Not(a) => !eval_bool(a, root),
        Bool::Compare { left, op, right } => eval_compare(left, *op, right, root),
        Bool::Truthy(operand) => matches!(resolve(operand, root), Some(Value::Bool(true))),
    }
}

/// Resolves an operand to the value it names, or `None` when a path is missing.
/// A literal always resolves (a literal `null` resolves to `Some(Null)`, which
/// is what keeps `null` distinct from a missing path).
fn resolve<'a>(operand: &'a Operand, root: &'a Value) -> Option<&'a Value> {
    match operand {
        Operand::Literal(value) => Some(value),
        Operand::Path(segments) => resolve_path(segments, root),
    }
}

/// Walks a path of segments from the root of a value, returning the value it
/// names or `None` when the path is missing (an absent key, an index past the
/// end, or a descent into a non-container). Shared by the expression evaluator
/// and by [`Reference::resolve`], so a `map` node's `over` reference and a branch
/// path resolve a routed value the identical way.
fn resolve_path<'a>(segments: &[Segment], root: &'a Value) -> Option<&'a Value> {
    let mut current = root;
    for segment in segments {
        current = match (current, segment) {
            (Value::Object(map), Segment::Key(key)) => map.get(key)?,
            (Value::Array(items), Segment::Index(index)) => items.get(*index)?,
            _ => return None,
        };
    }
    Some(current)
}

fn eval_compare(left: &Operand, op: CmpOp, right: &Operand, root: &Value) -> bool {
    // A missing operand makes every comparison false, so a branch never fires on
    // absent data.
    let (Some(l), Some(r)) = (resolve(left, root), resolve(right, root)) else {
        return false;
    };

    match op {
        CmpOp::Eq => values_equal(l, r),
        CmpOp::Ne => !values_equal(l, r),
        CmpOp::Lt => matches!(order(l, r), Some(Ordering::Less)),
        CmpOp::Le => matches!(order(l, r), Some(Ordering::Less | Ordering::Equal)),
        CmpOp::Gt => matches!(order(l, r), Some(Ordering::Greater)),
        CmpOp::Ge => matches!(order(l, r), Some(Ordering::Greater | Ordering::Equal)),
    }
}

/// Equality across all types: numbers compare by mathematical value, and any two
/// values of different types are unequal.
fn values_equal(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Number(a), Value::Number(b)) => number_cmp(a, b) == Some(Ordering::Equal),
        _ => l == r,
    }
}

/// Ordering, defined only for two numbers or two strings; every other pairing is
/// unordered.
fn order(l: &Value, r: &Value) -> Option<Ordering> {
    match (l, r) {
        (Value::Number(a), Value::Number(b)) => number_cmp(a, b),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Compares two JSON numbers by mathematical value. Two integers compare exactly
/// through `i128`; when either is floating-point both are compared as `f64`.
fn number_cmp(a: &Number, b: &Number) -> Option<Ordering> {
    if let (Some(x), Some(y)) = (as_i128(a), as_i128(b)) {
        return Some(x.cmp(&y));
    }
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => None,
    }
}

/// The exact integer value of a number, or `None` when it is floating-point.
fn as_i128(n: &Number) -> Option<i128> {
    n.as_u64()
        .map(i128::from)
        .or_else(|| n.as_i64().map(i128::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn eval(expr: &str, value: &Value) -> bool {
        parse(expr)
            .unwrap_or_else(|e| panic!("`{expr}` should parse: {e}"))
            .eval(value)
    }

    // --- The sample from the validator's own test must parse and evaluate. ---

    #[test]
    fn sample_score_expression() {
        assert!(eval("score > 0.8", &json!({"score": 0.9})));
        assert!(!eval("score > 0.8", &json!({"score": 0.5})));
    }

    // --- Each operator. ---

    #[test]
    fn every_comparison_operator() {
        let v = json!({"n": 5});
        assert!(eval("n == 5", &v));
        assert!(!eval("n == 4", &v));
        assert!(eval("n != 4", &v));
        assert!(!eval("n != 5", &v));
        assert!(eval("n < 6", &v));
        assert!(!eval("n < 5", &v));
        assert!(eval("n <= 5", &v));
        assert!(eval("n > 4", &v));
        assert!(!eval("n > 5", &v));
        assert!(eval("n >= 5", &v));
    }

    #[test]
    fn boolean_operators() {
        let v = json!({"a": true, "b": false});
        assert!(eval("a && !b", &v));
        assert!(eval("a || b", &v));
        assert!(!eval("!a", &v));
        assert!(eval("!b", &v));
        assert!(!eval("a && b", &v));
    }

    // --- Precedence and parenthesization. ---

    #[test]
    fn and_binds_tighter_than_or() {
        // false || (true && true) == true; if || bound tighter it would be
        // (false || true) && true == true too, so use a distinguishing case:
        // true || (false && false) == true, vs (true || false) && false == false.
        let v = json!({});
        assert!(eval("true || false && false", &v));
        assert!(!eval("(true || false) && false", &v));
    }

    #[test]
    fn not_binds_tighter_than_and() {
        let v = json!({"a": false, "b": true});
        // !a && b parses as (!a) && b == true && true == true.
        assert!(eval("!a && b", &v));
    }

    #[test]
    fn comparison_binds_tighter_than_not() {
        // !score > 0.8 parses as !(score > 0.8).
        assert!(eval("!score > 0.8", &json!({"score": 0.5})));
        assert!(!eval("!score > 0.8", &json!({"score": 0.9})));
    }

    #[test]
    fn parentheses_group_boolean_expressions() {
        let v = json!({"a": true, "b": false, "c": true});
        assert!(eval("a && (b || c)", &v));
        assert!(!eval("(a && b) || (b && c)", &v));
    }

    #[test]
    fn comparisons_do_not_chain() {
        assert!(parse("1 < 2 < 3").is_err());
    }

    #[test]
    fn a_boolean_group_is_not_a_comparison_operand() {
        assert!(parse("(a > b) > c").is_err());
    }

    // --- Paths, including array indexing. ---

    #[test]
    fn nested_and_indexed_paths() {
        let v = json!({"output": {"score": 0.9}, "items": [{"score": 1}, {"score": 2}]});
        assert!(eval("output.score > 0.8", &v));
        assert!(eval("items.0.score == 1", &v));
        assert!(eval("items.1.score == 2", &v));
    }

    #[test]
    fn bare_path_is_truthy_only_for_boolean_true() {
        assert!(eval("flag", &json!({"flag": true})));
        assert!(!eval("flag", &json!({"flag": false})));
        assert!(!eval("flag", &json!({"flag": 1})));
        assert!(!eval("flag", &json!({"flag": "true"})));
        assert!(!eval("flag", &json!({})));
    }

    // --- Missing-path semantics. ---

    #[test]
    fn missing_path_makes_every_comparison_false() {
        let v = json!({});
        assert!(!eval("missing == 1", &v));
        assert!(!eval("missing != 1", &v));
        assert!(!eval("missing < 1", &v));
        assert!(!eval("missing > 1", &v));
        // But negation gives a deliberate handle on absence.
        assert!(eval("!(missing > 1)", &v));
    }

    #[test]
    fn missing_is_distinct_from_null() {
        // A present null equals a null literal; a missing path does not.
        assert!(eval("x == null", &json!({"x": null})));
        assert!(!eval("missing == null", &json!({})));
    }

    #[test]
    fn descending_into_a_non_container_is_missing() {
        let v = json!({"x": 5});
        assert!(!eval("x.y == 1", &v));
        assert!(!eval("x.0 == 1", &v));
    }

    // --- Type-mismatch semantics. ---

    #[test]
    fn cross_type_equality_is_never_equal() {
        assert!(!eval("x == 5", &json!({"x": "5"})));
        assert!(eval("x != 5", &json!({"x": "5"})));
        assert!(!eval("x == 1", &json!({"x": true})));
    }

    #[test]
    fn cross_type_ordering_is_false() {
        assert!(!eval("x < 5", &json!({"x": "5"})));
        assert!(!eval("x > 5", &json!({"x": "5"})));
        assert!(!eval("x < 5", &json!({"x": true})));
        assert!(!eval("x < 5", &json!({"x": null})));
    }

    #[test]
    fn strings_order_lexicographically() {
        assert!(eval("x < \"b\"", &json!({"x": "a"})));
        assert!(!eval("x < \"a\"", &json!({"x": "b"})));
        assert!(eval("x == \"hi\"", &json!({"x": "hi"})));
    }

    // --- Number edge cases. ---

    #[test]
    fn integer_and_float_equality() {
        assert!(eval("x == 1", &json!({"x": 1.0})));
        assert!(eval("x == 1.0", &json!({"x": 1})));
        assert!(eval("x >= 1", &json!({"x": 1.0})));
    }

    #[test]
    fn large_integers_compare_exactly() {
        // Two distinct integers beyond f64's exact range must not collide.
        let big = json!({"a": 9_007_199_254_740_993_i64});
        assert!(eval("a == 9007199254740993", &big));
        assert!(!eval("a == 9007199254740992", &big));
    }

    #[test]
    fn negative_and_signed_comparison() {
        assert!(eval("x < 0", &json!({"x": -3})));
        assert!(eval("x == -0.5", &json!({"x": -0.5})));
        assert!(eval("x > y", &json!({"x": 1, "y": -1})));
    }

    // --- Cap and parse errors. ---

    #[test]
    fn the_length_cap_rejects_longer_input_and_names_it() {
        let ok = "a".repeat(MAX_EXPRESSION_LEN);
        assert!(parse(&ok).is_ok());
        let too_long = "a".repeat(MAX_EXPRESSION_LEN + 1);
        let err = parse(&too_long).expect_err("over the cap");
        assert!(
            err.message().contains(&MAX_EXPRESSION_LEN.to_string()),
            "names the cap: {err}"
        );
    }

    #[test]
    fn assorted_syntax_errors() {
        for bad in [
            "",
            "&&",
            "a &&",
            "a && (b",
            "== 5",
            "a = 5",
            "a & b",
            "a | b",
            "1.2.3 == 1",
            "\"unterminated",
            "a.",
            "a.-1 == 1",
        ] {
            assert!(parse(bad).is_err(), "`{bad}` should be a parse error");
        }
    }

    // --- References (the `map` `over` resolver). ---

    #[test]
    fn a_reference_resolves_a_top_level_and_nested_array() {
        let top = parse_reference("items").expect("`items` parses");
        assert_eq!(
            top.resolve(&json!({"items": [1, 2, 3]})),
            Some(&json!([1, 2, 3]))
        );
        let nested = parse_reference("output.items").expect("`output.items` parses");
        assert_eq!(
            nested.resolve(&json!({"output": {"items": ["a"]}})),
            Some(&json!(["a"]))
        );
        let indexed = parse_reference("results.0.items").expect("indexed path parses");
        assert_eq!(
            indexed.resolve(&json!({"results": [{"items": [true]}]})),
            Some(&json!([true]))
        );
    }

    #[test]
    fn a_missing_reference_resolves_to_none() {
        let reference = parse_reference("items").expect("parses");
        assert_eq!(reference.resolve(&json!({})), None);
        assert_eq!(reference.resolve(&json!({"other": [1]})), None);
        // Descending into a non-container is missing, exactly as in eval.
        let deep = parse_reference("x.items").expect("parses");
        assert_eq!(deep.resolve(&json!({"x": 5})), None);
    }

    #[test]
    fn a_reference_may_name_any_json_value_not_only_arrays() {
        // The resolver is type-agnostic; the engine decides an array is required.
        let reference = parse_reference("value").expect("parses");
        assert_eq!(reference.resolve(&json!({"value": 5})), Some(&json!(5)));
        assert_eq!(
            reference.resolve(&json!({"value": {"k": 1}})),
            Some(&json!({"k": 1}))
        );
    }

    #[test]
    fn a_literal_or_malformed_reference_is_rejected() {
        assert!(
            parse_reference("5").is_err(),
            "a bare literal is not a path"
        );
        assert!(
            parse_reference("\"x\"").is_err(),
            "a string literal is not a path"
        );
        assert!(parse_reference("true").is_err(), "a keyword is not a path");
        assert!(
            parse_reference("items ==").is_err(),
            "trailing tokens rejected"
        );
        assert!(
            parse_reference("items.").is_err(),
            "a dangling dot is rejected"
        );
        assert!(
            parse_reference("").is_err(),
            "an empty reference is rejected"
        );
    }

    // --- Property tests. ---

    proptest! {
        /// No input string, however arbitrary, panics the parser.
        #[test]
        fn parsing_never_panics(input in ".*") {
            let _ = parse(&input);
        }

        /// A generator biased toward near-miss syntax also never panics.
        #[test]
        fn near_miss_parsing_never_panics(
            input in "[a-z0-9_. ()!&|<>=\"'.-]{0,80}"
        ) {
            let _ = parse(&input);
        }

        /// Any input at or over the cap length is handled without panic, and a
        /// too-long one is always rejected.
        #[test]
        fn cap_always_holds(input in "a{500,700}") {
            let result = parse(&input);
            if input.chars().count() > MAX_EXPRESSION_LEN {
                prop_assert!(result.is_err());
            }
        }

        /// Evaluation never panics for any parsed expression against any JSON
        /// value. The expression is drawn from a grammar-shaped generator so
        /// real ASTs (not just trivial ones) are exercised.
        #[test]
        fn eval_never_panics(expr in expr_strategy(), value in json_strategy()) {
            if let Ok(parsed) = parse(&expr) {
                let _ = parsed.eval(&value);
            }
        }
    }

    /// A strategy that builds plausible expression strings from the real
    /// vocabulary, so parses often succeed and eval is genuinely exercised.
    fn expr_strategy() -> impl Strategy<Value = String> {
        let leaf = prop_oneof![
            Just("score".to_string()),
            Just("output.score".to_string()),
            Just("items.0.score".to_string()),
            Just("flag".to_string()),
            Just("0.8".to_string()),
            Just("5".to_string()),
            Just("-1".to_string()),
            Just("true".to_string()),
            Just("null".to_string()),
            Just("\"hi\"".to_string()),
        ];
        let comparison = (leaf.clone(), "==|!=|<|<=|>|>=", leaf.clone())
            .prop_map(|(l, op, r)| format!("{l} {op} {r}"));
        let atom = prop_oneof![leaf, comparison];
        atom.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                inner.clone().prop_map(|e| format!("!{e}")),
                inner.clone().prop_map(|e| format!("({e})")),
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} && {b}")),
                (inner.clone(), inner).prop_map(|(a, b)| format!("{a} || {b}")),
            ]
        })
    }

    /// A strategy for arbitrary JSON values of bounded depth.
    fn json_strategy() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| json!(n)),
            any::<f64>()
                .prop_filter("finite", |f| f.is_finite())
                .prop_map(|f| json!(f)),
            ".*".prop_map(Value::String),
        ];
        leaf.prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::hash_map("[a-z]{1,5}", inner, 0..4)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }
}
