//! The search query language.
//!
//! A query is a sequence of terms, combined with an implicit AND:
//!
//! ```text
//! subject:invoice from:accounts
//! ```
//!
//! `OR` joins alternatives, `-` negates, and parentheses group. `AND` may be
//! written out where the space already means it, since people reach for it
//! without thinking. A term with no field searches subject, sender and
//! recipient together, which is what a bare word has always done here.
//!
//! The same parse tree drives both halves of search, which is the point of
//! putting it in one place:
//!
//! * [`Query::to_imap`] compiles it to an RFC 3501 `SEARCH` command, for the
//!   server-side search that Enter runs.
//! * [`Query::matches`] evaluates it against a cached [`Envelope`], for the
//!   as-you-type filter over the messages already on screen.
//!
//! The two are not identical, and cannot be: the client has only what it has
//! cached. `body:` reaches the whole message on the server and only the
//! cached preview locally, so the filter is a subset of what Enter finds.
//! Every other term means the same thing on both sides.

use anyhow::{Result, bail};
use chrono::{Datelike, NaiveDate, TimeZone as _, Utc};

use super::imap::quote;
use super::model::{Envelope, Flags};

/// A parsed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    root: Node,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    /// An empty query, which excludes nothing.
    Everything,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Term(Term),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Term {
    Text { field: Field, value: String },
    Flag { bit: u16, present: bool },
    HasAttachment,
    Date { test: DateTest, date: NaiveDate },
    Size { at_least: bool, bytes: u64 },
}

/// Which part of a message a text term looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    /// No field given: subject, sender and recipient together.
    Any,
    Subject,
    From,
    To,
    Cc,
    Bcc,
    /// The message body. Locally this can only see the cached preview.
    Body,
    /// Headers and body together — everything the server has.
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateTest {
    Before,
    Since,
    On,
}

impl Query {
    /// Parses a query. An empty string is a query that matches everything.
    pub fn parse(input: &str) -> Result<Self> {
        let tokens = tokenize(input)?;
        if tokens.is_empty() {
            return Ok(Query { root: Node::Everything });
        }
        let mut parser = Parser { tokens: &tokens, at: 0 };
        let root = parser.parse_or()?;
        if parser.at < parser.tokens.len() {
            bail!("unexpected `)`");
        }
        Ok(Query { root })
    }

    /// Compiles to the body of an IMAP `SEARCH` command.
    ///
    /// Fails on an empty query rather than returning `ALL`: searching every
    /// message in every folder is never what the box was for.
    pub fn to_imap(&self) -> Result<String> {
        if self.root == Node::Everything {
            bail!("empty search");
        }
        let compiled = imap_key(&self.root, false);
        // A quoted string outside ASCII has to say what it is encoded in, or
        // the server is entitled to reject the command.
        if compiled.is_ascii() { Ok(compiled) } else { Ok(format!("CHARSET UTF-8 {compiled}")) }
    }

    /// Whether a cached envelope satisfies the query.
    pub fn matches(&self, envelope: &Envelope) -> bool {
        evaluate(&self.root, envelope)
    }

    /// Whether any term reaches past what the cache holds, so the local
    /// filter is only an approximation of the server's answer.
    pub fn needs_the_server(&self) -> bool {
        fn walk(node: &Node) -> bool {
            match node {
                Node::Everything => false,
                Node::And(parts) | Node::Or(parts) => parts.iter().any(walk),
                Node::Not(inner) => walk(inner),
                Node::Term(Term::Text { field, .. }) => {
                    matches!(field, Field::Body | Field::Text)
                }
                Node::Term(_) => false,
            }
        }
        walk(&self.root)
    }
}

// ---------------------------------------------------------------- tokenizer

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Or,
    And,
    Not,
    /// A bare or `field:`-prefixed word. The value keeps its original case;
    /// matching lowercases both sides.
    Word {
        field: Option<String>,
        value: String,
    },
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;

    while at < chars.len() {
        match chars[at] {
            c if c.is_whitespace() => at += 1,
            '(' => {
                tokens.push(Token::Open);
                at += 1;
            }
            ')' => {
                tokens.push(Token::Close);
                at += 1;
            }
            // A `-` only negates when it starts a term. Inside one it is an
            // ordinary character, so `from:mary-jane` stays one word.
            '-' if at + 1 < chars.len() && !chars[at + 1].is_whitespace() => {
                tokens.push(Token::Not);
                at += 1;
            }
            _ => {
                let (token, next) = read_word(&chars, at)?;
                tokens.push(token);
                at = next;
            }
        }
    }
    Ok(tokens)
}

