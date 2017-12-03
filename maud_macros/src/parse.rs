use proc_macro::{
    Delimiter,
    Literal,
    Spacing,
    Span,
    TokenNode,
    TokenStream,
    TokenTree,
    TokenTreeIter,
};
use std::mem;
use std::iter::FromIterator;

use literalext::LiteralExt;

use ast;
use ParseResult;

#[derive(Copy, Clone)]
pub enum BufferBorrow {
    NeedBorrow,
    AlreadyBorrowed
}

#[derive(Copy, Clone)]
pub enum BufferType {
    Allocated,
    Custom(BufferBorrow)
}


#[derive(Clone)]
pub struct OutputBuffer {
    ident: TokenTree,
    buffer_type: BufferType
}

impl OutputBuffer {
    pub fn new(ident: TokenTree, buffer_type: BufferType) -> OutputBuffer {
        OutputBuffer { ident, buffer_type }
    }

    pub fn ident(&self) -> TokenTree {
        self.ident.clone()
    }

    pub fn buffer_type(&self) -> BufferType {
        self.buffer_type
    }
}

pub fn parse(input: TokenStream) -> ParseResult<Vec<ast::Markup>> {
    Parser::new(input).markups()
}

#[derive(Clone)]
struct Parser {
    /// Indicates whether we're inside an attribute node.
    in_attr: bool,
    input: TokenTreeIter,
}

impl Iterator for Parser {
    type Item = TokenTree;

    fn next(&mut self) -> Option<TokenTree> {
        self.input.next()
    }
}

impl Parser {
    fn new(input: TokenStream) -> Parser {
        Parser {
            in_attr: false,
            input: input.into_iter(),
        }
    }

    fn with_input(&self, input: TokenStream) -> Parser {
        Parser {
            in_attr: self.in_attr,
            input: input.into_iter(),
        }
    }

    /// Returns the next token in the stream without consuming it.
    fn peek(&self) -> Option<TokenTree> {
        self.clone().next()
    }

    /// Returns the next two tokens in the stream without consuming them.
    fn peek2(&self) -> Option<(TokenTree, Option<TokenTree>)> {
        let mut clone = self.clone();
        clone.next().map(|first| (first, clone.next()))
    }

    /// Advances the cursor by one step.
    fn advance(&mut self) {
        self.next();
    }

    /// Advances the cursor by two steps.
    fn advance2(&mut self) {
        self.next();
        self.next();
    }

    /// Overwrites the current parser state with the given parameter.
    fn commit(&mut self, attempt: Parser) {
        *self = attempt;
    }

    /// Returns an `Err` with the given message.
    fn error<T, E: Into<String>>(&self, message: E) -> ParseResult<T> {
        Err(message.into())
    }

    /// Parses and renders multiple blocks of markup.
    fn markups(&mut self) -> ParseResult<Vec<ast::Markup>> {
        let mut result = Vec::new();
        loop {
            match self.peek2() {
                None => break,
                Some((TokenTree { kind: TokenNode::Op(';', _), .. }, _)) => self.advance(),
                Some((
                    TokenTree { kind: TokenNode::Op('@', _), .. },
                    Some(TokenTree { kind: TokenNode::Term(term), span }),
                )) if term.as_str() == "let" => {
                    self.advance2();
                    let keyword = TokenTree { kind: TokenNode::Term(term), span };
                    result.push(self.let_expr(keyword)?);
                },
                _ => result.push(self.markup()?),
            }
        }
        Ok(result)
    }

