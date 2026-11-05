//! Safe arithmetic and date math, mirroring `tools/calc.py`.
//!
//! Expressions are parsed by hand into a small AST; only numeric/string
//! constants, the six arithmetic operators, unary signs and six allowlisted
//! calls can be evaluated. Nothing is `eval`'d.

use chrono::{Local, NaiveDate};
use serde_json::{json, Value};

pub const MAX_EXPONENT: i64 = 12;
const MAX_EXPRESSION_CHARS: usize = 500;

const UNSUPPORTED: &str = "unsupported expression";
const DIVISION_BY_ZERO: &str = "division by zero";

#[derive(Clone, Debug, PartialEq)]
enum Eval {
    Int(i128),
    Float(f64),
    Text(String),
}

#[derive(Clone, Debug)]
enum Node {
    Value(Eval),
    Unary(char, Box<Node>),
    Binary(BinOp, Box<Node>, Box<Node>),
    Call(String, Vec<Node>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(Eval),
    Text(String),
    Name(String),
    Punct(&'static str),
    End,
}

struct Lexer {
    chars: Vec<char>,
    position: usize,
}

impl Lexer {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            position: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let value = self.peek();
        if value.is_some() {
            self.position += 1;
        }
        value
    }

    fn token(&mut self) -> Result<Token, String> {
        while self.peek().is_some_and(char::is_whitespace) {
            self.position += 1;
        }
        let Some(current) = self.peek() else {
            return Ok(Token::End);
        };
        if current.is_ascii_digit()
            || (current == '.'
                && self
                    .chars
                    .get(self.position + 1)
                    .is_some_and(|next| next.is_ascii_digit()))
        {
            return self.number();
        }
        if current == '\'' || current == '"' {
            return self.text();
        }
        if current.is_ascii_alphabetic() || current == '_' {
            let mut name = String::new();
            while let Some(character) = self.peek() {
                if character.is_ascii_alphanumeric() || character == '_' {
                    name.push(character);
                    self.position += 1;
                } else {
                    break;
                }
            }
            return Ok(Token::Name(name));
        }
        for (text, punct) in [
            ("**", "**"),
            ("//", "//"),
            ("+", "+"),
            ("-", "-"),
            ("*", "*"),
            ("/", "/"),
            ("%", "%"),
            ("(", "("),
            (")", ")"),
            (",", ","),
        ] {
            let mut matches = true;
            for (offset, expected) in text.chars().enumerate() {
                if self.chars.get(self.position + offset) != Some(&expected) {
                    matches = false;
                    break;
                }
            }
            if matches {
                self.position += text.chars().count();
                return Ok(Token::Punct(punct));
            }
        }
        Err(format!("invalid syntax: unexpected character {current:?}"))
    }

    fn number(&mut self) -> Result<Token, String> {
        let start = self.position;
        let mut seen_dot = false;
        let mut seen_exponent = false;
        while let Some(character) = self.peek() {
            if character.is_ascii_digit() {
                self.position += 1;
            } else if character == '.' && !seen_dot && !seen_exponent {
                seen_dot = true;
                self.position += 1;
            } else if (character == 'e' || character == 'E') && !seen_exponent {
                seen_exponent = true;
                self.position += 1;
                if self.peek().is_some_and(|sign| sign == '+' || sign == '-') {
                    self.position += 1;
                }
            } else {
                break;
            }
        }
        let literal: String = self.chars[start..self.position].iter().collect();
        if seen_dot || seen_exponent {
            literal
                .parse::<f64>()
                .map(|value| Token::Number(Eval::Float(value)))
                .map_err(|_| "invalid syntax: invalid number".to_string())
        } else {
            literal
                .parse::<i128>()
                .map(|value| Token::Number(Eval::Int(value)))
                .map_err(|_| "invalid syntax: invalid number".to_string())
        }
    }

    fn text(&mut self) -> Result<Token, String> {
        let quote = self.bump().expect("text starts with a quote");
        let mut value = String::new();
        loop {
            let Some(character) = self.bump() else {
                return Err("invalid syntax: unterminated string literal".to_string());
            };
            if character == quote {
                return Ok(Token::Text(value));
            }
            if character != '\\' {
                value.push(character);
                continue;
            }
            let Some(escaped) = self.bump() else {
                return Err("invalid syntax: unterminated string literal".to_string());
            };
            match escaped {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                'r' => value.push('\r'),
                'b' => value.push('\u{8}'),
                'f' => value.push('\u{c}'),
                '\\' => value.push('\\'),
                '\'' => value.push('\''),
                '"' => value.push('"'),
                other => value.push(other),
            }
        }
    }
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn new(text: &str) -> Result<Self, String> {
        let mut lexer = Lexer::new(text);
        let mut tokens = Vec::new();
        loop {
            let token = lexer.token()?;
            let done = token == Token::End;
            tokens.push(token);
            if done {
                break;
            }
        }
        Ok(Self {
            tokens,
            position: 0,
        })
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.position).unwrap_or(&Token::End)
    }