/// Reads one word, which may carry a `field:` prefix and may be quoted on
/// either side of the colon.
fn read_word(chars: &[char], start: usize) -> Result<(Token, usize)> {
    let (first, mut at) = read_atom(chars, start)?;

    // A colon straight after an unquoted atom makes it a field name.
    if at < chars.len() && chars[at] == ':' && !first.was_quoted {
        at += 1;
        let field = first.text.to_ascii_lowercase();
        if at >= chars.len() || chars[at].is_whitespace() || chars[at] == ')' {
            bail!("`{field}:` needs a value");
        }
        let (second, next) = read_atom(chars, at)?;
        if second.text.is_empty() {
            bail!("`{field}:` needs a value");
        }
        return Ok((Token::Word { field: Some(field), value: second.text }, next));
    }

    if first.text.is_empty() {
        bail!("empty search term");
    }
    // `OR` and `AND` are only operators when written bare; `"or"` searches
    // for the word, which is the escape hatch for a message about
    // conjunctions.
    if !first.was_quoted && first.text.eq_ignore_ascii_case("or") {
        return Ok((Token::Or, at));
    }
    if !first.was_quoted && first.text.eq_ignore_ascii_case("and") {
        return Ok((Token::And, at));
    }
    Ok((Token::Word { field: None, value: first.text }, at))
}

struct Atom {
    text: String,
    was_quoted: bool,
}

/// Reads a quoted string or a run of ordinary characters.
fn read_atom(chars: &[char], start: usize) -> Result<(Atom, usize)> {
    let mut at = start;

    if chars[at] == '"' {
        at += 1;
        let mut text = String::new();
        while at < chars.len() {
            match chars[at] {
                '"' => return Ok((Atom { text, was_quoted: true }, at + 1)),
                // Backslash escapes the next character, so a quotation mark
                // can be searched for.
                '\\' if at + 1 < chars.len() => {
                    text.push(chars[at + 1]);
                    at += 2;
                }
                c => {
                    text.push(c);
                    at += 1;
                }
            }
        }
        bail!("unclosed quote");
    }

    let mut text = String::new();
    while at < chars.len() {
        match chars[at] {
            c if c.is_whitespace() => break,
            '(' | ')' | ':' | '"' => break,
            c => {
                text.push(c);
                at += 1;
            }
        }
    }
    Ok((Atom { text, was_quoted: false }, at))
}

// ------------------------------------------------------------------- parser

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
}

impl Parser<'_> {
    /// `or := and ( "OR" and )*`
    fn parse_or(&mut self) -> Result<Node> {
        let mut parts = vec![self.parse_and()?];
        while self.peek() == Some(&Token::Or) {
            self.at += 1;
            if self.at >= self.tokens.len() {
                bail!("`OR` needs something after it");
            }
            parts.push(self.parse_and()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Node::Or(parts) })
    }

    /// `and := unary ( "AND"? unary )*`, with juxtaposition meaning AND.
    ///
    /// Writing the conjunction out is allowed because people do it without
    /// thinking — `subject:invoice and from:jane` — and reading it as one
    /// more word to search for turns a query that should match into one that
    /// cannot: the word has to be in the subject or an address as well.
    fn parse_and(&mut self) -> Result<Node> {
        let mut parts = Vec::new();
        while let Some(token) = self.peek() {
            match token {
                Token::Or | Token::Close => break,
                Token::And => {
                    if parts.is_empty() {
                        bail!("`AND` needs something before it");
                    }
                    self.at += 1;
                    if !matches!(self.peek(), Some(Token::Word { .. } | Token::Not | Token::Open)) {
                        bail!("`AND` needs something after it");
                    }
                }
                _ => parts.push(self.parse_unary()?),
            }
        }
        match parts.len() {
            // Only the whole query may be empty, and that is handled before
            // the parser runs. Here it means an operator with nothing beside
            // it, as in a leading `OR`.
            0 => bail!("`OR` needs something before it"),
            1 => Ok(parts.pop().unwrap()),
            _ => Ok(Node::And(parts)),
        }
    }

    /// `unary := "-" unary | "(" or ")" | term`
    fn parse_unary(&mut self) -> Result<Node> {
        match self.peek() {
            Some(Token::Not) => {
                self.at += 1;
                if self.at >= self.tokens.len() {
                    bail!("`-` needs a term after it");
                }
                Ok(Node::Not(Box::new(self.parse_unary()?)))
            }
            Some(Token::Open) => {
                self.at += 1;
                let inner = self.parse_or()?;
                if self.peek() != Some(&Token::Close) {
                    bail!("unclosed `(`");
                }
                self.at += 1;
                Ok(inner)
            }
            Some(Token::Word { .. }) => {
                let Some(Token::Word { field, value }) = self.tokens.get(self.at).cloned() else {
                    unreachable!("just matched a word")
                };
                self.at += 1;
                Ok(Node::Term(term(field.as_deref(), &value)?))
            }
            Some(Token::Or) => bail!("`OR` needs something before it"),
            Some(Token::And) => bail!("`AND` needs something before it"),
            Some(Token::Close) | None => bail!("expected a search term"),
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }
}