    /// Parses and renders a single block of markup.
    fn markup(&mut self) -> ParseResult<ast::Markup> {
        let token = match self.peek() {
            Some(token) => token,
            None => return self.error("unexpected end of input"),
        };
        let markup = match token {
            // Literal
            TokenTree { kind: TokenNode::Literal(lit), span } => {
                self.advance();
                self.literal(lit, span)?
            },
            // Special form
            TokenTree { kind: TokenNode::Op('@', _), .. } => {
                self.advance();
                match self.next() {
                    Some(TokenTree { kind: TokenNode::Term(term), span }) => {
                        let keyword = TokenTree { kind: TokenNode::Term(term), span };
                        match term.as_str() {
                            "if" => {
                                let mut segments = Vec::new();
                                self.if_expr(vec![keyword], &mut segments)?;
                                ast::Markup::If { segments }
                            },
                            "while" => self.while_expr(keyword)?,
                            "for" => self.for_expr(keyword)?,
                            "match" => self.match_expr(keyword)?,
                            "let" => return self.error(format!("@let only works inside a block")),
                            other => return self.error(format!("unknown keyword `@{}`", other)),
                        }
                    },
                    _ => return self.error("expected keyword after `@`"),
                }
            }
            // Element
            TokenTree { kind: TokenNode::Term(_), .. } => {
                let name = self.namespaced_name()?;
                self.element(name)?
            },
            // Splice
            TokenTree { kind: TokenNode::Group(Delimiter::Parenthesis, expr), .. } => {
                self.advance();
                ast::Markup::Splice { expr }
            }
            // Block
            TokenTree { kind: TokenNode::Group(Delimiter::Brace, body), span } => {
                self.advance();
                ast::Markup::Block(self.block(body, span)?)
            },
            // ???
            _ => return self.error("invalid syntax"),
        };
        Ok(markup)
    }

    /// Parses and renders a literal string.
    fn literal(&mut self, lit: Literal, span: Span) -> ParseResult<ast::Markup> {
        if let Some(s) = lit.parse_string() {
            Ok(ast::Markup::Literal {
                content: s.to_string(),
                span,
            })
        } else {
            self.error("expected string")
        }
    }

    /// Parses an `@if` expression.
    ///
    /// The leading `@if` should already be consumed.
    fn if_expr(
        &mut self,
        prefix: Vec<TokenTree>,
        segments: &mut Vec<ast::Special>,
    ) -> ParseResult<()> {
        let mut head = prefix;
        let body = loop {
            match self.next() {
                Some(TokenTree { kind: TokenNode::Group(Delimiter::Brace, body), span }) => {
                    break self.block(body, span)?;
                },
                Some(token) => head.push(token),
                None => return self.error("unexpected end of @if expression"),
            }
        };
        segments.push(ast::Special { head: head.into_iter().collect(), body });
        self.else_if_expr(segments)
    }

    /// Parses an optional `@else if` or `@else`.
    ///
    /// The leading `@else if` or `@else` should *not* already be consumed.
    fn else_if_expr(&mut self, segments: &mut Vec<ast::Special>) -> ParseResult<()> {
        match self.peek2() {
            // Try to match an `@else` after this
            Some((
                TokenTree { kind: TokenNode::Op('@', _), .. },
                Some(TokenTree { kind: TokenNode::Term(else_keyword), span }),
            )) if else_keyword.as_str() == "else" => {
                self.advance2();
                let else_keyword = TokenTree { kind: TokenNode::Term(else_keyword), span };
                match self.peek() {
                    // `@else if`
                    Some(TokenTree { kind: TokenNode::Term(if_keyword), span })
                    if if_keyword.as_str() == "if" => {
                        self.advance();
                        let if_keyword = TokenTree { kind: TokenNode::Term(if_keyword), span };
                        self.if_expr(vec![else_keyword, if_keyword], segments)
                    },
                    // Just an `@else`
                    _ => {
                        if let Some(TokenTree { kind: TokenNode::Group(Delimiter::Brace, block), span }) = self.next() {
                            let body = self.block(block, span)?;
                            segments.push(ast::Special {
                                head: vec![else_keyword].into_iter().collect(),
                                body,
                            });
                            Ok(())
                        } else {
                            self.error("expected body for @else")
                        }
                    },
                }
            },
            // We didn't find an `@else`; stop
            _ => Ok(()),
        }
    }