    fn bump(&mut self) -> Token {
        let token = self.peek().clone();
        if self.position < self.tokens.len() {
            self.position += 1;
        }
        token
    }

    fn accept(&mut self, punct: &str) -> bool {
        if matches!(self.peek(), Token::Punct(value) if *value == punct) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expression(&mut self) -> Result<Node, String> {
        let mut node = self.term()?;
        loop {
            let operator = if self.accept("+") {
                BinOp::Add
            } else if self.accept("-") {
                BinOp::Sub
            } else {
                return Ok(node);
            };
            node = Node::Binary(operator, Box::new(node), Box::new(self.term()?));
        }
    }

    fn term(&mut self) -> Result<Node, String> {
        let mut node = self.factor()?;
        loop {
            let operator = if self.accept("*") {
                BinOp::Mul
            } else if self.accept("/") {
                BinOp::Div
            } else if self.accept("//") {
                BinOp::FloorDiv
            } else if self.accept("%") {
                BinOp::Mod
            } else {
                return Ok(node);
            };
            node = Node::Binary(operator, Box::new(node), Box::new(self.factor()?));
        }
    }

    fn factor(&mut self) -> Result<Node, String> {
        if self.accept("+") {
            return Ok(Node::Unary('+', Box::new(self.factor()?)));
        }
        if self.accept("-") {
            return Ok(Node::Unary('-', Box::new(self.factor()?)));
        }
        self.power()
    }

    fn power(&mut self) -> Result<Node, String> {
        let base = self.atom()?;
        if self.accept("**") {
            return Ok(Node::Binary(
                BinOp::Pow,
                Box::new(base),
                Box::new(self.factor()?),
            ));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Node, String> {
        match self.bump() {
            Token::Number(value) => Ok(Node::Value(value)),
            Token::Text(value) => Ok(Node::Value(Eval::Text(value))),
            Token::Name(name) => {
                if !self.accept("(") {
                    return Err(UNSUPPORTED.to_string());
                }
                let mut arguments = Vec::new();
                if !self.accept(")") {
                    loop {
                        arguments.push(self.expression()?);
                        if self.accept(")") {
                            break;
                        }
                        if !self.accept(",") {
                            return Err("invalid syntax: invalid syntax".to_string());
                        }
                    }
                }
                Ok(Node::Call(name, arguments))
            }
            Token::Punct("(") => {
                let inner = self.expression()?;
                if !self.accept(")") {
                    return Err("invalid syntax: '(' was never closed".to_string());
                }
                Ok(inner)
            }
            _ => Err("invalid syntax: invalid syntax".to_string()),
        }
    }

    fn finish(&mut self) -> Result<Node, String> {
        let node = self.expression()?;
        match self.peek() {
            Token::End => Ok(node),
            _ => Err("invalid syntax: invalid syntax".to_string()),
        }
    }
}

/// Evaluate one arithmetic/date expression and return `{expression, value}`.
pub fn calculate(expression: &str) -> Result<Value, String> {
    let cleaned = expression.trim();
    if cleaned.is_empty() {
        return Err("empty expression".to_string());
    }
    if cleaned.chars().count() > MAX_EXPRESSION_CHARS {
        return Err("expression too long".to_string());
    }
    let mut parser = Parser::new(cleaned)?;
    let node = parser.finish()?;
    let value = evaluate(&node)?;
    Ok(json!({"expression": cleaned, "value": format_value(&value)}))
}

fn evaluate(node: &Node) -> Result<Eval, String> {
    match node {
        Node::Value(value) => Ok(value.clone()),
        Node::Unary(operator, operand) => {
            let value = evaluate(operand)?;
            match (operator, value) {
                ('+', Eval::Int(value)) => Ok(Eval::Int(value)),
                ('+', Eval::Float(value)) => Ok(Eval::Float(value)),
                ('-', Eval::Int(value)) => Ok(Eval::Int(-value)),
                ('-', Eval::Float(value)) => Ok(Eval::Float(-value)),
                (_, other) => Err(format!(
                    "bad operand type for unary {}: '{}'",
                    operator,
                    type_name(&other)
                )),
            }
        }
        Node::Binary(operator, left, right) => {
            let left = evaluate(left)?;
            let right = evaluate(right)?;
            if *operator == BinOp::Pow {
                let exponent = match &right {
                    Eval::Int(value) => *value as f64,
                    Eval::Float(value) => *value,
                    Eval::Text(_) => {
                        return Err(format!(
                            "unsupported operand type(s) for ** or pow(): '{}' and 'str'",
                            type_name(&left)
                        ))
                    }
                };
                if exponent.abs() > MAX_EXPONENT as f64 {
                    return Err(format!("exponent above {MAX_EXPONENT}"));
                }
            }
            binary(*operator, left, right)
        }
        Node::Call(name, arguments) => {
            let mut values = Vec::with_capacity(arguments.len());
            for argument in arguments {
                values.push(evaluate(argument)?);
            }
            call(name, values)
        }
    }
}

fn binary(operator: BinOp, left: Eval, right: Eval) -> Result<Eval, String> {
    match operator {
        BinOp::Add => match (left, right) {
            (Eval::Int(a), Eval::Int(b)) => Ok(Eval::Int(a + b)),
            (Eval::Text(a), Eval::Text(b)) => Ok(Eval::Text(format!("{a}{b}"))),
            (a, b) if a.is_number() && b.is_number() => Ok(Eval::Float(a.float() + b.float())),
            (a, b) => Err(unsupported_operands("+", &a, &b)),
        },
        BinOp::Sub => match (left, right) {
            (Eval::Int(a), Eval::Int(b)) => Ok(Eval::Int(a - b)),
            (a, b) if a.is_number() && b.is_number() => Ok(Eval::Float(a.float() - b.float())),
            (a, b) => Err(unsupported_operands("-", &a, &b)),
        },
        BinOp::Mul => match (left, right) {
            (Eval::Int(a), Eval::Int(b)) => Ok(Eval::Int(a * b)),
            (Eval::Text(a), Eval::Int(b)) | (Eval::Int(b), Eval::Text(a)) => {
                Ok(Eval::Text(repeat(&a, b)))
            }
            (a, b) if a.is_number() && b.is_number() => Ok(Eval::Float(a.float() * b.float())),
            (a, b) => Err(unsupported_operands("*", &a, &b)),
        },
        BinOp::Div => {
            let (a, b) = (left.float(), right.float());
            if b == 0.0 {
                return Err(if left.is_float() || right.is_float() {
                    "float division by zero"
                } else {
                    DIVISION_BY_ZERO
                }
                .to_string());
            }
            Ok(Eval::Float(a / b))
        }
        BinOp::FloorDiv => {
            if left.is_float() || right.is_float() {
                let (a, b) = (left.float(), right.float());
                if b == 0.0 {
                    return Err("float floor division by zero".to_string());
                }
                return Ok(Eval::Float((a / b).floor()));
            }
            let (Eval::Int(a), Eval::Int(b)) = (left, right) else {
                unreachable!("both values are ints")
            };
            if b == 0 {
                return Err("integer division or modulo by zero".to_string());
            }
            Ok(Eval::Int(floor_division(a, b)))
        }
        BinOp::Mod => {
            if left.is_float() || right.is_float() {
                let (a, b) = (left.float(), right.float());
                if b == 0.0 {
                    return Err("float modulo".to_string());
                }
                return Ok(Eval::Float(modulo(a, b)));
            }
            let (Eval::Int(a), Eval::Int(b)) = (left, right) else {
                unreachable!("both values are ints")
            };
            if b == 0 {
                return Err("integer modulo by zero".to_string());
            }
            Ok(Eval::Int(modulo_int(a, b)))
        }
        BinOp::Pow => match (&left, &right) {
            (Eval::Int(base), Eval::Int(exponent)) => {
                if *exponent < 0 {
                    if *base == 0 {
                        return Err("0.0 cannot be raised to a negative power".to_string());
                    }
                    Ok(Eval::Float((*base as f64).powi(*exponent as i32)))
                } else {
                    let exponent = *exponent as u32;
                    let mut result: i128 = 1;
                    for _ in 0..exponent {
                        result = result.wrapping_mul(*base);
                    }
                    Ok(Eval::Int(result))
                }
            }
            (a, b) if a.is_number() && b.is_number() => {
                let result = a.float().powf(b.float());
                Ok(Eval::Float(result))
            }
            (a, b) => Err(unsupported_operands("** or pow()", a, b)),
        },
    }
}

fn call(name: &str, arguments: Vec<Eval>) -> Result<Eval, String> {
    match name {
        "today" => {
            if !arguments.is_empty() {
                return Err("today() takes no arguments".to_string());
            }
            Ok(Eval::Text(
                Local::now().date_naive().format("%Y-%m-%d").to_string(),
            ))
        }
        "days_between" => {
            if arguments.len() != 2 {
                return Err("days_between() takes two ISO dates".to_string());
            }
            let start = date_from(&arguments[0])?;
            let end = date_from(&arguments[1])?;
            Ok(Eval::Int((end - start).num_days() as i128))
        }
        "abs" | "round" | "min" | "max" => call_numeric(name, arguments),
        _ => Err(UNSUPPORTED.to_string()),
    }
}

fn call_numeric(name: &str, arguments: Vec<Eval>) -> Result<Eval, String> {
    match name {
        "abs" => {
            if arguments.len() != 1 {
                return Err(format!(
                    "abs() takes exactly one argument ({} given)",
                    arguments.len()
                ));
            }
            match &arguments[0] {
                Eval::Int(value) => Ok(Eval::Int(value.abs())),
                Eval::Float(value) => Ok(Eval::Float(value.abs())),
                other => Err(format!(
                    "bad operand type for abs(): '{}'",
                    type_name(other)
                )),
            }
        }
        "round" => {
            if arguments.is_empty() || arguments.len() > 2 {
                return Err(format!(
                    "round expected 1 argument, got {}",
                    arguments.len()
                ));
            }
            let digits = match arguments.get(1) {
                None => None,
                Some(Eval::Int(value)) => Some(*value as i64),
                Some(other) => {
                    return Err(format!(
                        "'{}' object cannot be interpreted as an integer",
                        type_name(other)
                    ))
                }
            };
            match &arguments[0] {
                Eval::Int(value) if digits.is_none() => Ok(Eval::Int(*value)),
                Eval::Int(value) => {
                    let digits = digits.expect("digits are present");
                    if digits >= 0 {
                        return Ok(Eval::Int(*value));
                    }
                    let factor = 10_i128.pow((-digits).min(38) as u32);
                    let scaled = format!("{:.0}", *value as f64 / factor as f64);
                    let whole: i128 = scaled.parse().unwrap_or(0);
                    Ok(Eval::Int(whole * factor))
                }
                Eval::Float(value) if digits.is_none() => {
                    let rounded: i128 = format!("{value:.0}")
                        .parse()
                        .map_err(|_| "cannot convert float to int".to_string())?;
                    Ok(Eval::Int(rounded))
                }
                Eval::Float(value) => {
                    let digits = digits.expect("digits are present");
                    Ok(Eval::Float(round_digits(*value, digits)))
                }
                other => Err(format!(
                    "type {} doesn't define __round__ method",
                    type_name(other)
                )),
            }
        }
        "min" | "max" => {
            let items = if arguments.len() == 1 {
                match &arguments[0] {
                    Eval::Text(text) => text
                        .chars()
                        .map(|character| Eval::Text(character.to_string()))
                        .collect::<Vec<_>>(),
                    other => return Err(format!("'{}' object is not iterable", type_name(other))),
                }
            } else {
                arguments
            };
            if items.is_empty() {
                return Err(format!("{name} expected at least 1 argument, got 0"));
            }
            let mut best = items[0].clone();
            for item in &items[1..] {
                let ordering = compare(&best, item)?;
                let replace = if name == "min" {
                    ordering == std::cmp::Ordering::Greater
                } else {
                    ordering == std::cmp::Ordering::Less
                };
                if replace {
                    best = item.clone();
                }
            }
            Ok(best)
        }
        _ => Err(UNSUPPORTED.to_string()),
    }
}

fn compare(left: &Eval, right: &Eval) -> Result<std::cmp::Ordering, String> {
    match (left, right) {
        (Eval::Int(a), Eval::Int(b)) => Ok(a.cmp(b)),
        (Eval::Text(a), Eval::Text(b)) => Ok(a.cmp(b)),
        (a, b) if a.is_number() && b.is_number() => a
            .float()
            .partial_cmp(&b.float())
            .ok_or_else(|| "cannot compare NaN".to_string()),
        (a, b) => Err(format!(
            "'<' not supported between instances of '{}' and '{}'",
            type_name(a),
            type_name(b)
        )),
    }
}

fn floor_division(a: i128, b: i128) -> i128 {
    let quotient = a / b;
    let remainder = a % b;
    if remainder != 0 && (remainder < 0) != (b < 0) {
        quotient - 1
    } else {
        quotient
    }
}

fn modulo_int(a: i128, b: i128) -> i128 {
    let remainder = a % b;
    if remainder != 0 && (remainder < 0) != (b < 0) {
        remainder + b
    } else {
        remainder
    }
}

fn modulo(a: f64, b: f64) -> f64 {
    let remainder = a % b;
    if remainder != 0.0 && (remainder < 0.0) != (b < 0.0) {
        remainder + b
    } else {
        remainder
    }
}

fn date_from(value: &Eval) -> Result<NaiveDate, String> {
    let text = format_value(value);
    NaiveDate::parse_from_str(&text, "%Y-%m-%d")
        .map_err(|_| format!("Invalid isoformat string: '{text}'"))
}

fn round_digits(value: f64, digits: i64) -> f64 {
    if digits >= 17 {
        return value;
    }
    if digits < -308 {
        return 0.0;
    }
    if digits >= 0 {
        format!("{value:.digits$}", digits = digits as usize)
            .parse::<f64>()
            .unwrap_or(value)
    } else {
        let factor = 10_f64.powi((-digits) as i32);
        let scaled = value / factor;
        let whole: f64 = format!("{scaled:.0}").parse().unwrap_or(scaled);
        whole * factor
    }
}

fn type_name(value: &Eval) -> &'static str {
    match value {
        Eval::Int(_) => "int",
        Eval::Float(_) => "float",
        Eval::Text(_) => "str",
    }
}

fn repeat(text: &str, count: i128) -> String {
    if count <= 0 {
        String::new()
    } else {
        // shortcut: repeat is bounded at 1M chars to cap memory; raise if huge repeats matter.
        text.repeat(count.min(1_000_000) as usize)
    }
}

fn unsupported_operands(operator: &str, left: &Eval, right: &Eval) -> String {
    format!(
        "unsupported operand type(s) for {operator}: '{}' and '{}'",
        type_name(left),
        type_name(right)
    )
}

impl Eval {
    fn is_number(&self) -> bool {
        matches!(self, Eval::Int(_) | Eval::Float(_))
    }

    fn is_float(&self) -> bool {
        matches!(self, Eval::Float(_))
    }

    fn float(&self) -> f64 {
        match self {
            Eval::Int(value) => *value as f64,
            Eval::Float(value) => *value,
            Eval::Text(_) => 0.0,
        }
    }
}

/// `_format`: ints exact, floats rounded to six places, everything else as text.
fn format_value(value: &Eval) -> String {
    match value {
        Eval::Int(value) => value.to_string(),
        Eval::Float(value) => {
            if value.is_nan() || value.is_infinite() {
                return "result is not a finite number".to_string();
            }
            let rounded = round_digits(*value, 6);
            python_float_repr(rounded)
        }
        Eval::Text(text) => text.clone(),
    }
}

/// Python's `repr(float)`: shortest round-trip digits, exponent >=16 or < -4.
fn python_float_repr(value: f64) -> String {
    let rendered = format!("{value:?}");
    let Some((mantissa, exponent)) = rendered.split_once(['e', 'E']) else {
        return rendered;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return rendered;
    };
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(expression: &str) -> String {
        calculate(expression)
            .expect("expression evaluates")
            .get("value")
            .and_then(Value::as_str)
            .expect("value is a string")
            .to_string()
    }

    fn error(expression: &str) -> String {
        calculate(expression).expect_err("expression is rejected")
    }

    #[test]
    fn arithmetic_matches_the_reference() {
        assert_eq!(value("3 * 19.99"), "59.97");
        assert_eq!(value("round(10 / 3, 2)"), "3.33");
        assert_eq!(value("1 + 2 * 3"), "7");
        assert_eq!(value("(1 + 2) * 3"), "9");
        assert_eq!(value("2 ** 3 ** 2"), "512");
        assert_eq!(value("-2 ** 2"), "-4");
        assert_eq!(value("2 ** -1"), "0.5");
        assert_eq!(value("0 / 5"), "0.0");
        assert_eq!(value("7 / 2"), "3.5");
    }

    #[test]
    fn floor_division_and_modulo_follow_python_signs() {
        assert_eq!(value("-7 // 2"), "-4");
        assert_eq!(value("7 // -2"), "-4");
        assert_eq!(value("-7 % 2"), "1");
        assert_eq!(value("7 % -2"), "-1");
        assert_eq!(value("-7.0 // 2"), "-4.0");
        assert_eq!(value("-7.0 % 2"), "1.0");
    }

    #[test]
    fn division_and_modulo_by_zero_are_handler_errors() {
        assert_eq!(error("1 / 0"), "division by zero");
        assert_eq!(error("1 // 0"), "integer division or modulo by zero");
        assert_eq!(error("1 % 0"), "integer modulo by zero");
        assert_eq!(error("1.0 / 0"), "float division by zero");
        assert_eq!(error("1.0 // 0"), "float floor division by zero");
        assert_eq!(error("1.0 % 0"), "float modulo");
        assert_eq!(error("0 ** -1"), "0.0 cannot be raised to a negative power");
    }

    #[test]
    fn exponent_cap_matches_the_reference() {
        assert_eq!(value("2 ** 12"), "4096");
        assert_eq!(error("2 ** 13"), "exponent above 12");
        assert_eq!(error("2 ** -13"), "exponent above 12");
        assert_eq!(value("2 ** 0"), "1");
    }

    #[test]
    fn dates_and_calls_follow_the_reference() {
        assert_eq!(value("days_between('2026-11-01','2026-12-01')"), "30");
        assert_eq!(value("days_between('2026-12-01','2026-11-01')"), "-30");
        assert_eq!(value("abs(-4)"), "4");
        assert_eq!(value("round(2.5)"), "2");
        assert_eq!(value("round(3.5)"), "4");
        assert_eq!(value("round(2.675, 2)"), "2.67");
        assert_eq!(value("round(1234, 2)"), "1234");
        assert_eq!(value("round(1234, -2)"), "1200");
        assert_eq!(value("round(25, -1)"), "20");
        assert_eq!(value("min(3, 1, 2)"), "1");
        assert_eq!(value("max(3, 1, 2)"), "3");
        assert_eq!(value("min('abc')"), "a");
        assert_eq!(value("'a' + 'b'"), "ab");
        assert_eq!(value("'ab' * 2"), "abab");
        assert!(value("today()").len() == 10);
        assert_eq!(
            error("days_between('x','2026-12-01')"),
            "Invalid isoformat string: 'x'"
        );
        assert_eq!(error("today(1)"), "today() takes no arguments");
        assert_eq!(
            error("days_between('2026-01-01')"),
            "days_between() takes two ISO dates"
        );
        assert_eq!(error("min()"), "min expected at least 1 argument, got 0");
    }

    #[test]
    fn formatting_uses_python_float_repr() {
        assert_eq!(value("1.0"), "1.0");
        assert_eq!(value("0.1 + 0.2"), "0.3");
        assert_eq!(value("1 / 3"), "0.333333");
        assert_eq!(value("1e16"), "1e+16");
        assert_eq!(value("1e-5"), "1e-05");
    }

    #[test]
    fn unsupported_input_is_rejected() {
        for expression in [
            "",
            "   ",
            "__import__('os')",
            "open('x')",
            "True",
            "None",
            "[1, 2]",
            "{'a': 1}",
            "1 if 1 else 2",
            "'a' - 'b'",
            "abs('x')",
        ] {
            let _ = error(expression);
        }
        assert_eq!(error("__import__('os')"), "unsupported expression");
        assert_eq!(error(""), "empty expression");
        assert_eq!(error(&"1".repeat(501)), "expression too long");
    }
}