/// Builds a term from a field name and its value.
fn term(field: Option<&str>, value: &str) -> Result<Term> {
    let text = |field: Field| Term::Text { field, value: value.to_string() };

    let Some(field) = field else {
        return Ok(text(Field::Any));
    };

    Ok(match field {
        "subject" | "subj" => text(Field::Subject),
        "from" => text(Field::From),
        "to" => text(Field::To),
        "cc" => text(Field::Cc),
        "bcc" => text(Field::Bcc),
        "body" => text(Field::Body),
        "text" => text(Field::Text),

        "is" => match value.to_ascii_lowercase().as_str() {
            "unread" | "unseen" => Term::Flag { bit: Flags::SEEN, present: false },
            "read" | "seen" => Term::Flag { bit: Flags::SEEN, present: true },
            "starred" | "flagged" => Term::Flag { bit: Flags::FLAGGED, present: true },
            "unstarred" | "unflagged" => Term::Flag { bit: Flags::FLAGGED, present: false },
            "answered" | "replied" => Term::Flag { bit: Flags::ANSWERED, present: true },
            "draft" => Term::Flag { bit: Flags::DRAFT, present: true },
            "deleted" => Term::Flag { bit: Flags::DELETED, present: true },
            other => bail!("`is:{other}` is not something to search for"),
        },

        "has" => match value.to_ascii_lowercase().as_str() {
            "attachment" | "attachments" | "file" => Term::HasAttachment,
            other => bail!("`has:{other}` is not something to search for"),
        },

        "before" => Term::Date { test: DateTest::Before, date: parse_date(value)? },
        "since" | "after" => Term::Date { test: DateTest::Since, date: parse_date(value)? },
        "on" => Term::Date { test: DateTest::On, date: parse_date(value)? },

        "larger" | "bigger" => Term::Size { at_least: true, bytes: parse_size(value)? },
        "smaller" => Term::Size { at_least: false, bytes: parse_size(value)? },

        other => bail!("`{other}:` is not a field; try subject, from, to, cc, body or is"),
    })
}

/// Accepts `2026-09-12` and offsets like `7d`, `2w`, `3m`, `1y`.
fn parse_date(value: &str) -> Result<NaiveDate> {
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(date);
    }

    let lower = value.to_ascii_lowercase();
    if let Some(unit) = lower.chars().last()
        && let Ok(count) = lower[..lower.len() - unit.len_utf8()].parse::<i64>()
    {
        let today = Utc::now().date_naive();
        let shifted = match unit {
            'd' => today.checked_sub_signed(chrono::Duration::days(count)),
            'w' => today.checked_sub_signed(chrono::Duration::weeks(count)),
            'm' => shift_months(today, count),
            'y' => shift_months(today, count.saturating_mul(12)),
            _ => None,
        };
        if let Some(date) = shifted {
            return Ok(date);
        }
    }

    bail!("`{value}` is not a date; use 2026-09-12, or an offset like 7d, 2w, 3m, 1y")
}

/// Moves `count` months back, clamping the day so that a month end does not
/// roll into the following month.
fn shift_months(from: NaiveDate, count: i64) -> Option<NaiveDate> {
    let months = i64::from(from.year()) * 12 + i64::from(from.month0()) - count;
    let year = i32::try_from(months.div_euclid(12)).ok()?;
    let month = u32::try_from(months.rem_euclid(12)).ok()? + 1;
    for day in (1..=from.day()).rev() {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
            return Some(date);
        }
    }
    None
}