    /// Parses and renders an `@while` expression.
    ///
    /// The leading `@while` should already be consumed.
    fn while_expr(&mut self, keyword: TokenTree) -> ParseResult<ast::Markup> {
        let mut head = vec![keyword];
        let body = loop {
            match self.next() {
                Some(TokenTree { kind: TokenNode::Group(Delimiter::Brace, body), span }) => {
                    break self.block(body, span)?;
                },
                Some(token) => head.push(token),
                None => return self.error("unexpected end of @while expression"),
            }
        };
        Ok(ast::Markup::Special(ast::Special { head: head.into_iter().collect(), body }))
    }

    /// Parses a `@for` expression.
    ///
    /// The leading `@for` should already be consumed.
    fn for_expr(&mut self, keyword: TokenTree) -> ParseResult<ast::Markup> {
        let mut head = vec![keyword];
        loop {
            match self.next() {
                Some(TokenTree { kind: TokenNode::Term(in_keyword), span }) if in_keyword.as_str() == "in" => {
                    head.push(TokenTree { kind: TokenNode::Term(in_keyword), span });
                    break;
                },
                Some(token) => head.push(token),
                None => return self.error("unexpected end of @for expression"),
            }
        }
        let body = loop {
            match self.next() {
                Some(TokenTree { kind: TokenNode::Group(Delimiter::Brace, body), span }) => {
                    break self.block(body, span)?;
                },
                Some(token) => head.push(token),
                None => return self.error("unexpected end of @for expression"),
            }
        };
        Ok(ast::Markup::Special(ast::Special { head: head.into_iter().collect(), body }))
    }

    /// Parses a `@match` expression.
    ///
    /// The leading `@match` should already be consumed.
    fn match_expr(&mut self, keyword: TokenTree) -> ParseResult<ast::Markup> {
        let mut head = vec![keyword];
        let (arms, arms_span) = loop {
            match self.next() {
                Some(TokenTree { kind: TokenNode::Group(Delimiter::Brace, body), span }) => {
                    break (self.with_input(body).match_arms()?, span);
                },
                Some(token) => head.push(token),
                None => return self.error("unexpected end of @match expression"),
            }
        };
        Ok(ast::Markup::Match { head: head.into_iter().collect(), arms, arms_span })
    }

    fn match_arms(&mut self) -> ParseResult<Vec<ast::Special>> {
        let mut arms = Vec::new();
        while let Some(arm) = self.match_arm()? {
            arms.push(arm);
        }
        Ok(arms)
    }

    fn match_arm(&mut self) -> ParseResult<Option<ast::Special>> {
        let mut head = Vec::new();
        loop {
            match self.peek2() {
                Some((
                    eq @ TokenTree { kind: TokenNode::Op('=', Spacing::Joint), .. },
                    Some(gt @ TokenTree { kind: TokenNode::Op('>', _), .. }),
                )) => {
                    self.advance2();
                    head.push(eq);
                    head.push(gt);
                    break;
                },
                Some((token, _)) => {
                    self.advance();
                    head.push(token);
                },
                None =>
                    if head.is_empty() {
                        return Ok(None);
                    } else {
                        return self.error("unexpected end of @match pattern");
                    },
            }
        }
        let body = match self.next() {
            // $pat => { $stmts }
            Some(TokenTree { kind: TokenNode::Group(Delimiter::Brace, body), span }) => {
                let body = self.block(body, span)?;
                // Trailing commas are optional if the match arm is a braced block
                if let Some(TokenTree { kind: TokenNode::Op(',', _), .. }) = self.peek() {
                    self.advance();
                }
                body
            },
            // $pat => $expr
            Some(first_token) => {
                let mut span = first_token.span;
                let mut body = vec![first_token];
                loop {
                    match self.next() {
                        Some(TokenTree { kind: TokenNode::Op(',', _), .. }) => break,
                        Some(token) => {
                            if let Some(bigger_span) = span.join(token.span) {
                                span = bigger_span;
                            }
                            body.push(token);
                        },
                        None => return self.error("unexpected end of @match arm"),
                    }
                }
                self.block(body.into_iter().collect(), span)?
            },
            None => return self.error("unexpected end of @match arm"),
        };
        Ok(Some(ast::Special { head: head.into_iter().collect(), body }))
    }