/// Accepts a byte count, optionally suffixed `k`, `m` or `g`.
fn parse_size(value: &str) -> Result<u64> {
    let lower = value.to_ascii_lowercase();
    // Trailing "b" as in "10kb" is noise, but people write it.
    let lower = lower.strip_suffix('b').unwrap_or(&lower);
    let (digits, scale) = match lower.chars().last() {
        Some('k') => (&lower[..lower.len() - 1], 1024),
        Some('m') => (&lower[..lower.len() - 1], 1024 * 1024),
        Some('g') => (&lower[..lower.len() - 1], 1024 * 1024 * 1024),
        _ => (lower, 1),
    };

    match digits.trim().parse::<u64>() {
        Ok(count) => Ok(count.saturating_mul(scale)),
        Err(_) => bail!("`{value}` is not a size; use 200k, 2m or a plain byte count"),
    }
}

// ------------------------------------------------------------ IMAP compiler

/// Compiles a node to an IMAP search key.
///
/// `grouped` asks for a parenthesised key, which is what an operand of `OR`
/// or `NOT` needs when it is itself a conjunction.
fn imap_key(node: &Node, grouped: bool) -> String {
    match node {
        Node::Everything => "ALL".to_string(),
        Node::And(parts) => {
            let joined = parts.iter().map(|p| imap_key(p, true)).collect::<Vec<_>>().join(" ");
            if grouped { format!("({joined})") } else { joined }
        }
        // IMAP writes OR in prefix form and takes exactly two keys, so a
        // longer alternation folds to the right.
        Node::Or(parts) => match parts.as_slice() {
            [] => "ALL".to_string(),
            [only] => imap_key(only, grouped),
            [first, rest @ ..] => {
                let tail = imap_key(&Node::Or(rest.to_vec()), true);
                let alternation = format!("OR {} {tail}", imap_key(first, true));
                if grouped { format!("({alternation})") } else { alternation }
            }
        },
        Node::Not(inner) => format!("NOT {}", imap_key(inner, true)),
        Node::Term(term) => imap_term(term),
    }
}

fn imap_term(term: &Term) -> String {
    match term {
        Term::Text { field, value } => {
            let value = quote(value);
            match field {
                Field::Any => format!("(OR OR SUBJECT {value} FROM {value} TO {value})"),
                Field::Subject => format!("SUBJECT {value}"),
                Field::From => format!("FROM {value}"),
                Field::To => format!("TO {value}"),
                Field::Cc => format!("CC {value}"),
                Field::Bcc => format!("BCC {value}"),
                Field::Body => format!("BODY {value}"),
                Field::Text => format!("TEXT {value}"),
            }
        }
        Term::Flag { bit, present } => match (*bit, *present) {
            (Flags::SEEN, true) => "SEEN".to_string(),
            (Flags::SEEN, false) => "UNSEEN".to_string(),
            (Flags::FLAGGED, true) => "FLAGGED".to_string(),
            (Flags::FLAGGED, false) => "UNFLAGGED".to_string(),
            (Flags::ANSWERED, true) => "ANSWERED".to_string(),
            (Flags::ANSWERED, false) => "UNANSWERED".to_string(),
            (Flags::DRAFT, true) => "DRAFT".to_string(),
            (Flags::DELETED, true) => "DELETED".to_string(),
            (bit, present) => {
                let keyword = format!("KEYWORD {}", quote(Flags::imap_name(bit)));
                if present { keyword } else { format!("NOT {keyword}") }
            }
        },
        // IMAP has no attachment predicate. This is the closest a server can
        // answer, and it is an approximation in both directions: an inline
        // image makes multipart/related, and a message can be multipart/mixed
        // for reasons that are not attachments. The local filter, which has
        // the parsed structure, is exact.
        Term::HasAttachment => "HEADER Content-Type \"multipart/mixed\"".to_string(),
        Term::Date { test, date } => {
            let day = date.format("%d-%b-%Y");
            match test {
                DateTest::Before => format!("BEFORE {day}"),
                DateTest::Since => format!("SINCE {day}"),
                DateTest::On => format!("ON {day}"),
            }
        }
        Term::Size { at_least, bytes } => {
            if *at_least {
                format!("LARGER {bytes}")
            } else {
                format!("SMALLER {bytes}")
            }
        }
    }
}

// ------------------------------------------------------------ local matcher

fn evaluate(node: &Node, envelope: &Envelope) -> bool {
    match node {
        Node::Everything => true,
        Node::And(parts) => parts.iter().all(|p| evaluate(p, envelope)),
        Node::Or(parts) => parts.iter().any(|p| evaluate(p, envelope)),
        Node::Not(inner) => !evaluate(inner, envelope),
        Node::Term(term) => evaluate_term(term, envelope),
    }
}

fn evaluate_term(term: &Term, envelope: &Envelope) -> bool {
    match term {
        Term::Text { field, value } => {
            let needle = value.to_ascii_lowercase();
            let hit = |text: &str| text.to_ascii_lowercase().contains(&needle);
            let any =
                |addrs: &[super::model::Addr]| addrs.iter().any(|a| hit(&a.name) || hit(&a.email));
            match field {
                Field::Any => {
                    hit(&envelope.subject)
                        || hit(&envelope.preview)
                        || any(&envelope.from)
                        || any(&envelope.to)
                }
                Field::Subject => hit(&envelope.subject),
                Field::From => any(&envelope.from),
                Field::To => any(&envelope.to),
                Field::Cc => any(&envelope.cc),
                // The cache holds no Bcc: it is stripped in transit, and the
                // copy in Sent is the only place it survives.
                Field::Bcc => false,
                // Only the preview is cached, so this is a prefix of the body
                // rather than the body. Enter searches the real thing.
                Field::Body => hit(&envelope.preview),
                Field::Text => {
                    hit(&envelope.subject)
                        || hit(&envelope.preview)
                        || any(&envelope.from)
                        || any(&envelope.to)
                        || any(&envelope.cc)
                }
            }
        }
        Term::Flag { bit, present } => envelope.flags.has(*bit) == *present,
        Term::HasAttachment => envelope.has_attachments,
        Term::Date { test, date } => {
            // IMAP compares against the date part alone, so the day is a
            // half-open window in UTC.
            let Some(midnight) = date.and_hms_opt(0, 0, 0) else { return false };
            let start = Utc.from_utc_datetime(&midnight).timestamp();
            let end = start + 24 * 60 * 60;
            match test {
                DateTest::Before => envelope.date < start,
                DateTest::Since => envelope.date >= start,
                DateTest::On => envelope.date >= start && envelope.date < end,
            }
        }
        Term::Size { at_least, bytes } => {
            let size = u64::from(envelope.size);
            if *at_least { size > *bytes } else { size < *bytes }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imap(input: &str) -> String {
        Query::parse(input).expect("parses").to_imap().expect("compiles")
    }

    #[test]
    fn a_bare_word_searches_the_usual_fields() {
        assert_eq!(imap("report"), r#"(OR OR SUBJECT "report" FROM "report" TO "report")"#);
    }

    #[test]
    fn fields_compile_to_their_imap_keys() {
        assert_eq!(imap("subject:invoice"), r#"SUBJECT "invoice""#);
        assert_eq!(imap("from:jane"), r#"FROM "jane""#);
        assert_eq!(imap("cc:team"), r#"CC "team""#);
        assert_eq!(imap("body:contract"), r#"BODY "contract""#);
    }

    #[test]
    fn terms_are_anded_by_juxtaposition() {
        assert_eq!(imap("subject:invoice from:jane"), r#"SUBJECT "invoice" FROM "jane""#);
    }

    /// Writing the conjunction out has to mean the conjunction. Read as a
    /// word it would be one more thing to find in the subject or an address,
    /// and a query that should match would return nothing.
    #[test]
    fn and_may_be_written_out() {
        assert_eq!(imap("subject:pickleball and from:dupr"), r#"SUBJECT "pickleball" FROM "dupr""#);
        assert_eq!(imap("subject:pickleball AND from:dupr"), imap("subject:pickleball from:dupr"));
        assert_eq!(imap("a AND b AND c"), imap("a b c"));
    }

    #[test]
    fn an_explicit_and_still_binds_tighter_than_or() {
        assert_eq!(imap("subject:x AND from:y OR to:z"), imap("subject:x from:y OR to:z"));
    }

    #[test]
    fn and_joins_negated_and_grouped_terms_too() {
        assert_eq!(imap("subject:x AND -from:y"), imap("subject:x -from:y"));
        assert_eq!(imap("subject:x AND (from:y OR to:z)"), imap("subject:x (from:y OR to:z)"));
    }

    /// The escape hatch: a message actually about the word.
    #[test]
    fn a_quoted_and_is_searched_for() {
        assert_eq!(imap(r#""and""#), r#"(OR OR SUBJECT "and" FROM "and" TO "and")"#);
        assert_eq!(imap(r#"subject:and"#), r#"SUBJECT "and""#);
    }

    #[test]
    fn a_dangling_and_is_an_error() {
        assert!(Query::parse("and").is_err());
        assert!(Query::parse("subject:x and").is_err());
        assert!(Query::parse("and subject:x").is_err());
        assert!(Query::parse("subject:x and and from:y").is_err());
        assert!(Query::parse("subject:x and OR from:y").is_err());
    }

    #[test]
    fn quoted_values_keep_their_spaces() {
        assert_eq!(imap(r#"subject:"quarterly report""#), r#"SUBJECT "quarterly report""#);
        assert_eq!(
            imap(r#""two words""#),
            r#"(OR OR SUBJECT "two words" FROM "two words" TO "two words")"#
        );
    }

    #[test]
    fn or_is_written_in_prefix_form() {
        assert_eq!(imap("from:jane OR from:paul"), r#"OR FROM "jane" FROM "paul""#);
    }

    #[test]
    fn a_longer_alternation_folds_to_the_right() {
        // IMAP's OR takes exactly two keys, so the tail is parenthesised
        // rather than left to the server's associativity.
        assert_eq!(imap("from:a OR from:b OR from:c"), r#"OR FROM "a" (OR FROM "b" FROM "c")"#);
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // `a b OR c` is `(a AND b) OR c`, so the conjunction has to be
        // parenthesised for the server to read it the same way.
        assert_eq!(imap("subject:x from:y OR to:z"), r#"OR (SUBJECT "x" FROM "y") TO "z""#);
    }

    #[test]
    fn parentheses_override_precedence() {
        assert_eq!(imap("subject:x (from:y OR to:z)"), r#"SUBJECT "x" (OR FROM "y" TO "z")"#);
    }

    #[test]
    fn a_leading_dash_negates() {
        assert_eq!(imap("-from:noreply"), r#"NOT FROM "noreply""#);
        assert_eq!(
            imap("subject:invoice -from:noreply"),
            r#"SUBJECT "invoice" NOT FROM "noreply""#
        );
    }

    #[test]
    fn a_dash_inside_a_word_is_just_a_dash() {
        assert_eq!(imap("from:mary-jane"), r#"FROM "mary-jane""#);
    }

    #[test]
    fn flags_compile_to_their_keys() {
        assert_eq!(imap("is:unread"), "UNSEEN");
        assert_eq!(imap("is:read"), "SEEN");
        assert_eq!(imap("is:starred"), "FLAGGED");
        assert_eq!(imap("-is:starred"), "NOT FLAGGED");
    }

    #[test]
    fn dates_use_the_imap_spelling() {
        assert_eq!(imap("since:2026-01-31"), "SINCE 31-Jan-2026");
        assert_eq!(imap("before:2026-12-01"), "BEFORE 01-Dec-2026");
        assert_eq!(imap("on:2026-09-12"), "ON 12-Sep-2026");
    }

    #[test]
    fn relative_dates_resolve_against_today() {
        let week = Utc::now().date_naive() - chrono::Duration::days(7);
        assert_eq!(imap("since:7d"), format!("SINCE {}", week.format("%d-%b-%Y")));
    }

    #[test]
    fn month_offsets_clamp_rather_than_overflow_the_month() {
        // 31 March minus one month is 28 or 29 February, never 3 March.
        let march = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        assert_eq!(shift_months(march, 1), NaiveDate::from_ymd_opt(2026, 2, 28));
        let jan = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        assert_eq!(shift_months(jan, 1), NaiveDate::from_ymd_opt(2025, 12, 15));
    }

    #[test]
    fn sizes_accept_suffixes() {
        assert_eq!(parse_size("2048").unwrap(), 2048);
        assert_eq!(parse_size("200k").unwrap(), 200 * 1024);
        assert_eq!(parse_size("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("10kb").unwrap(), 10 * 1024);
        assert!(parse_size("big").is_err());
        assert_eq!(imap("larger:2m"), "LARGER 2097152");
    }

    #[test]
    fn values_are_escaped_for_the_wire() {
        assert_eq!(imap(r#"subject:"say \"hi\"""#), r#"SUBJECT "say \"hi\"""#);
    }

    #[test]
    fn a_query_outside_ascii_declares_its_charset() {
        assert_eq!(imap("subject:naïve"), "CHARSET UTF-8 SUBJECT \"naïve\"");
        assert!(!imap("subject:plain").contains("CHARSET"));
    }

    #[test]
    fn or_can_be_searched_for_when_quoted() {
        assert_eq!(imap(r#"subject:"or""#), r#"SUBJECT "or""#);
    }

    #[test]
    fn an_empty_query_has_no_search_to_run() {
        assert!(Query::parse("").unwrap().to_imap().is_err());
        assert!(Query::parse("   ").unwrap().to_imap().is_err());
        // But it excludes nothing, so the unfiltered list is what shows.
        assert!(Query::parse("").unwrap().matches(&envelope()));
    }

    #[test]
    fn malformed_queries_say_what_is_wrong() {
        let message = |q: &str| Query::parse(q).unwrap_err().to_string();
        assert!(message("subject:").contains("needs a value"));
        assert!(message(r#"subject:"unclosed"#).contains("unclosed quote"));
        assert!(message("(from:a").contains("unclosed `("));
        assert!(message("from:a)").contains("unexpected `)`"));
        assert!(message("OR from:a").contains("before it"));
        assert!(message("from:a OR").contains("after it"));
        assert!(message("colour:red").contains("is not a field"));
        assert!(message("is:important").contains("not something to search for"));
    }

    // -- local evaluation ------------------------------------------------

    fn envelope() -> Envelope {
        Envelope {
            subject: "Quarterly report".into(),
            from: vec![super::super::model::Addr {
                name: "Jane Doe".into(),
                email: "jane@example.com".into(),
            }],
            to: vec![super::super::model::Addr {
                name: "Team".into(),
                email: "team@example.com".into(),
            }],
            preview: "The numbers are attached.".into(),
            has_attachments: true,
            size: 4096,
            // 2026-09-12T00:00:00Z
            date: 1_789_171_200,
            ..Default::default()
        }
    }

    fn hits(query: &str) -> bool {
        Query::parse(query).expect("parses").matches(&envelope())
    }

    #[test]
    fn the_local_filter_reads_the_same_query() {
        assert!(hits("subject:quarterly"));
        assert!(hits("from:jane"));
        assert!(hits("from:example.com"));
        assert!(!hits("from:paul"));
        assert!(hits("subject:quarterly from:jane"));
        assert!(!hits("subject:quarterly from:paul"));
        assert!(hits("from:paul OR from:jane"));
        assert!(hits("-from:paul"));
    }

    #[test]
    fn matching_ignores_case_on_both_sides() {
        assert!(hits("subject:QUARTERLY"));
        assert!(hits("SUBJECT:quarterly"));
    }

    #[test]
    fn attachments_are_exact_locally() {
        assert!(hits("has:attachment"));
        let mut without = envelope();
        without.has_attachments = false;
        assert!(!Query::parse("has:attachment").unwrap().matches(&without));
    }

    #[test]
    fn flags_and_sizes_evaluate_locally() {
        assert!(hits("is:unread"));
        assert!(!hits("is:read"));
        assert!(hits("larger:2k"));
        assert!(hits("smaller:8k"));
        assert!(!hits("larger:8k"));
    }

    #[test]
    fn dates_evaluate_against_the_message_date() {
        assert!(hits("on:2026-09-12"));
        assert!(!hits("on:2026-09-11"));
        assert!(hits("since:2026-09-12"));
        assert!(hits("before:2026-09-13"));
        assert!(!hits("before:2026-09-12"));
    }

    #[test]
    fn only_body_and_text_need_the_server() {
        assert!(!Query::parse("subject:x from:y").unwrap().needs_the_server());
        assert!(Query::parse("body:contract").unwrap().needs_the_server());
        assert!(Query::parse("subject:x OR text:y").unwrap().needs_the_server());
    }
}