    /// Parses a `@let` expression.
    ///
    /// The leading `@let` should already be consumed.
    fn let_expr(&mut self, keyword: TokenTree) -> ParseResult<ast::Markup> {
        let mut tokens = vec![keyword];
        loop {
            match self.next() {
                Some(token @ TokenTree { kind: TokenNode::Op('=', _), .. }) => {
                    tokens.push(token);
                    break;
                },
                Some(token) => tokens.push(token),
                None => return self.error("unexpected end of @let expression"),
            }
        }
        loop {
            match self.next() {
                Some(token @ TokenTree { kind: TokenNode::Op(';', _), .. }) => {
                    tokens.push(token);
                    break;
                },
                Some(token) => tokens.push(token),
                None => return self.error("unexpected end of @let expression"),
            }
        }
        Ok(ast::Markup::Let { tokens: tokens.into_iter().collect() })
    }

    /// Parses an element node.
    ///
    /// The element name should already be consumed.
    fn element(&mut self, name: TokenStream) -> ParseResult<ast::Markup> {
        if self.in_attr {
            return self.error("unexpected element, you silly bumpkin");
        }
        let attrs = self.attrs()?;
        let body = match self.peek() {
            Some(TokenTree { kind: TokenNode::Op(';', _), .. }) |
            Some(TokenTree { kind: TokenNode::Op('/', _), .. }) => {
                // Void element
                self.advance();
                None
            },
            _ => Some(Box::new(self.markup()?)),
        };
        Ok(ast::Markup::Element { name, attrs, body })
    }

    /// Parses the attributes of an element.
    fn attrs(&mut self) -> ParseResult<ast::Attrs> {
        let mut classes_static = Vec::new();
        let mut classes_toggled = Vec::new();
        let mut ids = Vec::new();
        let mut attrs = Vec::new();
        loop {
            let mut attempt = self.clone();
            let maybe_name = attempt.namespaced_name();
            let token_after = attempt.next();
            match (maybe_name, token_after) {
                // Non-empty attribute
                (Ok(name), Some(TokenTree { kind: TokenNode::Op('=', _), .. })) => {
                    self.commit(attempt);
                    let value;
                    {
                        // Parse a value under an attribute context
                        let in_attr = mem::replace(&mut self.in_attr, true);
                        value = self.markup()?;
                        self.in_attr = in_attr;
                    }
                    attrs.push(ast::Attribute {
                        name,
                        attr_type: ast::AttrType::Normal { value },
                    });
                },
                // Empty attribute
                (Ok(name), Some(TokenTree { kind: TokenNode::Op('?', _), .. })) => {
                    self.commit(attempt);
                    let toggler = self.attr_toggler();
                    attrs.push(ast::Attribute {
                        name,
                        attr_type: ast::AttrType::Empty { toggler },
                    });
                },
                // Class shorthand
                (Err(_), Some(TokenTree { kind: TokenNode::Op('.', _), .. })) => {
                    self.commit(attempt);
                    let name = self.name()?;
                    if let Some(toggler) = self.attr_toggler() {
                        classes_toggled.push((name, toggler));
                    } else {
                        classes_static.push(name);
                    }
                },
                // ID shorthand
                (Err(_), Some(TokenTree { kind: TokenNode::Op('#', _), .. })) => {
                    self.commit(attempt);
                    ids.push(self.name()?);
                },
                // If it's not a valid attribute, backtrack and bail out
                _ => break,
            }
        }
        Ok(ast::Attrs { classes_static, classes_toggled, ids, attrs })
    }

    /// Parses the `[cond]` syntax after an empty attribute or class shorthand.
    fn attr_toggler(&mut self) -> Option<ast::Toggler> {
        if let Some(TokenTree {
            kind: TokenNode::Group(Delimiter::Bracket, cond),
            span: cond_span,
        }) = self.peek() {
            self.advance();
            Some(ast::Toggler { cond, cond_span })
        } else {
            None
        }
    }

    /// Parses an identifier, without dealing with namespaces.
    fn name(&mut self) -> ParseResult<TokenStream> {
        let mut result = Vec::new();
        if let Some(token @ TokenTree { kind: TokenNode::Term(_), .. }) = self.peek() {
            self.advance();
            result.push(token);
        } else {
            return self.error("expected identifier");
        }
        let mut expect_ident = false;
        loop {
            expect_ident = match self.peek() {
                Some(token @ TokenTree { kind: TokenNode::Op('-', _), .. }) => {
                    self.advance();
                    result.push(token);
                    true
                },
                Some(TokenTree { kind: TokenNode::Term(term), span }) if expect_ident => {
                    let token = TokenTree { kind: TokenNode::Term(term), span };
                    self.advance();
                    result.push(token);
                    false
                },
                _ => break,
            };
        }
        Ok(result.into_iter().collect())
    }

    /// Parses a HTML element or attribute name, along with a namespace
    /// if necessary.
    fn namespaced_name(&mut self) -> ParseResult<TokenStream> {
        let mut result = vec![self.name()?];
        if let Some(token @ TokenTree { kind: TokenNode::Op(':', _), .. }) = self.peek() {
            self.advance();
            result.push(TokenStream::from(token));
            result.push(self.name()?);
        }
        Ok(result.into_iter().collect())
    }

    /// Parses the given token stream as a Maud expression.
    fn block(&mut self, body: TokenStream, span: Span) -> ParseResult<ast::Block> {
        let markups = self.with_input(body).markups()?;
        Ok(ast::Block { markups, span })
    }
}

pub fn buffer_argument(input_stream: &mut TokenStream) -> ParseResult<OutputBuffer> {
    let mut input = input_stream.clone().into_iter();
    match peek3(&input) {
        // Case html_to! { my_buffer, <Markup> }
        Some((TokenTree { kind: TokenNode::Term(buffer), span },
              Some(TokenTree { kind: TokenNode::Op(',', _), .. }),
              _)) => {
            // Advance over argument
            advance2(&mut input);
            input_stream.clone_from(&TokenStream::from_iter(input));
            Ok(OutputBuffer {
                ident: TokenTree { kind: TokenNode::Term(buffer.clone()), span: span.clone() },
                buffer_type: BufferType::Custom(BufferBorrow::AlreadyBorrowed)
            })
        },
        // Case html_to! { &mut my_buffer, <Markup> }
        Some((TokenTree { kind: TokenNode::Op('&', _), .. },
              Some(TokenTree { kind: TokenNode::Term(mutable), .. }),
              Some(TokenTree { kind: TokenNode::Term(buffer),  span })))
            if mutable.as_str() == "mut" => {
                // Advance over argument
                advance4(&mut input);
                input_stream.clone_from(&TokenStream::from_iter(input));
                Ok(OutputBuffer {
                    ident: TokenTree { kind: TokenNode::Term(buffer.clone()), span: span.clone() },
                    buffer_type: BufferType::Custom(BufferBorrow::NeedBorrow)
                })
            },
        _ => { return Err("Error trying to parse the buffer name for html_to!".into()); }
    }
}

/// Returns the next three tokens in the stream without consuming them.
fn peek3(input: &TokenTreeIter) -> Option<(TokenTree, Option<TokenTree>, Option<TokenTree>)> {
    let mut clone = input.clone();
    clone.next().map(|first| {
        let second = clone.next();
        (first, second, clone.next())
    })
}

/// Advances the cursor by two steps.
fn advance2(input: &mut TokenTreeIter) {
    input.next();
    input.next();
}

/// Advances the cursor by four steps.
fn advance4(input: &mut TokenTreeIter) {
    advance2(input);
    advance2(input);
}

